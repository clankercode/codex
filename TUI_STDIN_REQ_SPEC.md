# TUI STDIN XML Requirements for c2c

## Goal

Enable c2c to deliver inbound messages into a live Codex session while preserving the normal fullscreen Codex CLI TUI as the visible foreground interface.

This spec is for the **TUI executable path**, not the standalone `codex-turn-start-bridge` binary.

## Current State

The current fork already proves several useful pieces:

- `codex-turn-start-bridge` exists as a separate binary that reads stdin and drives `codex app-server`.
- XML stdin framing already exists there.
- app-server already supports typed message injection and thread/session overrides.

What does **not** appear to exist yet is a way to keep the normal `codex` TUI in front while also feeding that session structured XML input from an outer process.

## Non-Goals

- Replacing the normal Codex TUI with `codex-turn-start-bridge`
- Using PTY keystroke injection as the primary receive path
- Requiring c2c to speak app-server directly for the first working version
- Supporting arbitrary message roles beyond `user` in v1
- Requiring c2c-specific parsing inside Codex

## Required Operator Model

The intended operator model is:

1. The user launches the normal Codex TUI.
2. c2c or a c2c-managed outer process keeps a writable structured-input channel to that TUI process.
3. The human can continue typing and using slash commands normally.
4. c2c writes XML-framed inbound messages into the structured-input channel.
5. The TUI appends those messages to the active thread as genuine user turns.

Example XML payload:

```xml
<message type="user"><c2c event="message" from="peer" alias="peer">hello</c2c></message>
```

## Hard Requirements

### 1. TUI-preserving launch mode

The normal Codex TUI executable must expose a mode that enables structured XML input without replacing the TUI.

Acceptable shapes include:

```bash
codex --stdin-format xml
codex tui --stdin-format xml
codex --structured-input xml
```

Default behavior must remain unchanged when the mode is not enabled.

### 2. Human keyboard input must be decoupled from stdin

This is the key requirement.

In structured-input mode, the TUI must not depend on ordinary stdin for human keyboard input, because c2c also needs to write structured frames.

One of these must be true:

- stdin becomes the XML channel, and the TUI reads human keyboard input from the controlling TTY or PTY directly
- the TUI offers a dedicated XML input fd or pipe path, such as `--xml-input-fd N` or `--xml-input-pipe PATH`

Without this decoupling, the stdin+xml plan is not viable while preserving the normal TUI.

### 3. XML grammar must match the bridge grammar

The TUI path should reuse the same XML framing semantics as `codex-turn-start-bridge` to avoid split behavior.

Required support:

- optional startup `<system_prompt>`
- `<message type="user" queue="...">...</message>`
- entity decoding
- CDATA
- incomplete-fragment detection at EOF

v1 may explicitly reject unsupported message roles, but the error must be clear and non-fatal.

### 4. Incoming XML messages must become real user turns

Each accepted `<message type="user">...</message>` must be appended to the live thread as a genuine user message.

It must not be handled as:

- synthetic terminal keystrokes
- hidden developer-only state
- composer text that still requires Enter
- a transient toast only

The injected message must participate in normal:

- history
- resume/fork behavior
- transcript visibility
- model-visible context

### 5. Thread routing must be deterministic

By default, XML messages should target the active visible thread in the current TUI session.

If there is no active thread yet, behavior must be defined and stable. Acceptable policies are:

- create a fresh visible thread automatically
- queue until a thread exists

The TUI must not silently drop structured input because no thread is active.

### 6. Queue semantics must match normal user-turn semantics

If a turn is already in progress, the injected XML message must queue exactly as a real user turn would.

If the `queue` attribute is present, existing queue-mode semantics must be honored.

If the `queue` attribute is absent, normal default queue semantics apply.

For unsolicited inbound sideband traffic such as c2c broker messages, callers
should prefer:

```xml
<message type="user" queue="AfterAnyItem">...</message>
```

`AfterAnyItem` should release on the next completed assistant turn item
boundary, not only after tool calls.

