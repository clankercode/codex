# X Client Branch Changes

This branch extends the turn-start bridge so callers can provide thread-scoped
base instructions and can opt into explicitly framed XML stdin input. Raw stdin
remains the default behavior.

It also adds a TUI-only sideband XML input path so an external harness can feed
structured user messages without multiplexing over the terminal keyboard stdin
stream.

## User-Facing Behavior

`codex-turn-start-bridge` now accepts:

```bash
--system-prompt <text>
--stdin-format raw|xml
```

`--system-prompt` passes `<text>` as the thread's system prompt override. The
override is applied only while starting or resuming a thread. It is not injected
into `turn/start` or `turn/steer`.

`--stdin-format raw` is the default and preserves the existing stdin behavior:

- input is chunked with the quiescence window
- `CODEX_QUEUE_MODE=<mode>` prefixes are still supported
- XML-looking text is treated as ordinary text

`--stdin-format xml` changes stdin into a sequence of XML fragments. Message
boundaries come from closing XML tags, not from quiescence timing.

`codex` TUI now also accepts:

```bash
--xml-input-fd <fd>
```

This is Unix-only in v1. The fullscreen TUI continues to read human keyboard
input from its normal terminal stdin path; the inherited file descriptor named
by `--xml-input-fd` is read as a sideband stream of XML fragments.

## XML Stdin Format

XML mode supports an optional startup system prompt followed by user messages:

```xml
<system_prompt>Be terse.</system_prompt>
<message type="user" queue="AfterToolCall">Summarize the diff.</message>
<message type="user">Run the focused tests.</message>
```

Rules:

- `<system_prompt>` is optional.
- `<system_prompt>` must appear before the first `<message>`.
- A later `<system_prompt>` is rejected.
- Passing both `--system-prompt` and XML `<system_prompt>` is rejected.
- `<message>` requires a `type` attribute.
- Only `type="user"` is supported today.
- Other message types, such as `system` or `assistant`, are reserved for future
  behavior and currently fail with a clear unsupported-type error.
- `queue` is optional and uses the existing `QueueMode` names.
- Missing `queue` means `Default`.
- XML entity decoding is supported.
- CDATA is supported, including text that looks like `</message>`.
- Nested inner XML inside `<message type="user">...</message>` is preserved
  literally as the message text instead of being rejected. This allows payloads
  like `<c2c ...>...</c2c>` to ride inside the user message body.
- EOF with a partial XML fragment is an error.

The bridge reads enough XML before app-server thread acquisition to capture the
optional startup system prompt and the first message. It then starts or resumes
the thread with the selected base instructions, queues any initial messages, and
continues reading XML-framed messages from stdin.

## TUI Structured Input Runtime

When `--xml-input-fd` is used:

- the XML reader starts before onboarding, resume/fork selection, and initial
  thread acquisition
- an early `<system_prompt>` is forwarded through the existing
  `base_instructions` field on `thread/start`, `thread/resume`, or `thread/fork`
- sideband messages arriving before the first visible thread exists are buffered
  and then bound to that first thread
- once a message is bound to a thread, later thread switches do not retarget it
- queued sideband messages reuse `BridgeController` queue semantics, driven by
  the same `turn/started`, `turn/completed`, `terminalInteraction`, and
  `item/completed` notifications used by the bridge
- queued sideband messages are shown in the TUI pending-input preview as a
  separate "Queued structured input" section
- startup-side notices such as early parse errors, rejected late
  `<system_prompt>` fragments, and XML-sideband EOF are replayed into TUI
  history once the chat widget exists
- recoverable parser errors do not disable the sideband reader; the reader
  skips the malformed prefix, surfaces the error, and continues parsing any
  later valid fragments that arrived in the same read buffer
- UTF-8/read failures and EOF disable only the structured-input sideband; they
  do not close the TUI session itself
- released sideband turns inherit the target thread's intended TUI
  collaboration mode and personality context instead of always falling back to
  default turn settings

## App-Server Client Changes

`codex_app_server_client::ThreadSessionRequest` now includes:

