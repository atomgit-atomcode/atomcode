# Context Accounting and Background Waits Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Correlate request size, usage and durable cleanup; notify on explicitly awaited background exits; support an optional absolute automatic compaction budget.

**Architecture:** CodingRuntime remains the single owner of background continuation and goal waiting. Capabilities exposes a non-consuming terminal observation of existing jobs; runtime owns futures and fences them by generation/cancel. Existing compaction rows and strategy perform all history changes; no new event format or compaction state machine. Diagnostics contain sizes and identifiers only.

**Tech Stack:** Rust, Tokio, tracing, kernel session events, cargo nextest.

Baseline: release/v5.2.5 at 344df967291a9701756921f7fa7436832d42951d; tracked tree clean, unrelated untracked artifacts preserved.

## Task 1: Correlated diagnostics

Modify harness agent_loop request/stream spans and coding context_maintenance logging. A request span encloses middleware and provider size logs; provider attempts record a stable logged-response ordinal. Emit final usage/outcome without message content. Cleanup diagnostics include session/turn/round and the durable event boundary. Verify tracing tests and existing provider stream tests.

## Task 2: Explicit background completion

Modify capabilities bash background observation and tool schema, add coding background job bookkeeping, and integrate it in runtime's existing event/select loop. Only bash_start calls with notify_on_exit=true are observed. Observe tracked-shell exit and inherited output-pipe closure without consuming poll output. Runtime defers goal continuation while explicit jobs remain, delivers a synthetic completion on exit, and fences replacement/cancel/shutdown. Add virtual-time observation tests and runtime integration cases. A job that cannot be observed falls back to ordinary polling rather than a permanent wait. Repeated identical tool result evidence is not progress; changed evidence and mutations remain eligible.

## Task 3: Absolute compaction budget

Modify CodingAgentConfig, on_harness patch, host compaction row and existing strategy policy. Optional token limit defaults to None. It lowers the automatic trigger and enables existing summary escalation, with a smaller retained recent budget; physical context-window reporting remains unchanged. Validate invalid row limits; retain sacred floor, signed blocks, pairing and net-size guards. Add tests for below-budget no-op, fixed-budget summary eligibility, bounded retention and unchanged defaults.

## Verification

Run focused nextest cases, affected crate suites, runtime criteria/differential tests, bash gates/compile.sh, cargo fmt --all and --check, git diff --check. Check Cargo.lock prohibited packages. Do not re-record goldens, modify unrelated files, commit or push without the user's next audit instruction.

## Implemented behavior and validation

- Request diagnostics now correlate session/parent/inherited-event count, turn and round across middleware, wire-size profiling and final usage/outcome. Provider spans carry the logged-response ordinal. Cleanup and compaction diagnostics name event boundaries; only sizes/identifiers are added, not prompt bodies. Metadata lookups avoid cloning the whole event log.
- `bash_start` accepts optional `notify_on_exit: true` (default false). A supporting CodingRuntime observes the tracked shell's exit and inherited output-pipe closure, then resumes the same session with an instruction to collect output using `bash_poll`. Observation does not consume output. Goal continuation waits for explicitly observed work; cancel, replacement and budget expiry retire observers. `/loop` notifications ride the next scheduled input and do not shorten the chosen cadence. Servers/watchers should keep the default manual-poll mode.
- Goal progress conservatively discounts identical historical receipts for known observational tools (`bash_poll`, reads and searches). Changed receipts and successful mutations remain eligible; arbitrary shell commands are not assumed read-only. This is activity evidence, not a proof that a business requirement was satisfied.
- The real config-file boundary and generic settings catalog support optional `[compaction].token_limit` (a positive u32). CLI/shared construction and the daemon's explicit construction carry it into the existing compaction row. Omission/reset preserves ratio-based defaults. The trigger and retention budget honor the configured value without changing the provider's physical window. Existing anti-thrash estimates now include tool arguments/images instead of text alone. Protected history can prevent reducing below this trigger; it is not a hard request-size cap.

Example configuration (optional, no user configuration was changed):

```toml
[compaction]
token_limit = 200000
```

Validation: config library 326 passed; capabilities library 1,073 passed; harness suite 271 passed; coding library 619 passed (8 skipped); runtime_criteria/differential/mount_wiring/verify_cadence/cache_prefix 194 passed; daemon live_api 37 passed. The final background/verification regression target passed 36 cases, including the subsequently added loop-cadence case. Final `bash gates/compile.sh` passed both compilation stages and negative controls. Golden files and Cargo.lock are unchanged; prohibited lock packages are absent.

Limits: no live-provider or paid-call comparison was run, and the old session is a baseline rather than evidence of realized savings. Process restart does not resurrect in-memory background processes/observers; retired notifications fall back to the existing poll contract. Progress detection does not attempt to infer side effects of unknown shell commands.

Audit follow-up: policy interventions retire background observers and block automatic completion prompts. Completion delivery follows queued owner controls and agent events in the biased select, so queued cancellation/replacement/policy events win. The expanded notification regression covers policy blocking; all 619 coding library tests and the compile gate passed again after this fix. Formatting and diff checks passed.
