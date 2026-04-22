use crate::QueueMode;
use quick_xml::Reader;
use quick_xml::errors::Error as QuickXmlError;
use quick_xml::errors::IllFormedError;
use quick_xml::errors::SyntaxError;
use quick_xml::events::Event;
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
struct PlainTextMessageXml {
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

        if !has_supported_root_prefix(&self.buffer) {
            return Err(XmlInputError::new(
                "expected system_prompt or message XML fragment",
            ));
        }

        let Some(fragment_end) = next_root_fragment_end(&self.buffer)? else {
            return Ok(None);
        };
        Ok(Some(self.buffer.drain(..fragment_end).collect()))
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

        let (message_type, queue, raw_inner_xml) = parse_message_fragment_metadata(fragment)?;
        if message_type != "user" {
            return Err(XmlInputError::new(format!(
                "unsupported message type `{message_type}`"
            )));
        }

        self.seen_message = true;
        let queue_mode = match queue {
            Some(queue) => QueueMode::parse(&queue)
                .map_err(|_| XmlInputError::new(format!("unknown queue mode `{queue}`")))?,
            None => QueueMode::Default,
        };
        let text = if contains_nested_xml_markup(raw_inner_xml) {
            raw_inner_xml.to_string()
        } else {
            let parsed: PlainTextMessageXml = quick_xml::de::from_str(fragment)
                .map_err(|err| XmlInputError::new(format!("failed to parse message: {err}")))?;
            parsed.text
        };

        Ok(ParsedXmlInput::Message(ParsedMessage { queue_mode, text }))
    }
}

fn next_root_fragment_end(buffer: &str) -> Result<Option<usize>, XmlInputError> {
    let mut reader = Reader::from_str(buffer);
    let mut root_depth = 0_u32;
    let mut root_name: Option<Vec<u8>> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                if root_depth == 0 {
                    let name = start.name().as_ref().to_vec();
                    if !matches!(name.as_slice(), b"system_prompt" | b"message") {
                        return Err(XmlInputError::new(
                            "expected system_prompt or message XML fragment",
                        ));
                    }
                    root_name = Some(name);
                }
                root_depth += 1;
            }
            Ok(Event::Empty(empty)) => {
                if root_depth == 0 {
                    let name = empty.name().as_ref().to_vec();
                    if !matches!(name.as_slice(), b"system_prompt" | b"message") {
                        return Err(XmlInputError::new(
                            "expected system_prompt or message XML fragment",
                        ));
                    }
                    return Ok(Some(reader.buffer_position() as usize));
                }
            }
            Ok(Event::End(end)) => {
                if root_depth == 0 {
                    return Err(XmlInputError::new("unexpected XML closing tag"));
                }
                root_depth -= 1;
                if root_depth == 0 {
                    let expected_root_name = root_name.as_deref().ok_or_else(|| {
                        XmlInputError::new("expected system_prompt or message XML fragment")
                    })?;
                    if end.name().as_ref() != expected_root_name {
                        return Err(XmlInputError::new(format!(
                            "mismatched XML closing tag `</{}>`",
                            String::from_utf8_lossy(end.name().as_ref())
                        )));
                    }
                    return Ok(Some(reader.buffer_position() as usize));
                }
            }
            Ok(Event::Eof) => return Ok(None),
            Ok(
                Event::Text(_)
                | Event::CData(_)
                | Event::Comment(_)
                | Event::Decl(_)
                | Event::PI(_)
                | Event::DocType(_)
                | Event::GeneralRef(_),
            ) => {
                if root_depth == 0 {
                    return Err(XmlInputError::new(
                        "expected system_prompt or message XML fragment",
                    ));
                }
            }
            Err(err) => {
                if is_incomplete_root_fragment_error(
                    &err,
                    reader.buffer_position() as usize,
                    buffer.len(),
                ) {
                    return Ok(None);
                }
                return Err(XmlInputError::new(format!(
                    "failed to parse XML input: {err}"
                )));
            }
        }
    }
}

fn has_supported_root_prefix(buffer: &str) -> bool {
    ["<system_prompt", "<message"]
        .into_iter()
        .any(|prefix| prefix.starts_with(buffer) || buffer.starts_with(prefix))
}

fn is_incomplete_root_fragment_error(
    error: &QuickXmlError,
    buffer_position: usize,
    buffer_len: usize,
) -> bool {
    if buffer_position < buffer_len {
        return false;
    }

    matches!(
        error,
        QuickXmlError::Syntax(
            SyntaxError::InvalidBangMarkup
                | SyntaxError::UnclosedPIOrXmlDecl
                | SyntaxError::UnclosedComment
                | SyntaxError::UnclosedDoctype
                | SyntaxError::UnclosedCData
                | SyntaxError::UnclosedTag
        ) | QuickXmlError::IllFormed(
            IllFormedError::MissingEndTag(_) | IllFormedError::UnclosedReference
        )
    )
}

