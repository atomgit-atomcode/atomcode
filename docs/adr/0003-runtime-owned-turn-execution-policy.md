# Runtime-owned per-turn execution policy

## Context

The coding persona and VerifyCadence previously encouraged verification even when the real user
explicitly prohibited compiling, testing, or executing scripts. Prompt wording alone is not an
execution boundary. A main agent can also delegate to a worker whose independent tool stack runs
with automatic approval, so gating only the main `bash` leaves a bypass.

## Decision

`CodingRuntime` owns one per-turn execution-policy handle. A real user Submit or steer replaces its
state immediately; the lifecycle hook re-derives the same state from the latest non-synthetic user
message after resume, compaction, or reassembly. Synthetic reminders never acquire authority.

The handle is installed before approval as middleware on the main agent and inherited by worker
subagents. `atomcode-capabilities::TaskTool` only transports generic worker middleware and remains
unaware of coding policy. Read-only explore subagents do not receive it because they mount no shell
or write tools.

Build, test, script, and all-shell restrictions are independent flags. Bash syntax is parsed once by
the capabilities layer with tree-sitter; it exposes neutral command invocations, while the coding
layer assigns product semantics. An incomplete parse fails closed under an active restriction.

## Consequences

- user restrictions apply consistently across main and worker execution;
- a test-only restriction does not unnecessarily block compilation, and vice versa;
- common shell wrappers, nested command substitutions, quoted separators, and Windows executable
  forms are classified from syntax rather than a second hand-written shell parser;
- natural-language detection remains a convenience interface, not a complete language parser;
  quoted examples are ignored and explicit structured controls can be added later without changing
  the runtime/middleware ownership boundary;
- a process that crossed middleware before a steer cannot be retroactively prevented from starting;
  normal cancellation remains the mechanism for already-running work.
