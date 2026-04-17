# X Client Branch Changes

This branch extends the turn-start bridge so callers can provide thread-scoped
base instructions and can opt into explicitly framed XML stdin input. Raw stdin
remains the default behavior.

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
- EOF with a partial XML fragment is an error.

The bridge reads enough XML before app-server thread acquisition to capture the
optional startup system prompt and the first message. It then starts or resumes
the thread with the selected base instructions, queues any initial messages, and
continues reading XML-framed messages from stdin.

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
- `thread/inject_messages` appends typed text messages to thread history without
  constructing raw Responses API items. Supported roles are `user`,
  `assistant`, and `developer`.
- `thread/inject_messages` does not accept `system`. System-prompt replacement
  should continue to use the base-instructions path instead.
- `codex` CLI now exposes the thin-style minimal-context bundle behind
  `--text-provider`, with `--minimal-context` retained as an alias.

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
