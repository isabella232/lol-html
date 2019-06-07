use super::*;
use crate::base::{Chunk, Range};
use crate::html::{LocalName, Namespace};
use crate::parser::{
    Lexeme, LexemeSink, NonTagContentLexeme, ParserDirective, ParserOutputSink, TagHintSink,
    TagLexeme, TagTokenOutline,
};
use crate::rewritable_units::{
    Serialize, ToToken, Token, TokenCaptureFlags, TokenCapturer, TokenCapturerEvent,
};
use encoding_rs::Encoding;
use std::rc::Rc;

use TagTokenOutline::*;

pub struct AuxStartTagInfo<'i> {
    pub input: &'i Chunk<'i>,
    pub attr_buffer: SharedAttributeBuffer,
    pub self_closing: bool,
}

type AuxStartTagInfoRequest<C> = Box<dyn FnOnce(&mut C, AuxStartTagInfo<'_>) -> TokenCaptureFlags>;
pub type StartTagHandlingResult<C> = Result<TokenCaptureFlags, AuxStartTagInfoRequest<C>>;

pub trait TransformController: Sized {
    fn initial_capture_flags(&self) -> TokenCaptureFlags;
    fn handle_start_tag(&mut self, name: LocalName, ns: Namespace) -> StartTagHandlingResult<Self>;
    fn handle_end_tag(&mut self, name: LocalName) -> TokenCaptureFlags;
    fn handle_token(&mut self, token: &mut Token);
    fn should_emit_content(&self) -> bool;
}

pub trait OutputSink {
    fn handle_chunk(&mut self, chunk: &[u8]);
}

impl<F: FnMut(&[u8])> OutputSink for F {
    fn handle_chunk(&mut self, chunk: &[u8]) {
        self(chunk);
    }
}

pub struct Dispatcher<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    transform_controller: C,
    output_sink: O,
    remaining_content_start: usize,
    token_capturer: TokenCapturer,
    got_flags_from_hint: bool,
    pending_element_aux_info_req: Option<AuxStartTagInfoRequest<C>>,
    emission_enabled: bool,
}

impl<C, O> Dispatcher<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    pub fn new(transform_controller: C, output_sink: O, encoding: &'static Encoding) -> Self {
        let initial_capture_flags = transform_controller.initial_capture_flags();

        Dispatcher {
            transform_controller,
            output_sink,
            remaining_content_start: 0,
            token_capturer: TokenCapturer::new(initial_capture_flags, encoding),
            got_flags_from_hint: false,
            pending_element_aux_info_req: None,
            emission_enabled: true,
        }
    }

    pub fn flush_remaining_input(&mut self, input: &Chunk, blocked_byte_count: usize) {
        let output = input.slice(Range {
            start: self.remaining_content_start,
            end: input.len() - blocked_byte_count,
        });

        if self.emission_enabled && !output.is_empty() {
            self.output_sink.handle_chunk(&output);
        }

        self.remaining_content_start = 0;
    }

    pub fn finish(&mut self, input: &Chunk) {
        self.flush_remaining_input(input, 0);

        // NOTE: output the finalizing chunk.
        self.output_sink.handle_chunk(&[]);
    }

    fn try_produce_token_from_lexeme<'i, T>(&mut self, lexeme: &Lexeme<'i, T>)
    where
        Lexeme<'i, T>: ToToken,
    {
        let transform_controller = &mut self.transform_controller;
        let output_sink = &mut self.output_sink;
        let emission_enabled = self.emission_enabled;
        let lexeme_range = lexeme.raw_range();
        let remaining_content_start = self.remaining_content_start;
        let mut lexeme_consumed = false;

        self.token_capturer.feed(lexeme, |event| match event {
            TokenCapturerEvent::LexemeConsumed => {
                let chunk = lexeme.input().slice(Range {
                    start: remaining_content_start,
                    end: lexeme_range.start,
                });

                lexeme_consumed = true;

                if emission_enabled && chunk.len() > 0 {
                    output_sink.handle_chunk(&chunk);
                }
            }
            TokenCapturerEvent::TokenProduced(mut token) => {
                trace!(@output token);

                transform_controller.handle_token(&mut token);

                if emission_enabled {
                    token.to_bytes(&mut |c| output_sink.handle_chunk(c));
                }
            }
        });

        if lexeme_consumed {
            self.remaining_content_start = lexeme_range.end;
        }
    }

    #[inline]
    fn get_next_parser_directive(&self) -> ParserDirective {
        if self.token_capturer.has_captures() {
            ParserDirective::Lex
        } else {
            ParserDirective::WherePossibleScanForTagsOnly
        }
    }

    fn adjust_capture_flags_for_tag_lexeme(&mut self, lexeme: &TagLexeme) {
        let input = lexeme.input();

        macro_rules! get_flags_from_aux_info_res {
            ($handler:expr, $attributes:expr, $self_closing:expr) => {
                $handler(
                    &mut self.transform_controller,
                    AuxStartTagInfo {
                        input,
                        attr_buffer: Rc::clone($attributes),
                        self_closing: $self_closing,
                    },
                )
            };
        }

        let capture_flags = match self.pending_element_aux_info_req.take() {
            // NOTE: tag hint was produced for the tag, but
            // attributes and self closing flag were requested.
            Some(aux_info_req) => match *lexeme.token_outline() {
                StartTag {
                    ref attributes,
                    self_closing,
                    ..
                } => get_flags_from_aux_info_res!(aux_info_req, attributes, self_closing),
                _ => unreachable!("Tag should be a start tag at this point"),
            },

            // NOTE: tag hint hasn't been produced for the tag, because
            // parser is not in the tag scan mode.
            None => match *lexeme.token_outline() {
                StartTag {
                    name,
                    name_hash,
                    ns,
                    ref attributes,
                    self_closing,
                } => {
                    let name = LocalName::new(input, name, name_hash);

                    match self.transform_controller.handle_start_tag(name, ns) {
                        Ok(flags) => flags,
                        Err(aux_info_req) => {
                            get_flags_from_aux_info_res!(aux_info_req, attributes, self_closing)
                        }
                    }
                }

                EndTag { name, name_hash } => {
                    let name = LocalName::new(input, name, name_hash);

                    self.transform_controller.handle_end_tag(name)
                }
            },
        };

        self.token_capturer.set_capture_flags(capture_flags);
    }

    #[inline]
    fn apply_capture_flags_from_hint_and_get_next_parser_directive(
        &mut self,
        flags: TokenCaptureFlags,
    ) -> ParserDirective {
        self.token_capturer.set_capture_flags(flags);
        self.got_flags_from_hint = true;
        self.get_next_parser_directive()
    }

    #[inline]
    fn flush_pending_captured_text(&mut self) {
        let transform_controller = &mut self.transform_controller;
        let output_sink = &mut self.output_sink;
        let emission_enabled = self.emission_enabled;

        self.token_capturer.flush_pending_text(&mut |event| {
            if let TokenCapturerEvent::TokenProduced(mut token) = event {
                trace!(@output token);

                transform_controller.handle_token(&mut token);

                if emission_enabled {
                    token.to_bytes(&mut |c| output_sink.handle_chunk(c));
                }
            }
        });
    }

    #[inline]
    fn should_stop_removing_element_content(&self) -> bool {
        !self.emission_enabled && self.transform_controller.should_emit_content()
    }
}

