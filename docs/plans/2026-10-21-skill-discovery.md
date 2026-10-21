# Skill discovery and loading diagnostics implementation plan

**Goal:** Allow explicitly configured skill roots and report what the live runtime actually loaded.

**Architecture:** Capabilities owns parsing and structured diagnostics. Coding owns directory resolution and the loaded instruction snapshot; drivers project that state. Existing approvals, session event storage, instruction precedence and refresh boundaries remain unchanged.

**Tech Stack:** Rust, serde/TOML, existing harness services and runtime events.

## Baseline and boundaries

- Branch `release/v5.2.4`, commit `67a7b3435fbb578bc49e0032e8d28602f7d4e508`.
- Existing untracked `.playwright-mcp/`, `docs/home-preview.png`, `hf-insights/` remain untouched.
- Skill startup and live reload currently resolve directories separately; both must use one resolver.
- Instruction loading selects the first existing candidate, including an empty/unreadable one. Preserve this precedence, report failure and shadowing.
- Failure to load an optional asset is diagnostic, not a fresh-session fallback or a runtime lifecycle change.
- No automatic external-tool discovery, conversion, private-format import, file watching or instruction hot reload.

## Implementation

1. Add `[skills].extra_dirs`, default empty; carry through runtime configuration. Resolve `~` against user home and relative paths against runtime working directory. Canonicalize existing roots for deduplication; extra roots have lower priority than standard roots. Explicit `PrepareOptions.skill_dirs` remains an exact override.
2. Keep skill diagnostics alongside the registry snapshot: failed reads/parses and overridden sources. Missing implicit roots remain normal. Surface new failures through runtime warnings and keep overrides queryable without warning spam.
3. Capture instruction text, selected sources and diagnostics once per prepared context. Reuse that snapshot at mount and in source reporting; compare with disk only when requested. Preserve resume refresh and light-reload behavior.
4. Project loaded instructions, skills and diagnostics into the existing source panel. Correct light-reload notices for the person and model.
5. Add regression tests for precedence/deduplication, failures, reload recovery, instruction shadowing and changed/deleted files, plus runtime config and live reload. Run affected crate nextest suites, formatting and `bash gates/compile.sh`; inspect Cargo.lock.
