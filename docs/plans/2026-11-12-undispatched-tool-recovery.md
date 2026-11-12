# Proven-undispatched tool stream recovery

Goal: recover typed transport/idle failures after local tool fragments only when the request collector proves no local call from the failed attempt reached execution.

Baseline: release/v5.2.7, 44ebf7392. Preserve concurrent reasoning-budget/provider edits, including agent_loop.rs. Session log and existing stream-recovery row remain the sole continuation owners.

- Add explicit attempt-local dispatch proof to RequestError; unknown by default. A partial response or tool fragment alone never grants proof.
- The built-in provider stream collector supplies NotDispatched: ToolExec happens only after AgentRequest succeeds. Do not change generic request retry or reasoning timeout policy.
- Existing stream recovery may continue proven-undispatched attempts, including tool-only output, under its existing shared attempt/wait and recovery limits. Preserve safe text, omit invalid tool calls from history, and distinguish this continuation from confirmed earlier results.
- Unknown/executing calls, authentication/logical failures, cancellation and exhausted budgets remain terminal. Prior successful mutations must never be replayed by the host.
- Verify emitted complete calls and fragments, text plus calls, prior successful calls, no-progress tool-only recovery, unknown proof, non-transport failures, budget exhaustion and cancellation during backoff. Run affected crate tests and compile/format gates; do not modify other in-flight work.
