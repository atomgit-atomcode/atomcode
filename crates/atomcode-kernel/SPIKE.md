# atomcode-kernel — Phase A0 Spike

Design-validation spike for the neutral-kernel platform strategy
(`docs/superpowers/specs/2026-06-05-atomcode-kernel-platform-strategy.md`).
Internals are minimal/throwaway — the public API *shape* is what Phase A1 carries
the proven production hot-paths into.

## What it proves

| Claim | Where |
|---|---|
| 1. Neutral kernel — a turn runs with no persona and no middleware | `tests/spike_claims.rs::neutral_turn_runs_without_persona_or_middleware` |
| 2. Approval is an external middleware over an id-correlated round-trip | `tests/spike_claims.rs::approval_middleware_gates_risky_tool_via_id_roundtrip` |
| 3. Selective tool mounting — unmounted tools invisible/inert | `src/tool.rs::tests::only_mounted_tools_are_exposed_or_resolvable` |
| 4. One primitive serves one-shot AND interactive drivers | `tests/...::one_shot_adapter_auto_answers_and_aggregates` + `examples/minimal_specialization.rs` |
| 5. Wire-compatible (serde round-trip) → web/daemon can use the same seam | `tests/...::events_and_commands_are_wire_serializable` |
| 6. LifecycleHooks — turn-level injection (turn_end continues loop) + TurnStarted observation | `tests/spike_claims.rs::lifecycle_hook_injects_and_continues_loop` |
| 7. Full LifecycleHooks surface — 8 turn-level points wired & fire | `tests/spike_claims.rs::lifecycle_hooks_complete_surface_all_fire` |
| 8. Execution-state recorded (Message.meta) + projected to LLM (tail reminder) + cache-safe | `tests/spike_claims.rs::execution_state_recorded_projected_to_llm_and_cache_safe` |
| 9. Round budget projected to LLM ("round X/Y") + hard cap, recorded in meta.round, cache-safe | `tests/spike_claims.rs::round_budget_projected_to_llm_and_hard_capped` |
| 10. on_model_response gets `&mut Message` — can transform the response; transform lands in storage (via Snapshot) | `tests/spike_claims.rs::on_model_response_can_transform_response_into_storage` |
| 11. on_model_response edits to tool_calls are honored (dropped call doesn't execute) | `tests/spike_claims.rs::dropping_tool_calls_in_on_model_response_prevents_execution` |
| 12. ToolMiddleware `before` rewrites/blocks (no ghost ToolStarted) + `after` transforms result | `tests/spike_claims.rs::tool_middleware_rewrites_blocks_and_transforms` |
| 13. Command-level approval — arg-aware `Tool::risk(args)`; ApprovalMiddleware gates dangerous commands, skips safe ones, caches session grants | `tests/spike_claims.rs::dangerous_command_requires_approval_safe_does_not_and_grant_is_cached` |
| 14. user_prompt_submit can block a prompt (Err → prompt rejected, no turn, not stored) | `tests/spike_claims.rs::user_prompt_submit_can_block_a_prompt` |

## Driver model

One primitive: a long-lived session consuming `AgentCommand` and emitting
`AgentEvent` (`AgentHandle`). The round-trip seam is the id-correlated
`AgentEvent::Request{id,kind,payload}` ↔ `AgentCommand::Respond{id,value}`. The
`oneshot` that resolves a middleware's await lives only in `RequestCtx` (kernel),
never in an event — so events/commands are serializable and work in-process AND
over the wire. `run_to_completion(input, policy)` is the one-shot adapter for
batch/CI. All four driver shapes (one-shot/CI, TUI, web, server) sit on this one
primitive:

| Driver | Command source | Event sink | Request answered by |
|---|---|---|---|
| one-shot / CI / CodeReview | one SendMessage | aggregated Outcome | AutoRespond policy |
| TUI | keypresses | render loop | modal → Respond |
| Web | WS/HTTP → AgentCommand | AgentEvent → SSE/WS | user → Respond frame |
| server / daemon | per-session RPC | per-session SSE | policy or remote user |

## Hook surface (perceive vs inject)

Two distinct mechanisms: **perceive** = the read-only `AgentEvent` stream
(observers cannot change the loop); **inject** = the `LifecycleHooks` trait
(runs inside the loop, can mutate/continue it). The trait declares 8 turn-level points — `session_start`, `user_prompt_submit`, `turn_start`, `pre_request`, `on_model_response`, `turn_end`, `on_error`, `session_end` — each wired into the loop (Claim 7 asserts every one fires). TOOL-level concerns (rewrite/gate/transform a tool call) live in the composable `ToolMiddleware` (`before` + `after`), NOT in LifecycleHooks. Out-of-process injection reuses the id-correlated
`Request`/`Respond` round-trip (a hook asks the remote driver and awaits).

Execution-state feedback follows the rule: RECORD at `on_model_response` (kernel-native `Message.meta` sidecar), PROJECT to the LLM at `pre_request` as a tail reminder — never mutating historical bytes (prefix-cache safe). Hooks needing loop position get a `TurnCtx { round, max_rounds }`; `pre_request` uses it to project round budget to the LLM, and the kernel hard-caps the loop at `max_rounds` as a safety fuse.

`on_model_response` receives `&mut Message` (the fully-built assistant message with kernel-filled `meta`). The hook may observe or TRANSFORM the response (e.g. redact text, truncate). `MessageMeta` holds only kernel-measured facts (`tokens`, `elapsed_ms`, `ctx_window`, `used_tokens`, `utilization`, `round`); cost and other specialization concerns live outside the kernel.

## Key boundary facts

- The kernel core (`agent.rs`, `event.rs`, `tool.rs`) never names "approval".
  Tools carry an arg-aware `risk(&str) -> RiskLevel` method (the tool itself knows
  which commands are dangerous); approval lives entirely in
  `testkit::ApprovalMiddleware` (specialization side) over `RequestCtx::request`.
  `ApprovalMiddleware` also holds a session-grant cache so identical dangerous
  commands are approved only once per session.
- `ToolContext` carries no semantic/graph/lsp services — the kernel needs none.
- Crate excluded from workspace `default-members`, so product builds are untouched.

## Run

    cargo test -p atomcode-kernel
    cargo run -p atomcode-kernel --example minimal_specialization

## Next (Phase A1)

Carry production hot-paths into these slots WITHOUT rewriting: `TurnRunner` loop →
`agent.rs`; `ctx/render` → a `CtxBuilder` impl behind the persona injection point;
`conversation` → `message.rs`; neutral provider impls → `provider.rs`. Preserve
prefix-cache invariants and existing edge-case fixes.
