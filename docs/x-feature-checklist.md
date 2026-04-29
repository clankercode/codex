# X-Thin Feature Checklist

This checklist is the current audit record for the fork-specific work described
in `x-todo.txt`, `x-todo.txt.save`, and `docs/x-*`.

Legend:

- `Implemented`: current code and tests show the feature exists.
- `Partial`: meaningful support exists, but the original note is broader than
  the current implementation or still has a known gap.
- `Deferred`: explicitly considered out of scope for the current fork pass.
- `Open`: should be implemented, but current evidence is not enough to call it
  done.

## Current Fork Features

| Feature | Status | Evidence / notes |
| --- | --- | --- |
| Inject non-user messages | Partial | `thread/inject_messages`, `turn/start.prefixedMessages`, and `turn/start.prefixedItems` support host-supplied history. Typed roles are `user`, `assistant`, and `developer`; `system` is intentionally handled through `baseInstructions`, not message injection. Covered by app-server v2 tests. |
| Replace the system prompt / base instructions later | Implemented | `thread/start`, `thread/resume`, `turn/start`, `thread/update`, XML `<system_prompt>`, and bridge `--system-prompt` all route through `baseInstructions`; compaction uses `turn_context.base_instructions()`. |
| Thin stdout / machine-readable details | Partial | `codex exec --json` exists, and the turn-start bridge has JSONL fd sidebands for thread handoff, control events, and server requests. There is no literal `codex-turn-start-bridge --output-format jsonl` flag in the current code, so keep this partial unless that old wishlist item is re-scoped to the fd sidebands. |
| Tool-call passthrough / externally registered tools | Partial | App-server v2 has dynamic tool specs/calls and MCP server tool/resource call surfaces. A generic "external MCP passthrough" contract is not documented as complete in `docs/x-client-changes.md`. |
| Compact status line for context and run state | Implemented | `[tui].status_line` supports compact status items including context used/remaining, run state, model, version, rate-limit windows, and idle/run timing. |
| First-class minimal-context / no-tool mode | Implemented | `codex --text-provider`, alias `--minimal-context`, and `codex --x-thin-check` cover the thin override bundle. |
| Version reports real upstream version instead of `v0.0.0` | Implemented | `codex-rs/Cargo.toml` workspace version is `0.125.0-alpha.2`. |
| `/compact-with-mini` slash command | Implemented | Slash command is registered, built into the popup, covered by `x_thin_slash_commands`, and uses the compact-model override path for manual compaction. |
| `compact_model` config for manual compaction | Implemented | Config parses `compact_model`; core manual compaction routes through `compact_with_model` and falls back to config when no explicit override is supplied. |
| `/idle-time` slash command | Implemented | Slash command is registered and covered by `x_thin_slash_commands`; it toggles hidden timing injection and reports status. |
| Hidden idle timing injection | Implemented | `IdleTimingState` prepares developer-role timing context for new turns when enabled and suppresses first-turn or active-turn injection. |
| Idle/run status-line item | Implemented | `idle-time` status item shows idle or running duration, pins the value to the right side of the footer, and schedules a 1-second redraw while visible. |
| Turn-finish timing row with live idle suffix | Implemented | `TurnTimingIdleHandle` updates the visible turn-timing row while idle without changing transcript storage. |
| Suppress `[After ...]` for steering messages | Implemented | Idle resume notes are prepared only for idle turn-start submissions, not active-turn steers. |
| First message should not get `[After ...]` | Implemented | `IdleTimingState` has no last-turn baseline before the first model turn, so no resume note is emitted. |
| Steering status counts from injection time | Implemented | `record_steer_user_message` records `Instant::now()` at injection time. |
| Avoid false idle status while agent is active | Implemented | In-flight turn state drives `Run ...` status until completion; recent right-pinning and refresh fixes keep it visible and current. |
| `/effort [off\|low\|medium\|high\|xhigh\|status]` | Implemented | Slash command is registered, live during assistant turns, updates runtime reasoning effort, supports status reporting, and is covered by `x_thin_slash_commands`. |
| `/effort <invalid>` clears the input | Implemented | `/effort` is in the slash dispatcher clear-input set; old todo item is satisfied. |
| `/permissions (default\|guardian\|all)` while running | Implemented | Inline permissions and `/approvals` parity were implemented with running-turn availability and Windows confirmation flow coverage. |
| Repo-scoped stop hooks docs cleanup | Implemented | `x-todo.txt` records this as docs-only; hook support already existed. |
| `/mcp-reload` slash command | Implemented | Slash command is registered, built in, routed to `config/mcpServer/reload`, and covered by `x_thin_slash_commands`. |
| CLI flag to confirm x-thin features | Implemented | `codex --x-thin-check` prints a JSON report and exits non-zero on missing thin overrides. |
| XML stdin format for bridge | Implemented | `--stdin-format xml`, XML parser, startup `<system_prompt>`, user message framing, CDATA/entities, queue modes, and error handling are implemented and tested. |
| TUI `--xml-input-fd` structured input | Implemented | TUI sideband reader buffers pre-thread messages, binds messages to target threads, shows pending structured input, and keeps terminal keyboard stdin separate. |
| Bridge/TUI sideband fds for server requests and control events | Implemented | `--thread-id-fd`, `--server-request-events-fd`, `--server-request-responses-fd`, `--control-events-fd`, and `--control-responses-fd` are documented and implemented with fd validation. |
| `--approvals-reviewer` bridge override | Implemented | Bridge forwards approvals reviewer through thread and turn requests. |
| App-server stdio transport and turn client | Implemented | `StdioAppServerClient`, `CodexTurnClient`, `ThreadSessionRequest`, and `TurnRequest` are implemented and exported. |
| `thread/import_transcript` | Implemented | App-server v2 creates a fresh thread from host-supplied typed messages and can inherit defaults from a source thread. |
| `thread/update` | Implemented | App-server v2 updates session settings without starting a turn. |
| `AfterAnyItem` sideband release after assistant items | Implemented with residual risk | Bridge and TUI classify assistant messages, reasoning, tool calls, plans, compaction, and other assistant items as release points while ignoring user/hook prompts, with unit coverage. The old `x-todo.txt` note also mentions an active-turn validation error/hang; no dedicated regression matching that sample was found in this pass, so keep that scenario on the watch list. |

