# Codex stdin / structured-input shutdown hang

## Summary

Codex's structured-input path appears to keep a detached XML reader task alive
past shutdown, which can make exit look like a "stdin hang" in `c2c start codex`
sessions.

The likely issue is not XML parsing itself. It is the lifetime / cancellation
shape of the structured-input reader: the TUI spawns a background task for
`--xml-input-fd`, but unlike thread event listeners, there is no explicit
shutdown cancellation path for that reader.

## Symptom

Observed / reported behavior:

- managed `c2c start codex` sessions can hang on exit
- the hang has been described as "stdin mishandling"
- the problem becomes more visible in Codex-managed sessions that also have
  live structured-input delivery

## Evidence

- `codex-rs/tui/src/structured_input/unix.rs` spawns the XML reader with
  `tokio::spawn(...)` and does not retain a `JoinHandle`
- `codex-rs/tui/src/structured_input/mod.rs` only tracks `reader_active`
  and a `mpsc::UnboundedReceiver`; there is no abort hook
- `codex-rs/tui/src/app.rs` aborts thread event listeners on shutdown:
  `shutdown_current_thread()` calls `abort_thread_event_listener()`
  and `abort_all_thread_event_listeners()` exists for broader cleanup
- there is no equivalent structured-input teardown path
- the TUI event loop exits on normal terminal shutdown, but the detached XML
  reader still depends on EOF to finish cleanly

## Why this is suspicious

The Codex fork already treats XML sideband input as an always-on background
channel:

- EOF on the XML channel is explicitly treated as "disable structured input"
  rather than "terminate the TUI"
- `AfterAnyItem` queueing is used for interruptions, so structured input can
  arrive while the app is busy

That means the structured-input reader must also be cancelled cleanly at exit.
Relying only on the writer side to close at the right time is fragile, because
shutdown timing depends on several processes:

- the managed Codex child
- the c2c deliver daemon
- the outer launcher
- the TUI runtime itself

## Likely fix shape

Prefer one of these:

1. Keep a `JoinHandle` or `AbortHandle` for the structured-input reader and
   abort it during shutdown, just like thread event listeners.
2. Add an explicit structured-input shutdown signal that is sent before the
   app begins terminal teardown.
3. As a backup, make the reader task exit promptly on app shutdown even if EOF
   is delayed.

## Notes

- `codex-rs/tui/tests/suite/structured_input_xml_fd.rs` currently verifies that
  XML EOF does not kill the TUI. That test is useful, but it does not cover
  the shutdown-cancellation path.
- This note is a hypothesis based on code inspection and current c2c dogfood
  symptoms; it still needs live tmux validation.