impl<C, O> LexemeSink for Dispatcher<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    fn handle_tag(&mut self, lexeme: &TagLexeme) -> ParserDirective {
        // NOTE: flush pending text before reporting tag to the transform controller.
        // Otherwise, transform controller can enable or disable text handlers too early.
        // In case of start tag, newly matched element text handlers
        // will receive leftovers from the previous match. And, in case of end tag,
        // handlers will be disabled before the receive the finalizing chunk.
        self.flush_pending_captured_text();

        if self.got_flags_from_hint {
            self.got_flags_from_hint = false;
        } else {
            self.adjust_capture_flags_for_tag_lexeme(lexeme);
        }

        if let TagTokenOutline::EndTag { .. } = lexeme.token_outline() {
            if self.should_stop_removing_element_content() {
                self.emission_enabled = true;
                self.remaining_content_start = lexeme.raw_range().start;
            }
        }

        self.try_produce_token_from_lexeme(lexeme);
        self.emission_enabled = self.transform_controller.should_emit_content();

        self.get_next_parser_directive()
    }

    #[inline]
    fn handle_non_tag_content(&mut self, lexeme: &NonTagContentLexeme) {
        self.try_produce_token_from_lexeme(lexeme);
    }
}

impl<C, O> TagHintSink for Dispatcher<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    fn handle_start_tag_hint(&mut self, name: LocalName, ns: Namespace) -> ParserDirective {
        match self.transform_controller.handle_start_tag(name, ns) {
            Ok(flags) => self.apply_capture_flags_from_hint_and_get_next_parser_directive(flags),
            Err(aux_info_req) => {
                self.got_flags_from_hint = false;
                self.pending_element_aux_info_req = Some(aux_info_req);

                ParserDirective::Lex
            }
        }
    }

    fn handle_end_tag_hint(&mut self, name: LocalName) -> ParserDirective {
        self.flush_pending_captured_text();

        let mut flags = self.transform_controller.handle_end_tag(name);

        // NOTE: if emission was disabled (i.e. we've been removing element content)
        // we need to request the end tag lexeme, to ensure that we have it.
        // Otherwise, if we have unfinished end tag in the end of input we'll emit
        // it where we shouldn't.
        if self.should_stop_removing_element_content() {
            flags |= TokenCaptureFlags::NEXT_END_TAG;
        }

        self.apply_capture_flags_from_hint_and_get_next_parser_directive(flags)
    }
}

impl<C, O> ParserOutputSink for Dispatcher<C, O>
where
    C: TransformController,
    O: OutputSink,
{
}
