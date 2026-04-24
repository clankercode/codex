use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;

use codex_turn_start_bridge_core::XmlInputParser;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::StructuredInputReaderEvent;

pub(crate) fn spawn_xml_input_reader(
    xml_input_fd: i32,
) -> std::io::Result<(
    mpsc::UnboundedReceiver<StructuredInputReaderEvent>,
    JoinHandle<()>,
)> {
    if xml_input_fd < 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid --xml-input-fd value `{xml_input_fd}`"),
        ));
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let mut file = unsafe { File::from_raw_fd(xml_input_fd) };

    let handle = tokio::spawn(async move {
        let mut parser = XmlInputParser::default();
        let mut pending_utf8_bytes = Vec::new();
        let original_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if original_flags == -1 {
            let err = std::io::Error::last_os_error();
            let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                "Failed to prepare structured input reader: {err}"
            )));
            return;
        }
        if unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_SETFL,
                original_flags | libc::O_NONBLOCK,
            )
        } == -1
        {
            let err = std::io::Error::last_os_error();
            let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                "Failed to prepare structured input reader: {err}"
            )));
            return;
        }

        let mut buf = [0_u8; 4096];
        loop {
            match Read::read(&mut file, &mut buf) {
                Ok(0) => {
                    finish_reader_input(&parser, &tx, &pending_utf8_bytes);
                    return;
                }
                Ok(bytes_read) => {
                    if let Err(message) = forward_utf8_bytes(
                        &mut parser,
                        &tx,
                        &mut pending_utf8_bytes,
                        &buf[..bytes_read],
                    ) {
                        let _ = tx.send(StructuredInputReaderEvent::ReadError(message));
                        return;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                        "Failed to read structured input: {err}"
                    )));
                    return;
                }
            }
        }

        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, original_flags) } == -1 {
            let err = std::io::Error::last_os_error();
            let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                "Failed to prepare structured input reader: {err}"
            )));
            return;
        }

        let _ = tx.send(StructuredInputReaderEvent::StartupDrainComplete);

        let mut file = tokio::fs::File::from_std(file);
        loop {
            match file.read(&mut buf).await {
                Ok(0) => {
                    finish_reader_input(&parser, &tx, &pending_utf8_bytes);
                    break;
                }
                Ok(bytes_read) => {
                    if let Err(message) = forward_utf8_bytes(
                        &mut parser,
                        &tx,
                        &mut pending_utf8_bytes,
                        &buf[..bytes_read],
                    ) {
                        let _ = tx.send(StructuredInputReaderEvent::ReadError(message));
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                        "Failed to read structured input: {err}"
                    )));
                    break;
                }
            }
        }
    });

    Ok((rx, handle))
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

fn forward_utf8_bytes(
    parser: &mut XmlInputParser,
    tx: &mpsc::UnboundedSender<StructuredInputReaderEvent>,
    pending_utf8_bytes: &mut Vec<u8>,
    input_bytes: &[u8],
) -> Result<(), String> {
    pending_utf8_bytes.extend_from_slice(input_bytes);

    let valid_up_to = match std::str::from_utf8(pending_utf8_bytes) {
        Ok(_) => pending_utf8_bytes.len(),
        Err(err) if err.error_len().is_none() => err.valid_up_to(),
        Err(err) => {
            return Err(format!("Structured input is not valid UTF-8: {err}"));
        }
    };

    if valid_up_to == 0 {
        return Ok(());
    }

    let input = match std::str::from_utf8(&pending_utf8_bytes[..valid_up_to]) {
        Ok(input) => input,
        Err(err) => return Err(format!("Structured input is not valid UTF-8: {err}")),
    };
    if let Err(message) = forward_parsed_input(parser, tx, input) {
        let _ = tx.send(StructuredInputReaderEvent::ParseError(message));
        while parser.discard_malformed_prefix() {
            match forward_parsed_input(parser, tx, "") {
                Ok(()) => break,
                Err(message) => {
                    let _ = tx.send(StructuredInputReaderEvent::ParseError(message));
                }
            }
        }
    }
    pending_utf8_bytes.drain(..valid_up_to);

    Ok(())
}

