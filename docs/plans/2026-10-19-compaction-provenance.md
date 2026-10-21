# Compaction provenance Implementation Plan

**Goal:** Distinguish retained historical requests from new instructions and expose the full immutable history.

**Architecture:** The kernel event projection derives a retained-user origin from surviving Compacted ranges. Model projections frame only those historical messages; snapshot projections retain their original text. Existing message provenance crosses daemon APIs; WebUI labels retained messages and reads the existing transcript endpoint. The event log remains the sole authority and no disk schema changes are required.

**Tech Stack:** Rust kernel/capabilities/harness/daemon, Preact and bilingual i18n.

1. Add retained-history provenance and model framing to the shared projection. Verify original events/text, current task, repeated compactions, rewind and summary recognition remain intact.
2. Strip model-only framing at daemon display boundaries, carrying provenance through HTTP and live snapshots. Keep synthetic filtering unchanged.
3. Add WebUI provenance badge and a read-only complete-history view via the existing transcript API. Protect asynchronous loads across session changes, show errors, and preserve the active composer.
4. Clarify the TUI compaction notice: its transcript remains available above. Add Chinese and English strings.
5. Run focused projection tests, affected crate tests, WebUI tests/typecheck, fmt and compile gates. Do not commit unrelated changes.
