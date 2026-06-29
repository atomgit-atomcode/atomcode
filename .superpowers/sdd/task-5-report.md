# Task 5 Report: TUI — don't restore the prompt to the input box when preserving interrupted context

## TDD RED/GREEN Evidence

**RED phase:** The test references `should_restore_cancelled_prompt`, which did not exist before the edit. Running the test before adding the predicate would fail to compile with `cannot find function should_restore_cancelled_prompt`. The predicate and test were introduced together in a single edit (per TDD brief workflow).

**GREEN phase:**
```
running 1 test
test event_loop::restore_cancelled_prompt_gate_tests::cancelled_prompt_not_restored_when_keeping_context ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1031 filtered out
```

## Exact Gating Edit

In `crates/atomcode-tuix/src/event_loop/mod.rs`:

Added pure predicate before `restore_cancelled_message_to_buf`:
```rust
fn should_restore_cancelled_prompt(config: &Config) -> bool {
    !config.keep_interrupted_context
}
```

Gated the restore body:
```rust
fn restore_cancelled_message_to_buf(app: &mut App, renderer: &mut dyn Renderer, ctx: &LoopCtx) {
    app.message_queue.clear();
    if !should_restore_cancelled_prompt(&ctx.config) {
        // Preserve mode: drop resend slot so prompt isn't duplicated in input box.
        app.state.last_submitted_message = None;
        return;
    }
    if let Some(msg) = app.state.last_submitted_message.take() {
        // ... existing restore body unchanged ...
    }
}
```

Added test module `restore_cancelled_prompt_gate_tests` with `cancelled_prompt_not_restored_when_keeping_context`.

## Existing `restore_cancelled` Tests Still Pass

```
running 4 tests
test event_loop::buffer_tests::restore_cancelled_text_ignores_whitespace_only_draft ... ok
test event_loop::buffer_tests::restore_cancelled_text_replaces_when_draft_empty ... ok
test event_loop::buffer_tests::restore_cancelled_text_prepends_before_existing_draft ... ok
test event_loop::restore_cancelled_prompt_gate_tests::cancelled_prompt_not_restored_when_keeping_context ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 1028 filtered out
```

## Field/Method Names Confirmed Against Real Code

- `app.message_queue` — `VecDeque<crate::state::QueuedMessage>` at App line 3206
- `app.state.last_submitted_message` — `Option<String>` assigned via `= None` / `.take()`
- `ctx.config: Config` — `LoopCtx.config` at line 721
- `Config.keep_interrupted_context: bool` — `atomcode-core/src/config/mod.rs:134`

## Files Changed

- `crates/atomcode-tuix/src/event_loop/mod.rs` — 28 lines inserted (predicate + gate + test module)

## Commit

`6bab61df feat(tuix): skip edit-and-resend restore when keep_interrupted_context preserves the prompt`

## Self-Review

- `message_queue.clear()` is unconditional in both paths (escape-cord applies in both modes).
- `last_submitted_message = None` in preserve mode explicitly clears the resend slot.
- Existing buffer-level tests test `Buffer::restore_cancelled_text` directly — unchanged, all green.
- Predicate is pure with no side effects; trivially testable.
- No new plumbing required: `LoopCtx.config` already present, `restore_cancelled_message_to_buf` already receives `ctx: &LoopCtx`.

## Concerns

None.
