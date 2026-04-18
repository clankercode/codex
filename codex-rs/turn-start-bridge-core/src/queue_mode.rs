#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueMode {
    Default,
    Immediate,
    AfterToolCall,
    AfterAnyItem,
    NextTurn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultQueueMode {
    AfterToolCall,
    AfterAnyItem,
    NextTurn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseQueueModeError {
    pub value: String,
}

impl QueueMode {
    pub fn parse(value: &str) -> Result<Self, ParseQueueModeError> {
        match value {
            "default" | "Default" => Ok(Self::Default),
            "immediate" | "Immediate" => Ok(Self::Immediate),
            "after-tool-call" | "AfterToolCall" => Ok(Self::AfterToolCall),
            "after-any-item" | "AfterAnyItem" => Ok(Self::AfterAnyItem),
            "next-turn" | "NextTurn" => Ok(Self::NextTurn),
            _ => Err(ParseQueueModeError {
                value: value.to_string(),
            }),
        }
    }

    pub fn resolve_default(self, default_mode: DefaultQueueMode) -> Self {
        match self {
            Self::Default => match default_mode {
                DefaultQueueMode::AfterToolCall => Self::AfterToolCall,
                DefaultQueueMode::AfterAnyItem => Self::AfterAnyItem,
                DefaultQueueMode::NextTurn => Self::NextTurn,
            },
            mode => mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_accepts_canonical_design_spellings() {
        assert_eq!(QueueMode::parse("Default"), Ok(QueueMode::Default));
        assert_eq!(QueueMode::parse("Immediate"), Ok(QueueMode::Immediate));
        assert_eq!(
            QueueMode::parse("AfterToolCall"),
            Ok(QueueMode::AfterToolCall)
        );
        assert_eq!(
            QueueMode::parse("AfterAnyItem"),
            Ok(QueueMode::AfterAnyItem)
        );
        assert_eq!(QueueMode::parse("NextTurn"), Ok(QueueMode::NextTurn));
    }

    #[test]
    fn default_mode_resolves_to_after_tool_call() {
        assert_eq!(
            QueueMode::Default.resolve_default(DefaultQueueMode::AfterToolCall),
            QueueMode::AfterToolCall
        );
    }
}
