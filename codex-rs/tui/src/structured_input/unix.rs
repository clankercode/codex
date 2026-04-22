use std::fs::File;
use std::os::fd::FromRawFd;

use codex_turn_start_bridge_core::XmlInputParser;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use super::StructuredInputReaderEvent;

pub(crate) fn spawn_xml_input_reader(
    xml_input_fd: i32,
) -> std::io::Result<mpsc::UnboundedReceiver<StructuredInputReaderEvent>> {
    if xml_input_fd < 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid --xml-input-fd value `{xml_input_fd}`"),
        ));
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let file = unsafe { File::from_raw_fd(xml_input_fd) };
    let mut file = tokio::fs::File::from_std(file);

    tokio::spawn(async move {
        let mut parser = XmlInputParser::default();
        let mut buf = [0_u8; 4096];
        loop {
            match file.read(&mut buf).await {
                Ok(0) => {
                    if let Err(err) = parser.finish() {
                        let _ = tx.send(StructuredInputReaderEvent::ParseError(err.to_string()));
                    }
                    let _ = tx.send(StructuredInputReaderEvent::Eof);
                    break;
                }
                Ok(bytes_read) => match std::str::from_utf8(&buf[..bytes_read]) {
                    Ok(input) => {
                        if let Err(message) = forward_parsed_input(&mut parser, &tx, input) {
                            let _ = tx.send(StructuredInputReaderEvent::ParseError(message));
                            while parser.discard_malformed_prefix() {
                                match forward_parsed_input(&mut parser, &tx, "") {
                                    Ok(()) => break,
                                    Err(message) => {
                                        let _ = tx
                                            .send(StructuredInputReaderEvent::ParseError(message));
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                            "Structured input is not valid UTF-8: {err}"
                        )));
                        break;
                    }
                },
                Err(err) => {
                    let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                        "Failed to read structured input: {err}"
                    )));
                    break;
                }
            }
        }
    });

    Ok(rx)
}

fn forward_parsed_input(
    parser: &mut XmlInputParser,
    tx: &mpsc::UnboundedSender<StructuredInputReaderEvent>,
    input: &str,
) -> Result<(), String> {
    let parsed_items = parser.push(input).map_err(|err| err.to_string())?;
    for item in parsed_items {
        let _ = tx.send(StructuredInputReaderEvent::Parsed(item));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_turn_start_bridge_core::ParsedMessage;
    use codex_turn_start_bridge_core::ParsedXmlInput;
    use codex_turn_start_bridge_core::QueueMode;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use std::os::fd::FromRawFd;

    #[tokio::test]
    async fn malformed_fragment_and_following_valid_fragment_are_recovered_within_one_read() {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_end = fds[0];
        let mut write_end = unsafe { File::from_raw_fd(fds[1]) };
        let mut rx = spawn_xml_input_reader(read_end).expect("reader should start");

        write!(
            write_end,
            "<message type=\"user\"><broken <message type=\"user\">ok</message>"
        )
        .expect("write should succeed");
        drop(write_end);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
            if matches!(events.last(), Some(StructuredInputReaderEvent::Eof)) {
                break;
            }
        }

        assert!(
            events
                .iter()
                .any(|event| matches!(event, StructuredInputReaderEvent::ParseError(_))),
            "expected parse error event, got {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                StructuredInputReaderEvent::Parsed(ParsedXmlInput::Message(ParsedMessage {
                    queue_mode: QueueMode::Default,
                    text,
                })) if text == "ok"
            )),
            "expected valid fragment after malformed prefix, got {events:?}"
        );
    }

    #[tokio::test]
    async fn malformed_fragment_and_partial_following_valid_fragment_are_recovered_across_reads() {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_end = fds[0];
        let mut write_end = unsafe { File::from_raw_fd(fds[1]) };
        let mut rx = spawn_xml_input_reader(read_end).expect("reader should start");

        write!(write_end, "<message type=\"user\"><broken <mess").expect("write should succeed");
        write!(write_end, "age type=\"user\">ok</message>").expect("write should succeed");
        drop(write_end);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
            if matches!(events.last(), Some(StructuredInputReaderEvent::Eof)) {
                break;
            }
        }

        assert!(
            events
                .iter()
                .any(|event| matches!(event, StructuredInputReaderEvent::ParseError(_))),
            "expected parse error event, got {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                StructuredInputReaderEvent::Parsed(ParsedXmlInput::Message(ParsedMessage {
                    queue_mode: QueueMode::Default,
                    text,
                })) if text == "ok"
            )),
            "expected valid fragment after malformed prefix split across reads, got {events:?}"
        );
    }
}
