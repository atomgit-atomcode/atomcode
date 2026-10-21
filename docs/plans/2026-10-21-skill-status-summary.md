# Compact skill status

`/status` and the Status tab show one Skills section with loaded/directory counts, nonzero override/failure counts, and a `/skills` detail hint. They do not expand skill paths or diagnostic reasons. Instruction/config/memory sources retain their existing rendering.

The host owns the summary and detailed source records. `SourceGroup.summary` is optional, omitted when absent, and defaults to None for older wire replies. The UI renders the summary when provided; detailed source data remains available. Bare `/skills` lists effective paths, scanned roots, and all diagnostics, including when no skill loaded. Invocations with arguments keep their existing behavior.

The registry suppresses override diagnostics for identical loaded definitions (description, template, allowed tools, invocation visibility). Directory precedence is unchanged: the higher-priority source still wins. Different definitions and genuine failures remain observable. Comparison uses loaded values, avoiding a second disk read that could disagree with the loaded snapshot.

Verification covers quiet duplicates and changed metadata, compact text/screen rendering, host counts with detailed data retained, failures-only skill listings, and old source wire replies.

Per-crate verification passed: capabilities with skills feature 1157, TUI 1794, harness 265, CLI 590, host API 10, i18n 73. One intermediate CLI run hit the existing background-session exit timing test; a subsequent full CLI run passed. Translation headings are owned by the product table and forwarded by the screen table to avoid duplicate strings.

Final compile gate, formatting, and diff checks passed. Cargo.lock is unchanged.