## Wishlist / Deferred Items

| Feature | Status | Evidence / notes |
| --- | --- | --- |
| Stable public Python/TypeScript app-server SDK | Deferred | Listed in the codename-thin wishlist, but not selected by the feature matrix for this pass. |
| Capability introspection JSON | Deferred | The current repo has initialize capabilities and `--x-thin-check`, but not a dedicated fork capability-introspection contract. |
| Stable error codes | Deferred | Existing JSON-RPC/app-server errors provide meaningful support; new contract work was explicitly deferred in `x-todo.txt`. |
| Explicit timeout/cancellation semantics | Deferred | Listed in the follow-up wishlist; not implemented as a fork feature in the current docs. |
| Event and usage capture contract | Deferred | Usage/token notifications and replay exist in app-server tests, but a broad lossless event/usage capture contract was marked too cross-cutting for the current pass. |
| Structured usage/quota notifications | Deferred new work | Existing support is considered meaningful enough that no new fork work was selected. |
| Thread replay/import API | Partial | `thread/import_transcript` is implemented; a broader replay/import contract remains larger than the current fork pass. |
| Lossless raw event replay | Deferred | Explicitly marked too large / cross-boundary in the feature matrix. |
| SDK keyword stability | Deferred | Listed in the follow-up wishlist; no current implementation target. |

## Source Drift Notes

- `x-todo.txt.save` was stale: it still listed `/compact-with-mini` and
  `/mcp-reload` as unchecked even though both are implemented and tested.
- `x-todo.txt` still has the `AfterAnyItem` hang/validation note unchecked.
  Current implementation evidence supports the general `AfterAnyItem` behavior,
  but the exact sampled validation-error scenario should get a focused
  regression if it recurs.
