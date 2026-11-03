# Lossless patch application implementation plan

Goal: import an existing unified diff without routing its body through model output, then apply it by immutable reference.

Baseline: release/v5.2.6 at 31771349d; clean worktree. Capabilities owns parsing, artifact integrity and world-routed I/O. Harness mounts the tools over the existing filesystem and session artifact services; coding remains the sole lifecycle owner. No kernel commands or second runtime owner.

1. Extend ArtifactStore with an isolated input namespace and full SHA-256 integrity validation. Reuse its persistence primitives; do not expose inputs through fetch_output.
2. Implement a strict text unified-diff parser and exact all-hunk preflight. Reject unsupported binary/rename/mode/link changes, ambiguous paths, traversal and malformed hunk counts; preserve newline markers and bytes.
3. Add explicit file import and reference application tools. Import returns digest and target manifest rather than patch text. Application verifies the supplied manifest matches the stored patch before I/O, so target paths are visible to existing approval policies. No inferred code blocks or provider output-limit changes.
4. Route all target reads and writes through FileSystem. Plan all outputs before committing; acquire shared edit locks, recheck source bytes, report progress and cancellation before commit. Keep backups for best-effort rollback; never claim cross-file OS atomicity. Record/check target before/after hashes for safe retry.
5. Mount a `tool-patch` row after the existing filesystem and output-artifact rows. Reuse the actual scoped agent session id to namespace inputs, because the output store itself is project-scoped. Cover native driver consumers through shared mounting rather than individual frontend implementations.
6. Verify 5 files / 20 hunks / 2000 lines, conflict with zero writes, malformed input, UTF-8/newline fidelity, path/manifest attacks, replay and partial-write failures. Run affected nextest suites, compile gate, rustfmt and lock checks.

Failure semantics: import/check failure performs no workspace writes. Write or recovery failure returns explicit committed/restored/uncertain targets. Do not repeat a patch solely because the call timed out; verify recorded target states. This is not an OS-level atomic transaction against arbitrary external writers.

## Implemented interface and explicit first-version limits

The common coding assembly mounts `tool-patch`; CLI/TUI/daemon/WebUI/headless/ACP using CodingRuntime get the same tools. `import_patch({file_path})` is the explicit file import action; no frontend-specific upload or automatic fenced-code extraction is added. The file must be accessible in the execution world. It returns a complete record SHA-256 ID, separate original patch SHA-256, target manifest and counts. `apply_patch({artifact_id,file_paths,dry_run})` validates the exact manifest so existing permission/path gates see every target. Allows must cover every target; a deny on any target wins. Delegated agents cannot consume lead-owned patch plans.

This first version accepts exact updates to existing UTF-8 regular files only. New/deleted files, renames, permission changes, binary data, symlink targets, sensitive credential targets and `.git` paths are rejected; no fuzzy matching or newline conversion. Limits: 8 MiB patch, 256 files, 20,000 hunks per file, 8 MiB source file, 16 MiB combined pre/post images, 64 MiB serialized record. The original patch bytes and pre/post images are immutable and hash verified. A session resume resolves the same scoped storage. Input records are not fetch_output artifacts.

Usage: ask the agent to import an existing `changes.patch` by path. The model calls import_patch with only the filename, then apply_patch with only the returned ID and exact path list. Dry-run and retries use the same reference. No patch text belongs in tool-call arguments or shell heredocs.

Preflight conflicts perform zero writes. All-before permits application, all-after reports already_applied, mixed/foreign states refuse to proceed. Each write is preceded by a content and canonical-path recheck. Failure rolls back only verified postimages; unknown states are reported rather than overwritten. The implementation does not promise atomicity against external filesystem writers, process crashes, or cancellation while an OS write is pending. In those cases retry inspects state and refuses mixed results.

Validation includes 5 files × 400 changed lines / 20 hunks, exact UTF-8/no-final-newline behavior, traversal and malformed rejection, manifest mismatch, conflict with zero writes, dry-run, repeated application, digest tampering, session isolation, all-target permission rules, write failure recovery, partial-write uncertainty, and shared runtime routing with 8K model output options and one short apply call. No real model/provider call is required.

The differential harness records import_patch/apply_patch as intentional post-retirement tool additions. No retired-engine golden is regenerated or changed.

Pre-commit review: reject no-final-newline markers followed by more hunk/file content, avoiding accidental line concatenation. Mixed-state retries report separate already_applied and unchanged targets and perform no writes. Regression tests cover both cases. Existing credential gating also sees every manifest path; import refuses sensitive target snapshots outright. Actual code application participates in the existing verification cadence, while dry-run/already-applied responses do not create a new verification debt.