```rust
pub base_instructions: Option<String>
```

The app-server client request builder maps this field to the existing v2
protocol fields:

- `ThreadStartParams.base_instructions`
- `ThreadResumeParams.base_instructions`

No app-server protocol fields were added. The change uses existing
`base_instructions` support in `thread/start` and `thread/resume`.

## Additional Relevant Changes

The app-server v2 surface now also exposes a few pieces that matter for x-thin
integrations:

- `turn/start` accepts `baseInstructions` and `developerInstructions` as
  persistent session-setting overrides. When provided, later turns on the same
  thread inherit them, including resumed and forked threads.
- `turn/start` accepts `prefixedMessages`: typed history messages that are
  appended to thread history immediately before the turn's user input, but only
  if the turn starts successfully. Roles follow the same `user`/`assistant`/
  `developer` rule as `thread/inject_messages` (no `system`).
- `thread/inject_messages` appends typed text messages to thread history without
  constructing raw Responses API items. Supported roles are `user`,
  `assistant`, and `developer`.
- `thread/inject_messages` does not accept `system`. System-prompt replacement
  should continue to use the base-instructions path instead.
- `thread/import_transcript` creates a fresh thread from host-supplied typed
  messages. This is the new app-server path for Thin’s per-turn context
  reconstruction experiments when the supplied transcript should become the
  entire model-visible history for the new thread.
- `thread/import_transcript` accepts the same typed message roles as
  `thread/inject_messages` and likewise rejects `system`. Use
  `baseInstructions` when the host needs to replace the system prompt.
- `thread/import_transcript` optionally accepts `sourceThreadId` so the new
  thread can inherit defaults such as cwd and persisted model metadata from an
  existing thread while still using the supplied transcript as truth.
- When `thread/import_transcript` uses `sourceThreadId`, app-server releases the
  importing connection’s subscription to the source thread after success so the
  existing idle-unload path can reclaim that old loaded thread if nothing else
  is using it.
- `thread/update` is a new v2 method that updates session settings on an
  existing thread without starting a turn. It accepts the same override fields
  as `turn/start` (cwd, approval policy, approvals reviewer, sandbox policy,
  Windows sandbox level, model, reasoning effort, reasoning summary, service
  tier, collaboration mode, personality, base instructions, developer
  instructions). Omitted fields are left untouched.
- `codex` CLI now exposes the thin-style minimal-context bundle behind
  `--text-provider`, with `--minimal-context` retained as an alias.

## App-Server Stdio Transport

`codex-app-server-client` now offers a third transport alongside in-process and
remote websocket:

- `AppServerClient::Stdio` wraps `StdioAppServerClient`, which spawns a local
  `codex app-server` child process and speaks newline-delimited JSON-RPC over
  its stdio.
- `StdioAppServerConnectArgs` captures the spawn command, environment, and
  startup identity the same way `RemoteAppServerConnectArgs` does for
  websocket.
- The facade mirrors the existing API surface: request, `request_typed`,
  notification, server-request resolve/reject, `next_event`, and graceful
  `shutdown`. Embedded surfaces that accept in-process also accept stdio where
  thread parameters are constructed in-process.

The crate also exports a shared `turn_client` module:

- `CodexTurnClient` / `CodexTurnSession` wrap the per-thread turn lifecycle.
- `ThreadSessionRequest` and `ThreadSessionStart` describe thread acquisition
  (including the `baseInstructions` field documented above) independent of
  transport.
- `TurnRequest` and `TurnClientError` provide typed turn submission and error
  reporting so callers do not re-build protocol plumbing per surface.

## Compact And Reasoning Overrides

- `config.toml` gains `compact_model`: the model used for manual `/compact`
  turns when it should differ from the session model. When unset, compaction
  uses the active model.
- Manual compaction now builds its prompt from `turn_context.base_instructions()`
  and calls the model stream with the current runtime reasoning effort, so the
  persistent overrides introduced in `turn/start` are honored during compaction.
