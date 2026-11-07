# Team scope refusal notice implementation plan

**Goal:** Make actual team write-boundary refusals visible to the lead and instruct members to report incomplete work instead of silently skipping it.
**Architecture:** DelegationBounds emits an ephemeral, parameter-free refusal signal. The existing team owner routes a fixed notice through the existing peer inbox, once per member turn. No kernel facts, persistence formats, permissions, approval behavior, sibling communication or task-state registry change.
**Tech Stack:** Rust, plexus typed events, existing replay-provider team tests.

Baseline: release/v5.2.7 at 3963b9646. Work in /tmp/atomcode-team-scope-notice; preserve unrelated login/quota/runtime changes.

1. Test that denied writes remain denied, lead hears a sanitized notification even when member reports completion, repeated refusals deduplicate, and a later turn can notify again.
2. Emit only for explicit delegated write violations, after the sensitive-path guard. Signal contains session id and turn only, never tool arguments or refusal text.
3. Team owns dedup under its existing member record; removed with the member. Notification is not a completion report and does not suppress existing end-of-turn reporting. Respect person-started member turn routing (note rather than automatic lead wakeup).
4. Member prompt: report paths, reasons and remaining work; never assume another member owns an omitted item. Lead guidance: verify task coverage, follow up or explicitly reassign; never bypass permission restrictions.
5. Run affected team/policy tests and crate checks, compile gate, format and lock checks. Pure semantic skips remain undetectable unless reported. No commit or push without user instruction.

Implementation finding: the new multi-tool member scenario exposed that Parallel used the root plugin context for execution. A batch containing `tell_parent` incorrectly returned unknown tool, while single-call rounds worked. Resolve both scheduling claims and execution through the currently scoped agent; add multi-call regression coverage and recheck scheduling tests. This restores the existing member catalog boundary rather than granting new tools.

Validation:
- coding team/concurrency/gate_order: 54 passed, 1 opt-in test skipped.
- harness tool_policy/composition/seam_convention: 36 passed.
- coding tool_intent/task_steering: 9 passed.
- harness subagent: 16 passed.
- `bash gates/compile.sh`: passed, including workspace all-targets and feature-gated test compilation and gate self-tests.
- New tests use paused Tokio time and replay providers, with no real model calls. Covered true write rejection, same-turn dedup, subsequent-turn re-notification, simultaneous tell_parent and refused writes, member tool-catalog isolation, sensitive-only denials, allowed writes, and person-started member turn routing.
- Cargo.lock unchanged; forbidden package names absent. No full workspace test run; focused regressions cover changed scheduling and team paths.