fn finish_reader_input(
    parser: &XmlInputParser,
    tx: &mpsc::UnboundedSender<StructuredInputReaderEvent>,
    pending_utf8_bytes: &[u8],
) {
    if !pending_utf8_bytes.is_empty() {
        match std::str::from_utf8(pending_utf8_bytes) {
            Ok(_) => {
                let _ = tx.send(StructuredInputReaderEvent::ReadError(
                    "Structured input ended with an incomplete UTF-8 sequence.".to_string(),
                ));
            }
            Err(err) => {
                let _ = tx.send(StructuredInputReaderEvent::ReadError(format!(
                    "Structured input is not valid UTF-8: {err}"
                )));
            }
        }
        return;
    }
    if let Err(err) = parser.finish() {
        let _ = tx.send(StructuredInputReaderEvent::ParseError(err.to_string()));
    }
    let _ = tx.send(StructuredInputReaderEvent::Eof);
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
    use tokio::time::Duration;
    use tokio::time::sleep;

    async fn collect_events(
        rx: &mut mpsc::UnboundedReceiver<StructuredInputReaderEvent>,
    ) -> Vec<StructuredInputReaderEvent> {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let terminal = matches!(
                event,
                StructuredInputReaderEvent::Eof | StructuredInputReaderEvent::ReadError(_)
            );
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    }

    #[tokio::test]
    async fn malformed_fragment_and_following_valid_fragment_are_recovered_within_one_read() {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_end = fds[0];
        let mut write_end = unsafe { File::from_raw_fd(fds[1]) };
        let (mut rx, _handle) = spawn_xml_input_reader(read_end).expect("reader should start");

        write!(
            write_end,
            "<message type=\"user\"><broken <message type=\"user\">ok</message>"
        )
        .expect("write should succeed");
        drop(write_end);

        let events = collect_events(&mut rx).await;

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
    async fn aborting_reader_handle_closes_open_pipe_without_eof() {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_end = fds[0];
        let write_end = unsafe { File::from_raw_fd(fds[1]) };
        let (mut rx, handle) = spawn_xml_input_reader(read_end).expect("reader should start");

        handle.abort();
        let event = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("reader did not close after abort");
        assert!(event.is_none(), "aborted reader should close the channel");

        drop(write_end);
    }

    #[tokio::test]
    async fn malformed_fragment_and_partial_following_valid_fragment_are_recovered_across_reads() {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_end = fds[0];
        let mut write_end = unsafe { File::from_raw_fd(fds[1]) };
        let (mut rx, _handle) = spawn_xml_input_reader(read_end).expect("reader should start");

        write!(write_end, "<message type=\"user\"><broken <mess").expect("write should succeed");
        write!(write_end, "age type=\"user\">ok</message>").expect("write should succeed");
        drop(write_end);

        let events = collect_events(&mut rx).await;

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

    #[tokio::test]
    async fn valid_utf8_split_across_reads_is_buffered_until_complete() {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_end = fds[0];
        let mut write_end = unsafe { File::from_raw_fd(fds[1]) };
        let (mut rx, _handle) = spawn_xml_input_reader(read_end).expect("reader should start");

        write!(write_end, "<message type=\"user\">caf").expect("write should succeed");
        write_end.write_all(&[0xC3]).expect("write should succeed");
        write_end.flush().expect("flush should succeed");
        sleep(Duration::from_millis(10)).await;
        write_end.write_all(&[0xA9]).expect("write should succeed");
        write!(write_end, "</message>").expect("write should succeed");
        drop(write_end);

        let events = collect_events(&mut rx).await;

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StructuredInputReaderEvent::ReadError(_))),
            "unexpected UTF-8 read error, got {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                StructuredInputReaderEvent::Parsed(ParsedXmlInput::Message(ParsedMessage {
                    queue_mode: QueueMode::Default,
                    text,
                })) if text == "café"
            )),
            "expected buffered split UTF-8 message, got {events:?}"
        );
    }

    #[tokio::test]
    async fn invalid_utf8_still_reports_read_error() {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_end = fds[0];
        let mut write_end = unsafe { File::from_raw_fd(fds[1]) };
        let (mut rx, _handle) = spawn_xml_input_reader(read_end).expect("reader should start");

        write!(write_end, "<message type=\"user\">").expect("write should succeed");
        write_end.write_all(&[0xFF]).expect("write should succeed");
        drop(write_end);

        let events = collect_events(&mut rx).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StructuredInputReaderEvent::ReadError(_))),
            "expected read error, got {events:?}"
        );
    }
}
