use cool_thing::{LexResult, TextParsingMode, Tokenizer};

#[derive(Clone, Copy, Deserialize, Debug)]
pub enum InitialState {
    #[serde(rename = "Data state")]
    Data,
    #[serde(rename = "PLAINTEXT state")]
    PlainText,
    #[serde(rename = "RCDATA state")]
    RCData,
    #[serde(rename = "RAWTEXT state")]
    RawText,
    #[serde(rename = "Script data state")]
    ScriptData,
    #[serde(rename = "CDATA section state")]
    CDataSection,
}

impl InitialState {
    pub fn to_tokenizer_state<'t, TokenHandler: FnMut(LexResult)>(
        &self,
    ) -> fn(&mut Tokenizer<'t, TokenHandler>, Option<u8>) {
        match self {
            InitialState::Data => Tokenizer::data_state,
            InitialState::PlainText => Tokenizer::plaintext_state,
            InitialState::RCData => Tokenizer::rcdata_state,
            InitialState::RawText => Tokenizer::rawtext_state,
            InitialState::ScriptData => Tokenizer::script_data_state,
            InitialState::CDataSection => Tokenizer::cdata_section_state,
        }
    }

    pub fn should_replace_unsafe_null_in_text(&self) -> bool {
        match self {
            InitialState::Data | InitialState::CDataSection => false,
            _ => true,
        }
    }

    pub fn allows_text_entitites(&self) -> bool {
        match self {
            InitialState::Data | InitialState::RCData => true,
            _ => false,
        }
    }
}

impl From<TextParsingMode> for InitialState {
    fn from(mode: TextParsingMode) -> Self {
        match mode {
            TextParsingMode::Data => InitialState::Data,
            TextParsingMode::PlainText => InitialState::PlainText,
            TextParsingMode::RCData => InitialState::RCData,
            TextParsingMode::RawText => InitialState::RawText,
            TextParsingMode::ScriptData => InitialState::ScriptData,
            TextParsingMode::CDataSection => InitialState::CDataSection,
        }
    }
}