fn parse_message_fragment_metadata(
    fragment: &str,
) -> Result<(String, Option<String>, &str), XmlInputError> {
    let start_tag_end = fragment
        .find('>')
        .ok_or_else(|| XmlInputError::new("failed to parse message: missing start tag"))?;
    let close_tag_start = fragment
        .rfind("</message>")
        .ok_or_else(|| XmlInputError::new("failed to parse message: missing closing tag"))?;
    let raw_inner_xml = &fragment[start_tag_end + 1..close_tag_start];
    let start_tag = &fragment[..=start_tag_end];

    let mut reader = Reader::from_str(start_tag);
    let mut message_type = None;
    let mut queue = None;

    match reader.read_event() {
        Ok(Event::Start(start)) => {
            for attribute in start.attributes() {
                let attribute = attribute.map_err(|err| {
                    XmlInputError::new(format!("failed to parse message attributes: {err}"))
                })?;
                let value = attribute
                    .decode_and_unescape_value(reader.decoder())
                    .map_err(|err| {
                        XmlInputError::new(format!("failed to parse message attributes: {err}"))
                    })?
                    .into_owned();
                match attribute.key.as_ref() {
                    b"type" => message_type = Some(value),
                    b"queue" => queue = Some(value),
                    _ => {}
                }
            }
        }
        Ok(_) => {
            return Err(XmlInputError::new(
                "failed to parse message: expected opening message tag",
            ));
        }
        Err(err) => {
            return Err(XmlInputError::new(format!(
                "failed to parse message attributes: {err}"
            )));
        }
    }

    let message_type = message_type
        .ok_or_else(|| XmlInputError::new("failed to parse message: missing type attribute"))?;
    Ok((message_type, queue, raw_inner_xml))
}

fn contains_nested_xml_markup(raw_inner_xml: &str) -> bool {
    let mut offset = 0;
    while offset < raw_inner_xml.len() {
        let rest = &raw_inner_xml[offset..];
        if rest.starts_with("<![CDATA[") {
            if let Some(end) = rest.find("]]>") {
                offset += end + "]]>".len();
                continue;
            }
            return false;
        }
        if rest.starts_with('<') && is_nested_xml_marker(rest) {
            return true;
        }
        let Some(character) = rest.chars().next() else {
            break;
        };
        offset += character.len_utf8();
    }
    false
}

fn is_nested_xml_marker(fragment: &str) -> bool {
    fragment
        .as_bytes()
        .get(1)
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'/' || *byte == b'_')
}

impl XmlInputError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl XmlInputParser {
    pub fn discard_malformed_prefix(&mut self) -> bool {
        let Some(next_root_start) = find_next_root_start(&self.buffer) else {
            self.buffer.clear();
            return false;
        };

        if next_root_start == 0 {
            return false;
        }

        self.buffer.drain(..next_root_start);
        true
    }
}

fn find_next_root_start(buffer: &str) -> Option<usize> {
    buffer
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index > 0 && has_supported_root_prefix(&buffer[*index..]))
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

    #[test]
    fn xml_parser_preserves_nested_inner_xml_markup() {
        let mut parser = XmlInputParser::default();

        assert_eq!(
            parser.push(
                "<message type=\"user\"><c2c event=\"message\" from=\"peer\" alias=\"peer\">hello</c2c></message>"
            ),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "<c2c event=\"message\" from=\"peer\" alias=\"peer\">hello</c2c>"
                    .to_string(),
            })])
        );
    }

    #[test]
    fn xml_parser_preserves_nested_inner_xml_with_nested_message_nodes() {
        let mut parser = XmlInputParser::default();

        assert_eq!(
            parser.push(
                "<message type=\"user\"><outer><message>nested literal</message></outer></message>"
            ),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "<outer><message>nested literal</message></outer>".to_string(),
            })])
        );
    }

    #[test]
    fn xml_parser_accepts_non_ascii_plain_text_message() {
        let mut parser = XmlInputParser::default();

        assert_eq!(
            parser.push("<message type=\"user\">café</message>"),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "café".to_string(),
            })])
        );
    }

    #[test]
    fn xml_parser_waits_for_partial_root_tag_name() {
        let mut parser = XmlInputParser::default();

        assert_eq!(parser.push("<mess"), Ok(Vec::new()));
        assert_eq!(
            parser.push("age type=\"user\">hello</message>"),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "hello".to_string(),
            })])
        );
    }

    #[test]
    fn xml_parser_waits_for_partial_attribute_value() {
        let mut parser = XmlInputParser::default();

        assert_eq!(
            parser.push("<message type=\"user\" queue=\"After"),
            Ok(Vec::new())
        );
        assert_eq!(
            parser.push("ToolCall\">hello</message>"),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::AfterToolCall,
                text: "hello".to_string(),
            })])
        );
    }

    #[test]
    fn xml_parser_waits_for_partial_nested_inner_xml() {
        let mut parser = XmlInputParser::default();

        assert_eq!(
            parser.push("<message type=\"user\"><outer><inner"),
            Ok(Vec::new())
        );
        assert_eq!(
            parser.push(">hello</inner></outer></message>"),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "<outer><inner>hello</inner></outer>".to_string(),
            })])
        );
    }

    #[test]
    fn discard_malformed_prefix_keeps_following_valid_fragment() {
        let mut parser = XmlInputParser::default();

        let err = parser
            .push("<message type=\"user\"><broken <message type=\"user\">ok</message>")
            .expect_err("malformed prefix should fail");
        assert!(err.to_string().contains("failed to parse XML input"));
        assert!(parser.discard_malformed_prefix());
        assert_eq!(
            parser.push(""),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "ok".to_string(),
            })])
        );
    }

    #[test]
    fn discard_malformed_prefix_keeps_partial_following_valid_fragment() {
        let mut parser = XmlInputParser {
            buffer: "<message type=\"user\"><broken <mess".to_string(),
            seen_message: false,
        };

        assert!(parser.discard_malformed_prefix());
        assert_eq!(
            parser.push("age type=\"user\">ok</message>"),
            Ok(vec![ParsedXmlInput::Message(ParsedMessage {
                queue_mode: QueueMode::Default,
                text: "ok".to_string(),
            })])
        );
    }
}
