# Structured Input Queue Syntax Is Too Easy To Misuse From External Sideband Clients

## Summary

When integrating an external sideband delivery system with Codex TUI `--xml-input-fd`, the queueing syntax is currently too subtle and too weakly explained for reliable third-party use.

In practice, the difference between:

- `<message type="user">...</message>`
- `<message type="user" queue="AfterToolCall">...</message>`
- `<message type="user" queue="AfterAnyItem">...</message>`

has major behavioral consequences, but the failure mode is not obvious from the UI and the documentation does not make the operational differences clear enough for harness authors.

This makes structured-input integration feel fragile even when the underlying controller is doing something coherent.

## What We Observed

While integrating c2c with Codex TUI sideband XML input, we hit several confusing states that looked like parser/controller bugs from the outside:

1. Plain user messages during active turns could trip active-turn validation errors.
2. Queued messages appeared in the "Queued structured input" UI, but it was initially unclear why they did or did not release.
3. `AfterAnyItem` eventually turned out to be the right queue mode for our inbound broker messages, but that was discovered by code-reading and dogfooding rather than from the docs or diagnostics.
4. Before a clean restart into the updated delivery path, old queued items continued to look "stuck", which made it even harder to distinguish:
   - bad XML syntax
   - wrong queue mode
   - stale session state
   - active-turn mismatch
   - missing release signal

## The Important Syntax Difference

The key behavior difference for us was the queue attribute:

```xml
<message type="user"><c2c>hello</c2c></message>
```

versus:

```xml
<message type="user" queue="AfterAnyItem"><c2c>hello</c2c></message>
```

and:

```xml
<message type="user" queue="AfterToolCall"><c2c>hello</c2c></message>
```

From the outside, these look like small syntax variations. In reality they materially change whether:

- the message starts immediately
- the message queues
- the message waits for a tool boundary
- the message waits for an item-completed signal that releases `AfterAnyItem`
- the message risks tripping active-turn validation if used at the wrong time

That is a lot of behavioral surface area to pack into one optional attribute without stronger guidance and diagnostics.

## Why This Is A UX Problem Even If The Controller Is Correct

After restarting into the corrected c2c delivery path, `queue="AfterAnyItem"` did work for our sideband self-delivery tests and drained correctly.

So this report is not claiming the controller is necessarily wrong.

The issue is that the integration experience is still too error-prone because:

- the correct queue mode is not obvious
- the release conditions for `AfterAnyItem` are not obvious
- plain `<message type="user">` looks valid but may be the wrong choice for active-turn injection
- when queued messages do not drain quickly, the UI does not explain what release condition they are still waiting on

For third-party harnesses, this feels like a syntax footgun.

## Suggested Improvements

### 1. Document queue modes operationally, not just syntactically

`docs/x-client-changes.md` should explain, in plain operator language:

- when to use plain `<message type="user">`
- when to use `queue="AfterToolCall"`
- when to use `queue="AfterAnyItem"`
- what exact events release each queue mode
- which option external delivery systems should prefer for unsolicited inbound messages

Right now the docs show examples, but not enough decision guidance.

### 2. Make queued-input diagnostics more explicit

When a message is shown under "Queued structured input", the UI should ideally expose why it is still queued, for example:

- waiting for tool-call completion
- waiting for any releasable item completion
- waiting for current active turn to complete
- blocked by pending steer turn

That would turn a mysterious stuck queue into an understandable state machine.

### 3. Improve validation errors for wrong queue mode at the wrong time

If a plain `<message type="user">` arrives during an active turn and Codex thinks it should have been queued, the error should say that directly.

For example, something like:

> structured input arrived during active turn; consider `queue="AfterAnyItem"` or `queue="AfterToolCall"`

That is much more actionable than an active-turn mismatch with no queue guidance.

### 4. Consider a safer default for sideband external input

If feasible, Codex could treat plain sideband XML user messages as a safer queued mode by default when a turn is already active, instead of treating them like eager immediate input.

Even if that is not the desired semantics for all clients, it would be worth considering for `--xml-input-fd` specifically, because that surface is especially likely to be driven by external automation.

### 5. Accept more forgiving queue syntax aliases

The parser already accepts both canonical and kebab-case variants in some places. It may be worth explicitly documenting or expanding accepted spellings so external clients are less likely to fail on cosmetic queue-value differences.

Examples:

- `AfterAnyItem`
- `after-any-item`
- `AfterToolCall`
- `after-tool-call`

If this is already supported, the docs should say so clearly.

## Practical Recommendation For Harness Authors

Based on dogfooding, unsolicited inbound sideband messages from external systems like c2c should prefer:

```xml
<message type="user" queue="AfterAnyItem">...</message>
```

rather than plain `<message type="user">...</message>`.

But that recommendation currently has to be reverse-engineered from source and behavior. It should be documented explicitly.

## Relevant Files

- `docs/x-client-changes.md`
- `codex-rs/tui/src/structured_input/mod.rs`
- `codex-rs/tui/src/bottom_pane/pending_input_preview.rs`
- `codex-rs/turn-start-bridge-core/src/controller.rs`
- `codex-rs/turn-start-bridge-core/src/queue_mode.rs`

## Bottom Line

The queue syntax itself is not huge, but the behavioral consequences are.

For external sideband integrations, the current experience is too easy to misread:

- valid-looking XML can still be operationally wrong
- the UI does not explain queue release conditions well enough
- the docs do not clearly say which queue mode an external delivery system should use

Even if the controller logic is mostly correct, Codex should make this surface much more forgiving or much more explicit.
