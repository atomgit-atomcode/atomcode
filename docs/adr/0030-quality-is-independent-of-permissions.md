# 0030: Quality policy is independent of execution permissions

Date: 2026-11-09
Status: Implemented, foreground-task MVP

## Context

Build, AcceptEdits, Auto and Plan control execution authority. They do not define a project verification contract. The existing verify cadence is a reminder, not a deterministic gate. A higher-quality persona or an LLM reviewer cannot prove that a configured command actually exited successfully.

## Decision

- Keep RuntimeMode unchanged. Add opt-in `[quality]` and the human `/quality` capability command. CodingParts owns session policy; ordered runtime controls fence changes by generation and session. New sessions reset policy; same-session rebuilds retain explicit on/off choices, while reprepare adopts new gate definitions.
- A coding-owned row wraps the existing `agent/request` seam. At normal handoff after changes, it schedules explicitly configured checks as ordinary shell tool calls, sequentially. Approvals, sensitive boundaries, cancellation, timeouts and native tool facts remain in the existing execution pipeline.
- The reusable shell capability exposes a read-only typed terminal receipt, forwarded by the harness. This is necessary because a nonzero shell exit is intentionally represented in-band rather than by ToolResult.is_error. Receipt observation never authorizes execution. No receipt means no pass.
- A pass requires matching command and tool-start facts, zero actual exits, successful final tool results, unchanged file fingerprints during checks and successful evidence persistence. New user input supersedes a verification attempt and must reach the model before fresh checks run.
- `turn/finishing` only records interrupted/incomplete attempts. No second agent owner, terminal continuation hook, kernel quality mode or alternate session writer is added.
- Store private, atomic JSON evidence under ProductDirs.user()/quality-evidence/, independently of the authoritative session event log. Evidence is an audit artifact, not a source from which conversation state is reconstructed. Restore reads the last record; status revalidates its file and gate fingerprints.
- Local policy failures use the existing PolicyDenied terminal, not a fabricated provider error. Failures stop and retain files. `/quality waive <reason>` records a separate human waiver and preserves the original failure record. A waiver is never a pass and does not disable future gates. Do not automatically reset, commit or tag a user's checkout.

## MVP limits

Only foreground ordinary work in a local Git worktree with an initial commit is supported. Goal/loop start is refused under quality; delegation and background starts are unavailable. Existing delegated agents must be stopped. Shell commands can still affect ignored files, external services or launch external processes: this policy is verification, not a filesystem sandbox or proof of semantic correctness.

Fingerprints cover tracked file contents and modes plus nonignored untracked files, including the dirty starting state. They exclude ignored artifacts and Git metadata. Submodules, non-UTF-8 paths and oversized snapshots are refused. Git snapshot calls disable fsmonitor, clean/process filters, textconv and external diff helpers. Evidence storage must be outside the worktree so evidence writes do not invalidate themselves.

Each check has a 1–300 second foreground timeout. Automatic command discovery, affected-test inference, coverage/performance certification, retention/deletion policy, background/controller integration and remote evidence storage are deferred.

## Consequences

The host can distinguish passed, failed, cancelled, stale, unverified, unchanged and explicitly waived results. Verification does not spend an extra model call to assess its own output. Commands still have the ambient effects allowed by their approval policy; users remain responsible for selecting meaningful checks. Evidence may contain private diffs and command output and must be reviewed before sharing.
