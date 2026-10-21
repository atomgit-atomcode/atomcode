# Conservative coding prompt deduplication

Scope: Shorten the persona's skill guidance without removing exact-name matching, before-work/before-clarification priority, announcements, minimal selection, empty-catalog fallback or selectable interview options. Keep both the catalog guidance and model-specific skill reminders.

Consolidate the identical open_file selection rule under OPENING FILES, retaining the explicit-user-request requirement and forbidden shell alternatives. Remove only the tool-parallelism latency rationale, not the parallel/sequential requirements or examples. Shorten OUTPUT prose while preserving phase updates, thorough explanations, plan capture, Markdown tables and matching the user's language.

Leave PROGRESS SIGNPOSTS, polling, workflow, validation, permissions, content safety, precedence and all model-specific reinforcement untouched. This is intentionally a small text reduction rather than an aggressive rewrite. Use existing persona regression tests and coding crate tests; run formatting and the compile gate. No provider/config/session/tool availability changes. Request-profile work is tracked separately.

Measured rendered constants: SKILLS_USAGE 1,066 → 777 characters; RULES 14,990 → 14,547 characters. Total reduction: 732 characters, not a measured tokenizer count. Model-specific reinforcement and safety/polling/asking-user blocks remain byte-identical.