### 7. Failure isolation

Malformed XML must not crash or wedge the TUI.

Required behavior:

- reject the bad fragment
- log or surface an understandable error
- continue accepting later valid fragments

Closing the XML input channel must not terminate the interactive TUI session unless explicitly configured.

### 8. Observability

Structured-input mode must emit enough signal for debugging.

Minimum observability:

- structured-input mode enabled at startup
- XML fragment accepted or rejected
- message routed to thread `<id>`
- message queued or applied immediately
- parser or channel EOF events

This can be stderr logging, trace logging, or a debug surface.

## Strongly Preferred Requirements

### 1. Dedicated sideband channel support

A dedicated sideband ingress is cleaner than overloading stdin.

Preferred interfaces:

```bash
codex --xml-input-fd 3
codex --xml-input-pipe /tmp/codex-xml.sock
```

If this exists, c2c can avoid stdin multiplexing entirely.

### 2. Reuse the existing XML parser implementation

The TUI path should reuse `turn-start-bridge-core` parsing/types rather than re-implementing XML framing a second time.

That keeps behavior aligned across:

- standalone bridge mode
- TUI structured-input mode

### 3. Explicit startup acknowledgement

A startup log line or status marker confirming that structured-input mode is active would make operator troubleshooting much easier.

## c2c-Specific Expectations

c2c does not need Codex to understand the inner `<c2c ...>` envelope.

c2c only needs the TUI to faithfully inject the outer XML-framed payload as a user turn.

Expected c2c behavior:

- c2c watches the broker inbox
- c2c wraps the broker-native message body into `<message type="user">...</message>`
- the TUI appends that message into the active thread
- Codex sees it as ordinary user input

Example c2c-delivered payload:

```xml
<message type="user"><c2c event="message" from="storm-beacon" alias="storm-beacon">hello from peer</c2c></message>
```

## Minimal Acceptable v1 Contract

A minimal contract that would unblock c2c is:

```bash
codex --stdin-format xml
```

with the following behavior:

- stdin is reserved for XML fragments
- the human still interacts with the normal fullscreen TUI
- each `<message type="user">...</message>` becomes a real visible user turn in the active thread
- malformed XML is rejected without killing the TUI
- EOF on the XML channel does not kill the TUI

## Acceptance Tests

### Basic delivery

1. Launch the normal Codex TUI in structured-input mode.
2. Confirm the human can still type normally.
3. Send `<message type="user">Reply exactly ok</message>` through the XML channel.
4. Verify the message appears visibly in the active thread as a user turn.
5. Verify Codex handles it using normal turn scheduling.

### Queue behavior

1. Start a long-running turn.
2. Send a second XML user message while the turn is still running.
3. Verify the message is queued, not dropped.
4. Verify it runs after the current turn according to queue semantics.

### Parser robustness

1. Send malformed XML.
2. Verify the TUI does not crash.
3. Verify a later valid fragment is still accepted.

### Channel lifecycle

1. Close the XML channel.
2. Verify the TUI remains interactive.
3. Verify human keyboard input still works.

### History correctness

1. Inject a message through XML structured input.
2. Resume or fork the session.
3. Verify the injected message remains in visible/model history as a user turn.

## Recommended Internal Implementation Direction

The cleanest internal design appears to be:

- keep the normal TUI as the operator-facing process
- add a structured-input ingress path to the TUI executable
- reuse the existing XML parsing logic from `turn-start-bridge-core`
- route accepted XML user messages into the same thread/session machinery the TUI already uses for app-server-backed threads

The important point is that this should be a **TUI-native ingress path**, not a separate bridge frontend.

## Why This Is Needed

`codex-turn-start-bridge` is already useful, but it currently looks like a standalone stdin-driven bridge frontend over `codex app-server`.

That is not enough for c2c, because c2c needs:

- the normal Codex TUI to remain visible and interactive
- inbound c2c messages to arrive as real user turns
- no dependence on PTY keystroke injection for the primary path

This spec defines the minimum TUI-side capability needed to make that possible.
