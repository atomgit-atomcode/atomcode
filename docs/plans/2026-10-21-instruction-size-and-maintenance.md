# Instruction size and maintenance implementation plan

Goal: prevent `/init` from encouraging unbounded instruction history, expose large loaded instruction bodies, and move this repository's historical verification evidence out of its resident instructions without dropping active constraints.

Baseline: `release/v5.2.4` at `821a2f337`; pending MCP search-limit changes and unrelated untracked files remain untouched. `LoadedInstructions` owns the frozen instruction text; `parts::prepare` already forwards its warnings through the runtime to all coding drivers. No protocol, persistence, lifecycle, selection, or memory-budget changes.

1. Update both locale variants in `coding/src/init_prompt.rs`: maintain current facts by replacement, keep history in linked docs, preserve user rules and require explicit permission before moving/removing existing material. The new-file length target is not a deletion mandate for existing files.
2. Add a soft 16 KiB combined loaded-body notice in `capabilities/src/instructions.rs`, reusing `warnings()`. Report exact UTF-8 bytes, characters, and source paths without body contents or token claims. Ignore missing, empty, unreadable, and shadowed sources. Test combined budgets, UTF-8 accounting, preservation, and selection.
3. Preserve the previous verification section verbatim in `docs/development-verification-notes.md`; retain every active build/test/lock/fmt requirement in `AGENTS.md` and link evidence from its applicable subsections. Leave architecture and runtime constraints unchanged.
4. Document the soft notice. Run focused tests, then both affected crate suites sequentially; finish with formatting, compile gate, lockfile and diff checks. Do not change other users' files or global settings; do not submit or push without a request.
