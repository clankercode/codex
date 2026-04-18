# Primary Goal
Complete the last four `x-todo.txt` items from the version issue onward, with each item driven through implementation, verification, and review before moving to the next.

## Acceptance Criteria
- The CLI/TUI report a real non-`0.0.0` version and the stale-version test no longer special-cases `0.0.0`.
- Manual compaction can use a lighter model while restoring the session model afterward.
- Idle-time injection can be toggled live, defaults on, and the status line can display idle time since the last model turn.
- `/effort [off|low|medium|high|xhigh]` works live, including during assistant turns, and updates current-session model calls immediately.
- Each completed item is reviewed by a subagent and the overall batch ends with a `review-and-fix` pass.

## Current Status
- Iteration: 4
- Newly satisfied AC:
  - The version item now reports `0.122.0` in workspace metadata, lockfile, release packaging, and updated snapshots/tests.
  - Manual compaction can use a lighter model for the compaction turn and restore the primary session model afterward.
  - Idle-time injection can be toggled live, defaults on, and the status line can display idle time since the last model turn.
  - `/effort [off|low|medium|high|xhigh]` works live, including during assistant turns, and updates current-session model calls immediately.
  - Review surfaced and cleared the remaining in-scope gaps: direct/core idle-time injection now skips steer updates, first-turn idle injection is suppressed until a completed turn exists, and compaction resets the direct-mode idle timer.
  - The final `review-and-fix` pass returned `PASS`.
- Remaining AC: none.

## Current Plan
- The version-through-`/effort` batch remains complete.
- The follow-on `compact statusline for ctx etc` item is now complete as well.
- Next loop can move on to the remaining older unchecked `x-todo.txt` items.

## Blockers / Notes
- The user explicitly requested `ultra-goal-loop`, code review subagents, and a final `review-and-fix` loop.
- `git fetch --tags upstream` completed successfully, and `rust-v0.122.0` now exists locally as the target release version.
- Review also raised broader follow-up ideas that I did not treat as blockers for this batch: auto-compaction using the lighter model and richer replay-time idle/model reconstruction.
- The compact-statusline follow-up intentionally changed only rendered footer strings (`Ctx ...`, `... ctx`) and left `/statusline` item IDs/config compatibility unchanged.
