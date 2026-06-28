# Task 5 Report: core TurnEvent::RateLimited + bridge KEv mapping

## Files Modified

### Core changes (new variants added)

1. **`crates/atomcode-core/src/turn/event.rs`** — Added `TurnEvent::RateLimited { reset_at_display, reset_label, secs_until_reset }` after `Warning(String)` (line ~68).

2. **`crates/atomcode-core/src/agent/mod.rs`** — Added `AgentEvent::RateLimited { reset_at_display, reset_label, secs_until_reset }` after `Warning(String)` (line ~328), because `CoreEv` in bridge is aliased to `atomcode_core::agent::AgentEvent`, not `TurnEvent`. Both types needed the variant.

### Bridge changes

3. **`crates/atomcode-bridge/src/runtime.rs`** — Added `KEv::RateLimited` arm in `on_kernel_event` (line ~1519), mapping kernel event to `CoreEv::RateLimited`. Also added the TDD test module `ratelimited_mapping_tests::ratelimited_event_variant_exists`.

### Downstream placeholder arms (exhaustive matches — Task 6/7 to replace)

| File | Line (approx) | Placeholder | Owner task |
|------|--------------|-------------|-----------|
| `crates/atomcode-core/src/agent/tool_dispatch.rs` | ~263 | `TurnEvent::RateLimited { .. } => {}` | Task 7 |
| `crates/atomcode-core/src/agent/mod.rs` | ~2813 | `TurnEvent::RateLimited { .. } => {}` (inside run_turn_loop's `match event`) | Task 7 |
| `crates/atomcode-tuix/src/event_loop/live_sync.rs` | ~43 | `\| TurnEvent::RateLimited { .. } => return None` (joined the "ignore" arm) | Task 7 |
| `crates/atomcode-tuix/src/event_loop/mod.rs` | ~9092 | `AgentEvent::RateLimited { .. } => {}` (end of `handle_agent_event`) | Task 7 |
| `crates/atomcode-daemon/src/live_api.rs` | ~1395 | `\| TE::RateLimited { .. } => return None` (joined "ignore" arm in `turn_event_to_wire`) | Task 6 |
| `crates/atomcode-daemon/src/lib.rs` | ~2915 | `TurnEvent::RateLimited { .. } => {}` (end of `/chat` handler loop) | Task 6 |

## Observation: Brief Alias Discrepancy

The brief stated `CoreEv` = `atomcode_core::turn::event::TurnEvent`. This is wrong — actual code uses:
```rust
use atomcode_core::agent::{AgentClient, AgentCommand as CoreCmd, AgentEvent as CoreEv, ...};
```
`CoreEv` = `atomcode_core::agent::AgentEvent`. Adding the variant to `TurnEvent` alone was insufficient; `AgentEvent` needed it too.

## TDD Test Result

```
CARGO_INCREMENTAL=0 cargo test -p atomcode-bridge ratelimited_event_variant
test runtime::ratelimited_mapping_tests::ratelimited_event_variant_exists ... ok
1 passed; 0 failed
```

## Compilation (all packages)

```
CARGO_INCREMENTAL=0 cargo build -p atomcode-core -p atomcode-bridge -p atomcode-daemon -p atomcode-tuix
Finished `dev` profile [unoptimized + debuginfo] target(s) in 16.51s
```

## Pre-existing Red Test

`atomcode-core` has one pre-existing failing test `agent::classifier_tests::invalid_request_is_summarized_without_raw_body` — unrelated to this task.

## Commit

Auto-committed by turn hook: `d647b680` "Auto-commit at turn #1 (11 files changed)"
(Also includes Task 1-4 kernel work from prior sessions.)
