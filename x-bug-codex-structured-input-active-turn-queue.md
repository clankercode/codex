# Structured Input Active-Turn Validation Rejects Sideband User Messages Mid-Turn

## Summary

When Codex TUI is launched with `--xml-input-fd`, externally injected XML user messages appear to work only when Codex is already idle and waiting for input.

If a structured XML user message is injected while Codex is in the middle of a turn, the TUI reports a structured-input controller validation error instead of cleanly queueing the message for later delivery.

## Observed Symptom

Example observed error:

```text
Structured input controller validation error for thread 019dafa6-caef-7e50-bfad-323af643e3ce:
turn/started turn id `019dbdcd-09a9-70d1-83e9-1c54a7270fba`
did not match active turn id `019dbdcc-2d2a-76d3-b476-ae1b1682ea55`
```

The same session later shows:

```text
Queued structured input
↳ [AfterToolCall] <c2c event="message" ...>...</c2c>
```

So the fork clearly has a queueing model for structured input, but the injected message path is still able to trip active-turn validation instead of being consistently deferred.

## Practical User Impact

This breaks auto-delivery of inbound c2c messages into Codex when Codex is busy.

Behavior seen from the c2c side:

- inbound message arrives cleanly when it is already the agent's turn
- inbound message triggers active-turn validation noise when Codex is mid-turn

That makes sideband delivery unreliable precisely when external messaging matters most.

## Repro Shape

1. Launch normal Codex TUI with `--xml-input-fd <fd>`.
2. Start a non-trivial turn so Codex has an active turn in progress.
3. While that turn is still active, write a structured XML user message through the XML sideband:

```xml
<message type="user"><c2c event="message" from="peer" alias="peer">hello</c2c></message>
```

4. Observe the structured-input controller validation error instead of the message being cleanly queued.

## Expected Behavior

Sideband XML user messages delivered during an active turn should be queued for later execution using the same structured-input queue semantics the fork already documents, rather than causing active-turn validation errors.

At minimum, externally injected non-steer messages should have a well-defined queued behavior while another turn is active.

## Why This Looks Like A Codex-Fork Issue

The fork docs and code already point to queued structured input as a supported concept:

- `docs/x-client-changes.md` documents:
  - `<message type="user" queue="AfterToolCall">...`
  - `<message type="user">...`
- the docs also mention a visible "Queued structured input" section when `--xml-input-fd` is in use
- `turn-start-bridge-core/src/controller.rs` contains explicit active-turn validation logic and emits the same mismatch text seen live
- `turn-start-bridge/src/main.rs` and the controller code both reference active-turn-not-steerable and pending queue windows

Relevant files:

- `docs/x-client-changes.md`
- `codex-rs/turn-start-bridge/src/main.rs`
- `codex-rs/turn-start-bridge-core/src/controller.rs`
- `codex-rs/turn-start-bridge-core/src/message.rs`

## Current Suspicion

The likely issue is that externally injected sideband user messages are being handled as immediate turn-start requests even while another turn is active, instead of being normalized into queued structured input semantics.

Concretely, the path may require:

- automatic queueing for sideband user messages when a turn is already active, or
- explicit treatment of plain `<message type="user">...</message>` as queued/non-immediate under active-turn conditions, or
- a documented requirement that callers must send `queue="AfterToolCall"` whenever the session is busy

Right now the behavior appears inconsistent enough that an external harness cannot rely on it safely.

## Suggested Direction

One of these needs to become true and stable:

1. Plain sideband user messages are automatically queued if Codex is already in an active turn.
2. The fork explicitly requires a queue attribute for mid-turn structured input, and rejects only non-queued input with a clearer error.
3. The XML sideband path itself rewrites or routes non-steer mid-turn messages into the queued-input path.

## Notes From c2c Dogfooding

There was also a separate c2c-side bug where message text was over-escaped and rendered `&quot;` literally. That is independent and already identifiable on the c2c side.

This report is specifically about the active-turn structured-input validation failure that occurs when Codex receives inbound sideband XML while it is busy.
