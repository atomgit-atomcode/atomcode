# Background task permission inheritance implementation plan

**Goal:** New delegated tasks use the originating runtime's live permission mode; read-only reviews cannot execute the parent's follow-up work.

**Architecture:** Each CodingRuntime retains ownership of its own permissions. The CLI background coordinator reads the originating runtime through `mode()` and configures a fresh candidate through `set_mode()` before submitting work. No kernel, persisted session schema, or host wire changes are needed.

**Tech Stack:** Rust, existing CodingRuntime control commands, nextest headless TUI tests.

## Baseline and boundaries

Baseline: `a957c200d` on `release/v5.2.7`. Existing unrelated changes in CLI main/headless_json are preserved. The spawn factory rereads disk configuration but cannot see interactive mode changes. `start_for` also forwards the original task as an assignment, allowing a review wrapper to attempt edits or PR creation. The embedded reviewer itself is already read-only; the background wrapper is the missing boundary.

## Implementation

1. In `crates/atomcode-cli/src/background.rs`, snapshot the parent's live mode before creating a delegated task. Apply it before slot publication or first submit. Retire the candidate on configuration failure. Existing background tasks keep their modes.
2. For scope-tagged/code-review jobs, use Plan instead of inherited Auto. Keep the original request as explicitly labeled context and assign review/report only. Follow-up edits, commit, push and PR creation belong to the originating conversation.
3. In `crates/atomcode-cli/tests/tui_bg.rs`, verify all four modes through real runtime/host calls and verify read-only review override. Confirm the inherited mode remains unchanged after the parent switches mode. The existing mode display on foreground navigation reads that task's actual runtime; no second cached permission field is introduced.
4. Run relevant CLI background tests, formatting, and `bash gates/compile.sh`. Inspect the final diff and preserve unrelated work.

## Failure semantics and scope

A failed mode read does not spawn; a failed candidate mode change closes it and reports the error. No fallback to config/default permissions is permitted. The coordinator operation lock keeps session navigation from replacing the originating slot during launch. `/bg` parking an existing runtime does not change its mode. This does not synchronize modes across existing tasks or change team-member policies.

## Verification results

- New mode inheritance regression passes with a paused current-thread Tokio runtime.
- Background suite: 58 pass; `shared_navigation_to_a_missing_project_keeps_the_old_foreground` fails unchanged on clean baseline `a957c200d`.
- CLI package: 629 pass, excluding that baseline failure and `a_turn_stopped_before_any_answer_can_be_undone_by_its_number`, which also fails on the same clean baseline. Neither unrelated failure is changed here.
- `bash gates/compile.sh`, `cargo fmt --all -- --check`, and `git diff --check` pass.
- Tests use real local runtimes and scripted providers; no external AtomGit PR is created.