- Reasoning effort is now a runtime override on the turn context rather than a
  one-shot argument, so later turns and compactions use the effort selected by
  the user or by `/effort` until changed.

## TUI Slash Commands And Status Line

- `/effort [off|low|medium|high|xhigh|status]` reports or changes the current
  reasoning effort. With no argument (or `status`) it prints the current
  effort. Setting a value updates Plan mode effort when in Plan mode, otherwise
  the session-level effort, and pushes an `override_turn_context` update with
  the new effort.
- `/idletime` toggles a hidden idle-timing context injection for new turns and
  supports a subcommand form for enable/disable.
- `codex-rs/tui/src/idle_timing.rs` adds `IdleTimingState` and
  `PreparedIdleTimingSubmission`. When injection is enabled, the status line
  shows a compact "idle for X" marker that refreshes once per second and a
  developer-role note is prepared for the next turn's `prefixedMessages`.
- `SlashCommand::Effort` and `SlashCommand::IdleTime` are registered in
  `slash_command.rs` with help text visible in the slash popup.

## Version

- `codex-rs` workspace version is bumped to `0.122.0`. The new
  `turn-start-bridge` and `turn-start-bridge-core` crates are added to the
  workspace members list.

## Turn-Start Bridge Core Changes

`codex-turn-start-bridge-core` now exports XML stdin parsing types:

- `ParsedXmlInput`
- `XmlInputParser`
- `XmlInputError`

The parser is incremental. `push()` accepts more stdin text and returns any
complete XML fragments parsed into either a startup system prompt or a
`ParsedMessage`. `finish()` verifies that EOF did not leave an incomplete XML
fragment buffered.

The parser intentionally lives in the core crate so XML framing can be tested
without spawning the app-server bridge process.

New dependencies for `codex-turn-start-bridge-core`:

- `quick-xml`
- `serde`

`Cargo.lock` and `MODULE.bazel.lock` were refreshed for the dependency change.

## Turn-Start Bridge Runtime Changes

The bridge now creates stdin message channels before app-server connection so
XML mode can read startup metadata before calling `thread/start` or
`thread/resume`.

Raw mode:

- starts or resumes the app-server thread immediately
- spawns the existing quiescence-based raw stdin reader
- forwards raw reader errors through the main select loop

XML mode:

- reads the XML prelude from stdin first
- extracts an optional startup system prompt
- stashes one or more parsed initial user messages
- starts or resumes the app-server thread with the selected base instructions
- releases the stashed messages through the existing `BridgeController`
- spawns a follow-up XML reader if stdin is still open

This keeps all turn release semantics in the existing controller. XML mode only
changes how stdin is framed and how the initial system prompt is discovered.

## Validation And Tests

New or expanded test coverage includes:

- `thread/start` request construction carries `base_instructions`.
- `thread/resume` request construction carries `base_instructions`.
- XML parsing accepts startup `<system_prompt>` before user messages.
- XML parsing decodes entities and CDATA.
- XML parsing rejects late `<system_prompt>`.
- XML parsing rejects unsupported message types.
- XML parsing reports incomplete fragments at EOF.
- Bridge request construction includes `--system-prompt`.
- Bridge request construction rejects duplicate CLI/XML system prompts.
- XML prelude reading captures the startup system prompt and first messages.
- Existing raw chunking and bridge controller behavior still pass.

Commands run on this branch:

```bash
cargo test -p codex-turn-start-bridge-core
cargo test -p codex-app-server-client
cargo test -p codex-turn-start-bridge
just fmt
just fix -p codex-turn-start-bridge-core
just fix -p codex-app-server-client
just fix -p codex-turn-start-bridge
just bazel-lock-update
just bazel-lock-check
git diff --check
```

The final `just fix` passes made two Clippy-driven cleanups:

- collapsed a nested `if` in `codex-rs/app-server-client/src/stdio.rs`
- inlined a `format!` argument in `codex-rs/turn-start-bridge/src/main.rs`

Per the repository guidance, tests were not rerun after the final formatting
and lint-fix commands.
