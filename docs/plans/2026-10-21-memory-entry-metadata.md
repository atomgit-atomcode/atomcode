# Memory entry metadata and exact management

**Goal:** Add stable entry identity, honest timestamps, topic/source/status metadata, and pinned selection, plus exact update/deprecation and topic filtering.

**Baseline:** release/v5.2.4, 67a7b3435, with the previous memory phase and unrelated staged/unstaged changes already present. Preserve those edits. No commits or pushes are part of this task.

**Owner and boundaries:** `atomcode-capabilities::memory` owns the single Markdown store, entry parser, mutation and prompt selection. The existing memory tool owns action validation and approval classification. Harness commands translate input to that same tool; TUI consumes the existing additive source-section DTO. Runtime/session logs remain authoritative for already-injected context; no history rewrite or additional lifecycle owner.

**Format:** New active entries remain `- text` followed by a reserved versioned HTML-comment JSON metadata suffix. Inactive managed entries use `> - text` so older bullet-only readers do not inject them. Content has one source, not a JSON copy plus Markdown copy. Legacy lines have unknown creation/update times and a deterministic content-derived ID; modifying a uniquely identified legacy entry persists that ID. Identical unannotated duplicates are ambiguous and cannot be changed by ID until their text is disambiguated. Unsupported/malformed metadata is an explicit error, never silently treated as active text.

**Failure semantics:** Managed writers serialize per resolved file using an advisory lock and replace via a synced temporary file. Parse, permission, validation and stale-source failures leave the old bytes untouched. Unknown scope/ID/status is an error. Keyword forget keeps its existing behavior but matches content, never metadata; new exact deprecation is reversible. External concurrent editors are checked before replacement but do not participate in advisory locking.

## Implementation

1. Add `memory/entry.rs` with versioned parse/render, validated metadata, deterministic legacy IDs, and patch validation. Test old/new round trips, duplicates and malformed/future metadata.
2. Route store writes through one locked atomic transaction. Add remember-with-metadata, list/filter and exact update/deprecation. Preserve non-entry lines, file permissions, local gitignore protection and custom global paths. Test failures, old timestamps, stable IDs, concurrent writers and soft deprecation.
3. Extend existing context selection to prioritize pinned active entries across scopes, exclude inactive entries from budget omissions, strip metadata from model text and show metadata/selection reasons in diagnostics. Keep the bounded source-read policy explicit.
4. Extend memory tool actions/schema and `/remember`/`/memory` parsing. Add additive structured entries to JSON sections; use shared tool logic for mutations and filters. Preserve old commands and approval policy. Test scope isolation, filtering, invalid input and no session-log rewrite.
5. Document format and commands. Run affected crate tests, TUI serial tests if the existing shared-counter race recurs, fmt and compile gate without overlapping ephemeral gate probes. Audit Cargo.lock for only legitimate optional leaf dependencies.

**Deferred:** TTL, physical expiry cleanup and automatic extraction/consolidation remain outside this phase.

## Completion and verification

Implemented the metadata store, pinned selection, scoped exact management and frontend commands. Shared `memory/input.rs` owns slash-command parsing for harness and TUI; TUI requests JSON and opens the viewer only for listings. Actual client transport regression coverage checks listings, mutations and invalid arguments. Source tabs expose every parsed entry ID alongside original Markdown.

Validation: selected capabilities/harness/TUI suites previously passed 3,784 tests across 39 groups (3 ignored). After final fixes, memory/parser tests passed 66 cases and TUI lib passed 1,574 serial cases. After the shared parser move, harness lib passed 74 cases and integration passed 26 cases; all four compile-gate checks passed. Cargo.lock adds only the legitimate i18n leaf dependency; private dependency names are absent.

Known baseline limitation: the extra capabilities `--no-default-features --features memory` check fails in unchanged `world.rs`/`process_utils.rs` because tools/process dependencies are referenced without their features. Product feature combinations are checked by the compile gate; fixing that independent feature combination is outside this change. No TTL, automatic extraction, commit or push.
