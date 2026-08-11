# First-stage planning quality design

## Scope

Improve the planning floor for weaker models without introducing a plan artifact,
approval transition, new protocol, or second runtime owner. Todo remains execution
tracking; the existing coding runtime and tool lifecycle remain authoritative.

## Behavior

- The system prompt and `todowrite` description define a useful initial list as the
  complete work surface: investigation, architecture/module design, implementation,
  and verification where relevant. Items must describe concrete outcomes that a later
  turn can execute without re-planning.
- DeepSeek under `tools.todo.eager = "auto"` keeps the normal preferred reminder, but
  high-confidence architecture, refactor, migration, redesign, and feature-development
  requests force `todowrite` as the first tool. Simple and informational turns remain
  judgment-based, including requests that explicitly prohibit modification or ask for
  read-only explanation. A scoped read-only clause does not suppress planning when the
  same request explicitly asks for a complex code change. Providers that cannot express
  forced tool choice retain only the reminder.
- The todo parser rejects only unmistakable placeholder labels such as `task 1`,
  `step 2`, `阶段3`, and bare `处理功能`. It does not attempt semantic scoring or require
  every task to use fixed phase names. This quality gate applies to newly executed
  full plans and incremental additions; persisted transcript replay retains the older
  structural parser so existing sessions do not lose their todo state. Failed current
  calls are excluded from transcript-derived state by their tool-result correlation id.
  Daemon and TUI `/todo`, session replay, and runtime reminders all use that same
  result-aware projection. The TUI stages live mutations at call start and commits them
  only after a successful matching tool result, so rejected calls cannot flash or persist
  as current work.
- Before the skill catalog's existing byte-budget truncation, installed skill names
  explicitly referenced at token boundaries in the effective instruction tiers are
  promoted. Matching is exact and case-insensitive; the runtime never guesses a skill
  from generic workflow prose.

## Ownership and failure semantics

Planning policy stays in `atomcode-coding`; todo argument validation and skill catalog
rendering stay in `atomcode-capabilities`. Project-instruction precedence is read through
the existing `SessionContextHook`, so catalog ranking cannot invent a parallel loader.
Rejected placeholder todos return a normal tool error for correction. Missing or
unresolvable skill names do not change existing behavior.

## Verification

- DeepSeek complex requests force `todowrite`; simple requests do not.
- Mixed requests with a scoped read-only clause still force planning for an explicit
  complex code change.
- Placeholder-only todo labels fail while specific numbered tasks remain valid.
- Failed todo calls do not alter daemon/TUI command output or the live TUI panel.
- An explicitly named low-source-rank skill sorts ahead of an unreferenced native skill.
- Instruction-name matching rejects prefixes and suffix collisions.
- Run the affected `atomcode-capabilities` and `atomcode-coding` library tests, followed
  by the CLI/TUI/daemon compile checks because all production drivers share assembly.
