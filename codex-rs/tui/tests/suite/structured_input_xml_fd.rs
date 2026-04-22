use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio::select;
use tokio::time::sleep;
use tokio::time::timeout;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_input_fd_delivers_valid_message_and_stays_alive_after_eof() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let fixture_path =
        codex_utils_cargo_bin::find_resource!("../core/tests/cli_responses_fixture.sse")?;
    let codex_home = tempdir()?;

    let repo_root_display = repo_root.display();
    let config_contents = format!(
        r#"model = "gpt-5.4"
model_provider = "openai"

[projects."{repo_root_display}"]
trust_level = "trusted"
"#
    );
    std::fs::write(codex_home.path().join("config.toml"), config_contents)?;
    std::fs::write(
        codex_home.path().join("auth.json"),
        r#"{"OPENAI_API_KEY":"sk-test-key","tokens":null,"last_refresh":null}"#,
    )?;

    let codex = codex_utils_cargo_bin::cargo_bin("codex")
        .context("structured-input smoke test requires the codex binary")?;
    let program = codex.to_string_lossy().into_owned();

    let mut fds = [0; 2];
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(result, 0, "pipe should be created");
    let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };

    let mut env = HashMap::new();
    env.insert(
        "CODEX_HOME".to_string(),
        codex_home.path().display().to_string(),
    );
    env.insert("OPENAI_API_KEY".to_string(), "dummy".to_string());
    env.insert(
        "CODEX_RS_SSE_FIXTURE".to_string(),
        fixture_path.display().to_string(),
    );

    let args = vec![
        "--xml-input-fd".to_string(),
        read_end.as_raw_fd().to_string(),
        "--no-alt-screen".to_string(),
        "-C".to_string(),
        repo_root.display().to_string(),
        "-c".to_string(),
        "analytics.enabled=false".to_string(),
    ];

    let spawned = codex_utils_pty::pty::spawn_process_with_inherited_fds(
        &program,
        &args,
        &repo_root,
        &env,
        &None,
        codex_utils_pty::TerminalSize::default(),
        &[read_end.as_raw_fd()],
    )
    .await?;
    drop(read_end);

    write_end
        .write_all(b"<message type=\"user\">sideband smoke</message>")
        .context("failed to write structured input")?;
    drop(write_end);

    let codex_utils_pty::SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    let mut output_rx = codex_utils_pty::combine_output_receivers(stdout_rx, stderr_rx);
    let mut exit_rx = exit_rx;
    let writer_tx = session.writer_sender();
    let interrupt_writer = writer_tx.clone();
    let mut output = Vec::new();
    let mut saw_fixture_hello = false;
    let mut saw_sideband_eof = false;
    let mut requested_shutdown = false;

    let exit_code_result = timeout(Duration::from_secs(30), async {
        loop {
            select! {
                result = output_rx.recv() => match result {
                    Ok(chunk) => {
                        if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                            let _ = writer_tx.send(b"\x1b[1;1R".to_vec()).await;
                        }
                        output.extend_from_slice(&chunk);
                        let text = String::from_utf8_lossy(&output);
                        if let Some(unexpected_prompt) = unexpected_startup_prompt(&text) {
                            anyhow::bail!(
                                "unexpected startup prompt `{unexpected_prompt}` in structured-input smoke test; output: {text}"
                            );
                        }
                        saw_fixture_hello |= text.contains("fixture hello");
                        saw_sideband_eof |= text.contains("Structured input channel closed");

                        if saw_fixture_hello
                            && saw_sideband_eof
                            && !requested_shutdown
                        {
                            assert!(
                                !session.has_exited(),
                                "structured-input EOF should not terminate the TUI; output: {text}"
                            );
                            requested_shutdown = true;
                            for _ in 0..4 {
                                let _ = interrupt_writer.send(vec![3]).await;
                                sleep(Duration::from_millis(500)).await;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break Ok(exit_rx.await?)
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                },
                result = &mut exit_rx => break Ok(result?),
            }
        }
    })
    .await;

    let exit_code = match exit_code_result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            session.terminate();
            let output_text = String::from_utf8_lossy(&output);
            anyhow::bail!(
                "timed out waiting for codex structured-input smoke test to exit; output: {output_text}"
            );
        }
    };

    let output_text = String::from_utf8_lossy(&output);
    assert!(
        saw_fixture_hello,
        "expected sideband message to start a turn, got: {output_text}"
    );
    assert!(
        saw_sideband_eof,
        "expected structured-input EOF info message, got: {output_text}"
    );
    assert!(
        requested_shutdown,
        "test never reached the state where the TUI stayed alive after sideband EOF: {output_text}"
    );

    let interrupt_only_output = {
        let trimmed_output = output_text.trim();
        !trimmed_output.is_empty()
            && trimmed_output
                .chars()
                .all(|character| character == '^' || character == 'C' || character.is_whitespace())
    };
    assert!(
        exit_code == 0 || exit_code == 130 || (exit_code == 1 && interrupt_only_output),
        "unexpected exit code from codex structured-input smoke test: {exit_code}; output: {output_text}",
    );

    Ok(())
}

fn unexpected_startup_prompt(output: &str) -> Option<&'static str> {
    [
        "Welcome to Codex",
        "Choose how you'd like Codex to proceed.",
    ]
    .into_iter()
    .find(|prompt| output.contains(prompt))
}
