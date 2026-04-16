use crate::QueueMode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedMessage {
    pub queue_mode: QueueMode,
    pub text: String,
}

pub fn parse_prefixed_message(input: &str) -> ParsedMessage {
    let Some(rest) = input.strip_prefix("CODEX_QUEUE_MODE=") else {
        return ParsedMessage {
            queue_mode: QueueMode::Default,
            text: input.to_string(),
        };
    };

    let Some((mode, text)) = rest.split_once(char::is_whitespace) else {
        return ParsedMessage {
            queue_mode: QueueMode::parse(rest).unwrap_or(QueueMode::Default),
            text: String::new(),
        };
    };

    ParsedMessage {
        queue_mode: QueueMode::parse(mode).unwrap_or(QueueMode::Default),
        text: text.trim_start().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_canonical_queue_mode_prefix_and_strips_it() {
        assert_eq!(
            parse_prefixed_message("CODEX_QUEUE_MODE=AfterToolCall summarize the diff"),
            ParsedMessage {
                queue_mode: QueueMode::AfterToolCall,
                text: "summarize the diff".to_string(),
            }
        );
    }

    #[test]
    fn falls_back_to_default_for_unknown_prefix_mode() {
        assert_eq!(
            parse_prefixed_message("CODEX_QUEUE_MODE=Later summarize the diff"),
            ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "summarize the diff".to_string(),
            }
        );
    }
}
