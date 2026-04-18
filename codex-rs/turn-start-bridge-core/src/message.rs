use crate::QueueMode;
use serde::Deserialize;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedMessage {
    pub queue_mode: QueueMode,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedXmlInput {
    SystemPrompt(String),
    Message(ParsedMessage),
}

#[derive(Clone, Debug, Default)]
pub struct XmlInputParser {
    buffer: String,
    seen_message: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XmlInputError {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "system_prompt")]
struct SystemPromptXml {
    #[serde(rename = "$text")]
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "message")]
struct MessageXml {
    #[serde(rename = "@type")]
    message_type: String,
    #[serde(rename = "@queue")]
    queue: Option<String>,
    #[serde(rename = "$text")]
    text: String,
}

impl XmlInputParser {
    pub fn push(&mut self, input: &str) -> Result<Vec<ParsedXmlInput>, XmlInputError> {
        self.buffer.push_str(input);
        let mut parsed = Vec::new();

        while let Some(fragment) = self.next_fragment()? {
            parsed.push(self.parse_fragment(&fragment)?);
        }

        Ok(parsed)
    }

    pub fn finish(&self) -> Result<(), XmlInputError> {
        if self.buffer.trim().is_empty() {
            Ok(())
        } else {
            Err(XmlInputError::new("incomplete XML stdin fragment"))
        }
    }

    fn next_fragment(&mut self) -> Result<Option<String>, XmlInputError> {
        let trimmed_start = self.buffer.trim_start();
        let leading_whitespace = self.buffer.len() - trimmed_start.len();
        if leading_whitespace > 0 {
            self.buffer.drain(..leading_whitespace);
        }

        if self.buffer.is_empty() {
            return Ok(None);
        }

        let root_name = if self.buffer.starts_with("<system_prompt>") {
            "system_prompt"
        } else if self.buffer.starts_with("<message")
            && self
                .buffer
                .as_bytes()
                .get("<message".len())
                .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'>')
        {
            "message"
        } else {
            return Err(XmlInputError::new(
                "expected system_prompt or message XML fragment",
            ));
        };

        let close_tag = format!("</{root_name}>");
        let mut offset = 0;
        let mut in_cdata = false;
        let close_start = loop {
            if offset >= self.buffer.len() {
                break None;
            }

            let rest = &self.buffer[offset..];
            if in_cdata {
                if rest.starts_with("]]>") {
                    in_cdata = false;
                    offset += "]]>".len();
                } else {
                    offset += 1;
                }
            } else if rest.starts_with("<![CDATA[") {
                in_cdata = true;
                offset += "<![CDATA[".len();
            } else if rest.starts_with(&close_tag) {
                break Some(offset);
            } else {
                offset += 1;
            }
        };
        let Some(close_start) = close_start else {
            return Ok(None);
        };
        let close_end = close_start + close_tag.len();
        Ok(Some(self.buffer.drain(..close_end).collect()))
    }

    fn parse_fragment(&mut self, fragment: &str) -> Result<ParsedXmlInput, XmlInputError> {
        if fragment.starts_with("<system_prompt>") {
            if self.seen_message {
                return Err(XmlInputError::new(
                    "system_prompt must appear before the first message",
                ));
            }

            let parsed: SystemPromptXml = quick_xml::de::from_str(fragment).map_err(|err| {
                XmlInputError::new(format!("failed to parse system_prompt: {err}"))
            })?;
            return Ok(ParsedXmlInput::SystemPrompt(parsed.text));
        }

        let parsed: MessageXml = quick_xml::de::from_str(fragment)
            .map_err(|err| XmlInputError::new(format!("failed to parse message: {err}")))?;
        if parsed.message_type != "user" {
            return Err(XmlInputError::new(format!(
                "unsupported message type `{}`",
                parsed.message_type
            )));
        }

        self.seen_message = true;
        let queue_mode = match parsed.queue {
            Some(queue) => QueueMode::parse(&queue)
                .map_err(|_| XmlInputError::new(format!("unknown queue mode `{queue}`")))?,
            None => QueueMode::Default,
        };

        Ok(ParsedXmlInput::Message(ParsedMessage {
            queue_mode,
            text: parsed.text,
        }))
    }
}

impl XmlInputError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for XmlInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for XmlInputError {}

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

    #[test]
    fn xml_parser_reads_system_prompt_before_user_message() {
        let mut parser = XmlInputParser::default();

        assert_eq!(
            parser.push(
                "<system_prompt>be terse</system_prompt>\
                 <message type=\"user\" queue=\"AfterToolCall\">summarize &amp; test</message>"
            ),
            Ok(vec![
                ParsedXmlInput::SystemPrompt("be terse".to_string()),
                ParsedXmlInput::Message(ParsedMessage {
                    queue_mode: QueueMode::AfterToolCall,
                    text: "summarize & test".to_string(),
                }),
            ])
        );
    }

    #[test]
    fn xml_parser_rejects_system_prompt_after_message() {
        let mut parser = XmlInputParser::default();

        let err = parser
            .push(
                "<message type=\"user\">hello</message>\
                 <system_prompt>late</system_prompt>",
            )
            .expect_err("late system prompt should fail");

        assert_eq!(
            err.to_string(),
            "system_prompt must appear before the first message"
        );
    }

    #[test]
    fn xml_parser_rejects_unsupported_message_type() {
        let mut parser = XmlInputParser::default();

        let err = parser
            .push("<message type=\"assistant\">hello</message>")
            .expect_err("assistant messages are not supported yet");

        assert_eq!(err.to_string(), "unsupported message type `assistant`");
    }

    #[test]
    fn xml_parser_reports_incomplete_fragment_at_eof() {
        let mut parser = XmlInputParser::default();
        assert_eq!(parser.push("<message type=\"user\">hello"), Ok(Vec::new()));

        let err = parser.finish().expect_err("unfinished message should fail");

        assert_eq!(err.to_string(), "incomplete XML stdin fragment");
    }

    #[test]
    fn xml_parser_decodes_cdata_without_treating_inner_text_as_framing() {
        let mut parser = XmlInputParser::default();

        assert_eq!(
            parser
                .push("<message type=\"user\"><![CDATA[look at </message> literally]]></message>"),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "look at </message> literally".to_string(),
            })])
        );
    }
}
