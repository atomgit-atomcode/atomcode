# Project-Scoped TUI Input History Implementation Plan

> **For Claude:** Implement the two phases sequentially and review each checkpoint before continuing.

**Goal:** Keep at most 200 prompt-history entries per working directory, preserve the legacy global history as read-only fallback, and prevent concurrent TUI windows from overwriting each other.

**Architecture:** TUI remains the sole owner of input history. New history lives under a project-hash namespace derived with the existing native session hash. The legacy `~/.atomcode/history` file is never migrated or written: it only fills unused capacity until the project owns 200 entries. Working-directory commits rebind the foreground `History` after saving its pending entries.

**Tech Stack:** Rust, serde JSONL, fs2 file locks, same-directory atomic replacement, existing `SessionManager::project_hash`.

---

## Persisted layout and compatibility

```text
$ATOMCODE_HOME/history                    # legacy, read-only
$ATOMCODE_HOME/history-v2/<hash>/entries.jsonl
$ATOMCODE_HOME/history-v2/<hash>/write.lock
$ATOMCODE_HOME/history-v2/<hash>/images/
```

- Project entries are newest authority and capped on disk at 200.
- If a project has N < 200 entries, the view prepends the newest non-duplicate `200 - N` legacy entries.
- Once a project has 200 entries, legacy is not read.
- New pushes and image GC affect only the active project namespace.
- Exact entry equality is used for merging; a project entry wins over an identical legacy entry.

### Phase 1: Project history storage and concurrency

**Files:**
- Modify: `crates/atomcode-tuix/src/platform.rs`
- Modify: `crates/atomcode-tuix/src/input/history.rs`

1. Add tests for project path stability, project/legacy blending, the 200-entry cutoff, project-only writes, and two writers merging without lost updates.
2. Add a project-history path bundle derived from `SessionManager::project_hash(cwd)`.
3. Track only entries pushed since the last load/save as pending writes.
4. On save, acquire the project lock, reload current project JSONL, merge pending entries, cap to 200, and atomically replace the file.
5. Keep legacy and project image caches distinct; GC only the project cache.
6. Run the focused history and platform tests.

### Phase 2: Runtime working-directory rebinding

**Files:**
- Modify: `crates/atomcode-tuix/src/lib.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`

1. Add tests proving startup selects the current cwd and a committed cwd/session switch replaces the history view.
2. Initialize history from the startup working directory.
3. Add one TUI-owned rebind helper: save pending entries for the old project, retain failed saves in memory for retry, load or reuse the target project, then publish the new working-directory projection.
4. Route `/cd`, cross-project `/resume`, and foreground/background session replacement through the same committed projection helper.
5. Reset input-history navigation/search state when rebinding so indexes cannot point into the old project.
6. Run focused TUI tests, then the affected crate test suite.

## Failure semantics

- A history save/load failure must never fail a runtime cwd or session transition.
- A failed save keeps the old project `History` (including pending rows) in a TUI-owned deferred queue. Later cwd changes and shutdown retry it; switching back reuses that in-memory history instead of loading a stale disk view.
- Load failures are diagnostic and history falls back to an empty project view plus readable legacy where possible.
- The legacy file is never renamed, deleted, truncated, or appended.
- Lock contention waits only during the small local merge/write critical section; no runtime/network work occurs while holding it.
- Atomic replacement prevents partial JSONL after crashes.

## Verification

```bash
env -u ATOMCODE_HOME cargo test -p atomcode-tuix input::history --lib
env -u ATOMCODE_HOME cargo test -p atomcode-tuix working_dir_projection --lib
env -u ATOMCODE_HOME cargo test -p atomcode-tuix --lib
```

Windows-specific locking and replacement behavior should additionally be exercised by CI or a Windows build host.
