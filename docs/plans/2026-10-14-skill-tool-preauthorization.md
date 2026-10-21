# Skill tool preauthorization implementation plan

**Goal:** Honor skill `allowed-tools` as turn-scoped Claude Code compatible preauthorization, without changing the available tool set or bypassing security boundaries.

**Architecture:** Capabilities parse list and scalar declarations, preserving command patterns. A harness row owns ephemeral grants per session and turn, using existing input, session and tool execution seams. It runs after static permission rules and before convenience approval gates; the existing runtime remains the lifecycle owner. Grants are never persisted or restored.

**Tech stack:** Rust, harness plexus rows, existing `PermissionRules` matcher.

1. Parse block lists, inline lists and scalar lists without splitting spaces inside command patterns; test quoted entries and malformed declarations.
2. Resolve the calling session through the existing harness agent task-local, including the parallel scheduler, to isolate grants without changing tool protocols.
3. Add the skill permission row. Clear on new user input, turn termination and agent removal. Activate slash invocations from committed skill injections and `use_skill` only after successful execution. Invalid rules refuse activation; static deny and hard gates remain authoritative.
4. Verify both invocation paths, matching and unmatched calls, deny precedence, sensitive targets, cancellation, new turns and independent agents. Verify malformed rules fail closed.
5. Describe behavior in the owning row and the Chinese/English Skills pages. Clarify that Bash authorization is not a filesystem/network sandbox and hooks run separately.
6. Run affected crate tests, formatting and `gates/compile.sh`. Preserve unrelated CLI edits and exclude private lockfile dependencies.

## Completed verification

- Capabilities with `skills`: 1,105 tests passed; final parser checks passed after tightening malformed inline-list handling.
- Harness: 240 tests passed; the agent-isolation/history-resume criterion was also rerun after strengthening its refusal assertion.
- Coding: 987 tests passed in the crate regression; the final skill-specific suite has 12 passing end-to-end criteria.
- The final workspace and feature-gated compile gate, formatting check and diff check passed. Documentation search indexes were regenerated.
- No new dependencies, persistent event variants, credentials in row configuration, or runtime owners were introduced. Temporary grants use the existing agent context and disappear with their row/tree.
- Installation commands were exercised through a test substitute; no Python packages or hardware hooks were executed.
