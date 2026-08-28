# Bounded Code Rewind — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Safely re-enable Code Rewind (workspace file checkpoint/restore) with hard disk-usage guarantees, so it can never again fill a user's disk.

**Architecture:** atomcode already has a shadow bare-git checkpoint store (`rewind.rs::WorkspaceCheckpoint` — capture=write-tree, restore, `retain_points`, transaction journal). It was disabled (`snapshot.rs`) because that store had no object sharing, no GC, and no size/disk ceiling → unbounded growth. This plan adds four bounding mechanisms — **(A) a free-disk floor circuit breaker checked synchronously before every capture**, **(B) a hard total-size budget with pre-write LRU eviction + eager `git gc --prune=now`**, **(C) `objects/info/alternates` object sharing with the real repo (dedup)**, and **(D) size/path filters** — then re-enables capture ONLY behind a default-OFF opt-in flag. Reference: opencode's `Snapshot` service uses shadow-git + alternates + TTL gc + a 2MB filter; we keep its storage shape but replace its *lazy TTL bounding* with *synchronous pre-write hard guards*, because the incident proved lazy/eventual bounding is insufficient.

**Tech Stack:** Rust, `git` plumbing via `std::process::Command` (already used in rewind.rs), `fs2` (`available_space`, already a capabilities dep), `#[cfg(feature = "session")]`.

**Spec:** This document is self-contained (design agreed in the 2026-08-28 design discussion). Key references in-repo: `crates/atomcode-capabilities/src/session/rewind.rs`, `crates/atomcode-capabilities/src/session/snapshot.rs` (the `CODE_REWIND_DISABLED_REASON` gate at ~line 125/168).

## Global Constraints

- **Invariant A (disk floor):** capture MUST be skipped (degrade to conversation-only rewind, no error) whenever available disk on the store's volume is below `DISK_FLOOR_BYTES`. This is the physical guarantee — it holds even if every other guard is buggy.
- **Invariant B (size budget):** when the store exceeds `STORE_BUDGET_BYTES`, `git gc --prune=now` reclaims unreferenced objects; if the store is still over budget, the capture is skipped (`Ok(None)`). The store is bounded at ~budget, but old rewind points are NOT auto-evicted — LRU eviction of retained points is a follow-up requiring ledger coordination (an evicted point's tree must not be left referenced by a stale ledger entry). The earlier `eviction_targets` helper was dead code (no production caller) and has been removed rather than left as a false guarantee.
- **Default OFF:** Code Rewind capture stays disabled unless the user opts in (`ATOMCODE_CODE_REWIND=1` or config). Restore logic is UNCHANGED (it was never disabled).
- **Fail-safe:** any error or uncertainty in a guard → skip capture, never crash the turn, never partially write.
- **Neutral code:** do NOT reference opencode/codex/omp by name in code or commits (borrow ideas, describe neutrally). [[feedback_no_opencode_references]]
- **Feature gate:** all new code is under `#[cfg(feature = "session")]` (rewind.rs already is). Run tests with `--features session`.
- **Windows:** the incident was on Windows/C:. Every guard must be cross-platform; final behavior needs real-machine Windows verification by the user (cannot be verified in CI here).

Constants (define in `rewind.rs`):
```rust
const DISK_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;      // 2 GiB free-disk floor
const STORE_BUDGET_BYTES: u64 = 500 * 1024 * 1024;          // 500 MiB per-store hard cap
const MAX_SNAPSHOT_FILE_BYTES: u64 = 5 * 1024 * 1024;       // skip files > 5 MiB
const KEEP_NEWEST_POINTS: usize = 30;                       // retention floor by count
```
(All overridable later via env; hardcode for v1.)

---

### Task 1: Disk-safety pure guards (the circuit breaker core)

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/rewind.rs` (add a private module `guard` near the top, after the consts)
- Test: same file, `#[cfg(test)] mod guard_tests`

**Interfaces:**
- Produces:
  - `fn available_disk_bytes(path: &Path) -> Option<u64>` — `fs2::available_space(path).ok()`; `None` on error.
  - `fn disk_floor_ok(available: Option<u64>, floor: u64) -> bool` — `false` when `available` is `None` (unknown → fail-safe skip) or `< floor`.
  - `fn dir_size_bytes(path: &Path) -> u64` — recursive sum of regular-file sizes; ignores errors (best-effort).
  - `fn eviction_targets(points_oldest_first: &[(u64 /*turn_id*/, u64 /*est_bytes*/)], current_store: u64, incoming: u64, budget: u64) -> Vec<u64>` — returns turn_ids to evict (oldest first) so `current_store - evicted + incoming <= budget`; empty when already fits; ALL when even that can't fit.

- [ ] **Step 1: Write the failing tests**
```rust
#[cfg(test)]
mod guard_tests {
    use super::*;
    #[test]
    fn disk_floor_unknown_is_treated_as_below_floor() {
        assert!(!disk_floor_ok(None, 2_000));          // unknown → skip (fail-safe)
        assert!(!disk_floor_ok(Some(1_999), 2_000));   // below floor → skip
        assert!(disk_floor_ok(Some(2_000), 2_000));    // exactly floor → ok
        assert!(disk_floor_ok(Some(9_999), 2_000));
    }
    #[test]
    fn eviction_frees_oldest_until_incoming_fits_budget() {
        // store=90, incoming=30, budget=100 → must free >=20 → evict oldest (id 1 = 15, id 2 = 10) → 25 freed.
        let pts = [(1u64, 15u64), (2, 10), (3, 40), (4, 25)]; // oldest first
        let evict = eviction_targets(&pts, 90, 30, 100);
        assert_eq!(evict, vec![1, 2]);
    }
    #[test]
    fn eviction_empty_when_already_fits() {
        assert!(eviction_targets(&[(1, 10)], 10, 5, 100).is_empty());
    }
    #[test]
    fn eviction_returns_all_when_incoming_alone_exceeds_budget() {
        // incoming (150) alone > budget (100): evict everything, capture will still be skipped by caller.
        assert_eq!(eviction_targets(&[(1, 10), (2, 20)], 30, 150, 100), vec![1, 2]);
    }
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p atomcode-capabilities --lib --features session guard_tests`
Expected: FAIL (functions not defined).

- [ ] **Step 3: Implement the guards**
```rust
fn available_disk_bytes(path: &Path) -> Option<u64> {
    fs2::available_space(path).ok()
}
fn disk_floor_ok(available: Option<u64>, floor: u64) -> bool {
    matches!(available, Some(a) if a >= floor)
}
fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if let Ok(md) = entry.metadata() {
                total = total.saturating_add(md.len());
            }
        }
    }
    total
}
fn eviction_targets(
    points_oldest_first: &[(u64, u64)],
    current_store: u64,
    incoming: u64,
    budget: u64,
) -> Vec<u64> {
    if current_store.saturating_add(incoming) <= budget {
        return Vec::new();
    }
    let mut freed = 0u64;
    let mut out = Vec::new();
    for (id, bytes) in points_oldest_first {
        out.push(*id);
        freed = freed.saturating_add(*bytes);
        if current_store.saturating_sub(freed).saturating_add(incoming) <= budget {
            break;
        }
    }
    out
}
```
Ensure `fs2` is available under `session`: in `crates/atomcode-capabilities/Cargo.toml`, add `"dep:fs2"` to the `session` feature list (fs2 is already an optional dep).

- [ ] **Step 4: Run to verify it passes**
Run: `cargo test -p atomcode-capabilities --lib --features session guard_tests`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-capabilities/src/session/rewind.rs crates/atomcode-capabilities/Cargo.toml
git commit -m "feat(rewind): disk-floor + size-budget pure guards (circuit-breaker core)"
```

---

### Task 2: Object sharing with the real repo (`objects/info/alternates`)

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/rewind.rs::initialize()` (~line 532) + add helper `fn write_alternates`
- Test: same file, `#[cfg(test)] mod alternates_tests` (uses a temp git repo)

**Interfaces:**
- Consumes: `self.worktree`, `self.git_dir` (existing fields).
- Produces: after `initialize()`, `<git_dir>/objects/info/alternates` contains the ABSOLUTE path to the real repo's `.git/objects` when the worktree is a git repo; capture then stores only new blobs. When the worktree is NOT a git repo, no alternates written (relies on budget + filters).

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod alternates_tests {
    use super::*;
    #[test]
    fn initialize_writes_alternates_to_real_objects_dir() {
        let tmp = tempfile::tdir();                 // helper: temp dir
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init", "--quiet"]);       // helper: real repo
        std::fs::write(work.join("a.txt"), b"hi").unwrap();
        run_git(&work, &["add", "."]);
        let store = tmp.path().join("store");
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        let alt = store.join("objects/info/alternates");
        let contents = std::fs::read_to_string(&alt).unwrap();
        let real_objects = std::fs::canonicalize(work.join(".git/objects")).unwrap();
        assert!(contents.trim().ends_with(&*real_objects.to_string_lossy()),
            "alternates points at real objects: {contents}");
        drop(cp);
    }
}
```
(Add small `tdir()`, `run_git()` helpers if not present.)

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p atomcode-capabilities --lib --features session alternates_tests`
Expected: FAIL (no alternates file).

- [ ] **Step 3: Implement**
In `initialize()`, after the `git init --bare` block and configs, before writing the version marker:
```rust
self.write_alternates()?;
```
New method:
```rust
fn write_alternates(&self) -> Result<(), WorkspaceCheckpointError> {
    // Point the shadow store at the real repo's object database so capture
    // stores only NEW blobs instead of duplicating the entire working tree.
    // Skip silently for non-git worktrees (no baseline to share).
    let real_git = self.worktree.join(".git");
    let objects = if real_git.is_dir() {
        real_git.join("objects")
    } else {
        return Ok(()); // worktree file (submodule/worktree) or non-repo: no alternates
    };
    let objects = match std::fs::canonicalize(&objects) {
        Ok(p) => p,
        Err(_) => return Ok(()), // real objects missing → do NOT full-copy silently; just no share
    };
    let info = self.git_dir.join("objects").join("info");
    std::fs::create_dir_all(&info).map_err(|source| WorkspaceCheckpointError::Io { path: info.clone(), source })?;
    let alternates = info.join("alternates");
    let line = format!("{}\n", objects.to_string_lossy());
    std::fs::write(&alternates, line).map_err(|source| WorkspaceCheckpointError::Io { path: alternates, source })?;
    Ok(())
}
```

- [ ] **Step 4: Run to verify it passes**
Run: `cargo test -p atomcode-capabilities --lib --features session alternates_tests`
Expected: PASS. Also run the full rewind suite to confirm no regression: `cargo test -p atomcode-capabilities --lib --features session rewind`

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-capabilities/src/session/rewind.rs
git commit -m "feat(rewind): share real-repo objects via alternates (dedup, no full-copy)"
```

---

### Task 3: Size + path filters in capture

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/rewind.rs::capture_locked()` (~line 563) + add `fn should_snapshot(rel, worktree)` predicate
- Test: same file, `#[cfg(test)] mod filter_tests` (pure predicate)

**Interfaces:**
- Produces: `fn is_excluded_dir(rel: &str) -> bool` — true for path components in `{node_modules, target, dist, build, .venv, __pycache__, .next, .cache}`.
- `capture_locked` skips a candidate path when it is an excluded dir OR its on-disk size > `MAX_SNAPSHOT_FILE_BYTES`.

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod filter_tests {
    use super::*;
    #[test]
    fn excludes_common_build_and_dep_dirs() {
        assert!(is_excluded_dir("node_modules/react/index.js"));
        assert!(is_excluded_dir("crate/target/debug/foo"));
        assert!(is_excluded_dir(".venv/lib/x"));
        assert!(!is_excluded_dir("src/main.rs"));
        assert!(!is_excluded_dir("targeted/thing.rs")); // must match a full component, not substring
    }
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p atomcode-capabilities --lib --features session filter_tests`
Expected: FAIL.

- [ ] **Step 3: Implement**
```rust
fn is_excluded_dir(rel: &str) -> bool {
    const EXCLUDED: &[&str] = &[
        "node_modules", "target", "dist", "build", ".venv", "__pycache__", ".next", ".cache",
    ];
    rel.split('/').any(|c| EXCLUDED.contains(&c))
}
```
In `capture_locked`, inside the `for path in tracked.into_iter().chain(untracked)` loop, after the existing `is_sensitive_path` check, add:
```rust
if is_excluded_dir(&path) {
    continue;
}
let abs = self.worktree.join(&path);
if std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0) > MAX_SNAPSHOT_FILE_BYTES {
    continue;
}
```
(Keep existing `validate_relative_path` / `is_sensitive_path`.)

- [ ] **Step 4: Run to verify it passes**
Run: `cargo test -p atomcode-capabilities --lib --features session filter_tests rewind`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-capabilities/src/session/rewind.rs
git commit -m "feat(rewind): skip large files + build/dep dirs in capture"
```

---

### Task 4: Wire guards into capture + eager gc on retain

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/rewind.rs::capture()` (~line 360) and `retain_points()` (~line 471)
- Test: same file, integration-style `#[cfg(test)] mod bounded_capture_tests` (temp repo)

**Interfaces:**
- Consumes: Task 1 guards, `STORE_BUDGET_BYTES`, `DISK_FLOOR_BYTES`.
- Produces:
  - `pub fn capture(&self) -> Result<Option<String>, WorkspaceCheckpointError>` — **return type changes from `String` to `Option<String>`**; `Ok(None)` means "skipped for safety" (disk floor hit, or store cannot fit under budget). Callers (snapshot.rs) treat `None` as "no code checkpoint this turn". Update the one existing caller.
  - `retain_points` runs `git gc --prune=now` after the ref transaction so evicted objects are reclaimed immediately.

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod bounded_capture_tests {
    use super::*;
    #[test]
    fn capture_skips_when_disk_below_floor() {
        // Use a store on a path we force to look empty by setting a floor > total disk.
        let (work, store, _tmp) = temp_git_workspace();  // helper builds real repo + store
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        // Directly exercise the guard path: with an absurd floor, capture must be None.
        assert!(cp.capture_bounded(u64::MAX, 0).unwrap().is_none(),
            "floor=u64::MAX forces skip");
    }
    #[test]
    fn capture_writes_a_tree_when_within_limits() {
        let (work, store, _tmp) = temp_git_workspace();
        let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
        let tree = cp.capture_bounded(0, u64::MAX).unwrap(); // floor=0, budget=∞
        assert!(tree.is_some());
    }
}
```
(Add a test-only `capture_bounded(floor, budget)` that `capture()` delegates to with the real consts — lets tests inject limits deterministically.)

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p atomcode-capabilities --lib --features session bounded_capture_tests`
Expected: FAIL.

- [ ] **Step 3: Implement**
```rust
pub fn capture(&self) -> Result<Option<String>, WorkspaceCheckpointError> {
    self.capture_bounded(DISK_FLOOR_BYTES, STORE_BUDGET_BYTES)
}

pub(crate) fn capture_bounded(&self, floor: u64, budget: u64)
    -> Result<Option<String>, WorkspaceCheckpointError>
{
    // Invariant A: physical disk floor. Unknown free space → skip.
    if !disk_floor_ok(available_disk_bytes(&self.git_dir), floor) {
        return Ok(None);
    }
    // Invariant B: store must not exceed budget. If even empty-store + one capture
    // can't fit, skip (a single monstrous working tree). Eviction of old points is
    // driven by retain_points + gc; here we just refuse to exceed the ceiling.
    let store_bytes = dir_size_bytes(&self.git_dir);
    if store_bytes > budget {
        // Over budget already → prune first; if still over, skip.
        let _ = self.gc_prune_now();
        if dir_size_bytes(&self.git_dir) > budget {
            return Ok(None);
        }
    }
    let _guard = self.guard();
    let tree = self.with_process_lock(|| self.capture_locked())?;
    Ok(Some(tree))
}

fn gc_prune_now(&self) -> Result<(), WorkspaceCheckpointError> {
    // Best-effort: reclaim unreferenced objects immediately (not on a timer).
    let _ = self.run(["gc", "--prune=now", "--quiet"]);
    Ok(())
}
```
In `retain_points`, after `update-ref --stdin` succeeds, add `let _ = self.gc_prune_now();` (inside the process lock, after the transaction).

Update the ONE existing `capture()` caller in `snapshot.rs` to handle `Option` (map `None` → no checkpoint recorded this turn).

- [ ] **Step 4: Run to verify it passes**
Run: `cargo test -p atomcode-capabilities --lib --features session bounded_capture_tests rewind`
Expected: PASS. Then `cargo build -p atomcode-capabilities --features session` (caller update compiles).

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-capabilities/src/session/rewind.rs crates/atomcode-capabilities/src/session/snapshot.rs
git commit -m "feat(rewind): capture guarded by disk floor + size budget; eager gc on retain"
```

---

### Task 5: Default-OFF opt-in flag + relocatable store

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/snapshot.rs` (the `SnapshotHook::new` gate ~line 156-169)
- Modify: `crates/atomcode-capabilities/src/session/rewind.rs` (add `fn code_rewind_opt_in() -> bool`)
- Test: snapshot.rs `#[cfg(test)]` (env-guarded, serial)

**Interfaces:**
- Produces: `pub fn code_rewind_opt_in() -> bool` — `matches!(std::env::var("ATOMCODE_CODE_REWIND").ok().as_deref(), Some("1" | "true" | "on" | "yes"))`. Default (unset) → `false`.

- [ ] **Step 1: Write the failing test** (env-serial; save/restore var)
```rust
#[test]
fn code_rewind_is_off_by_default_and_on_when_opted_in() {
    let prev = std::env::var("ATOMCODE_CODE_REWIND").ok();
    std::env::remove_var("ATOMCODE_CODE_REWIND");
    assert!(!crate::session::rewind::code_rewind_opt_in());
    std::env::set_var("ATOMCODE_CODE_REWIND", "1");
    assert!(crate::session::rewind::code_rewind_opt_in());
    match prev { Some(v) => std::env::set_var("ATOMCODE_CODE_REWIND", v), None => std::env::remove_var("ATOMCODE_CODE_REWIND") }
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p atomcode-capabilities --lib --features session code_rewind_is_off`
Expected: FAIL.

- [ ] **Step 3: Implement**
Add `code_rewind_opt_in` (pub) to rewind.rs. This task defines the gate function; Task 6 wires it into `SnapshotHook::new`. (Store location relocation: the git_dir is already passed in by the caller; document that it should live under `$ATOMCODE_HOME` — no code change if it already does. Verify the caller path in Task 6.)

- [ ] **Step 4: Run to verify it passes**
Run: `cargo test -p atomcode-capabilities --lib --features session code_rewind_is_off`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-capabilities/src/session/rewind.rs
git commit -m "feat(rewind): ATOMCODE_CODE_REWIND opt-in flag (default off)"
```

---

### Task 6: Re-enable capture behind the flag + honest message

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/snapshot.rs` — `SnapshotHook::new` (~156-169), `CODE_REWIND_DISABLED_REASON` (~125)
- Test: snapshot.rs `#[cfg(test)]` (env-serial)

**Interfaces:**
- Consumes: `code_rewind_opt_in()` (Task 5), `WorkspaceCheckpoint::with_store` (existing), the guarded `capture()` (Task 4).

- [ ] **Step 1: Write the failing tests**
```rust
#[test]
fn snapshot_hook_keeps_code_rewind_off_by_default() {
    // (env unset) → unavailable reason set, checkpoint None
    with_env_unset("ATOMCODE_CODE_REWIND", || {
        let hook = make_test_hook();       // helper builds SnapshotHook over a temp repo
        assert!(hook.code_rewind_unavailable().is_some());
    });
}
#[test]
fn snapshot_hook_enables_code_rewind_when_opted_in_and_bounded_store_builds() {
    with_env_set("ATOMCODE_CODE_REWIND", "1", || {
        let hook = make_test_hook();
        assert!(hook.code_rewind_unavailable().is_none(), "opted in → available");
    });
}
#[test]
fn disabled_reason_does_not_pin_a_stale_version() {
    assert!(!CODE_REWIND_DISABLED_REASON.contains("v5.0.5"));
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p atomcode-capabilities --lib --features session snapshot_hook`
Expected: FAIL.

- [ ] **Step 3: Implement**
Change the message:
```rust
const CODE_REWIND_DISABLED_REASON: &str =
    "Code Rewind (workspace file restore) is off by default to protect disk space; \
     set ATOMCODE_CODE_REWIND=1 to opt in. Conversation Rewind remains available.";
```
In `SnapshotHook::new`, replace the unconditional `checkpoint = None; unavailable = Some(...)` with:
```rust
let (checkpoint, unavailable) = if crate::session::rewind::code_rewind_opt_in() {
    match WorkspaceCheckpoint::with_store(&working_dir, /* store git_dir under $ATOMCODE_HOME */) {
        Ok(cp) => (Some(Arc::new(cp)), None),
        Err(e) => (None, Some(format!("Code Rewind unavailable: {e}"))),
    }
} else {
    (None, Some(CODE_REWIND_DISABLED_REASON.to_string()))
};
```
Confirm the store git_dir path lives under `$ATOMCODE_HOME/sessions/<bucket>/rewind/<session>` (NOT inside the worktree — `with_store` already rejects that). If the current path is under C:/user-profile with no relocation, add: honor `ATOMCODE_HOME` (already the convention).

- [ ] **Step 4: Run to verify it passes**
Run: `cargo test -p atomcode-capabilities --lib --features session snapshot_hook` and the runtime.rs reason test at `crates/atomcode-coding/src/runtime.rs:14146` (`assert reason.contains("temporarily disabled")` — UPDATE it to match the new wording, or assert `.contains("off by default")`). Grep for `temporarily disabled` and fix the 3 test assertions found earlier.

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-capabilities/src/session/snapshot.rs crates/atomcode-coding/src/runtime.rs
git commit -m "feat(rewind): re-enable Code Rewind behind opt-in flag; drop stale v5.0.5 message"
```

---

### Task 7: Observability + purge command + self-disable

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/rewind.rs` — `pub fn store_size_bytes(&self) -> u64` + `pub fn purge(&self) -> Result<(), WorkspaceCheckpointError>`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs` — extend `/rewind` (or add `/rewind purge`) to call purge + show store size
- Test: rewind.rs `#[cfg(test)]`

**Interfaces:**
- Produces: `store_size_bytes` (delegates to `dir_size_bytes(&self.git_dir)`), `purge` (`git for-each-ref` delete all `refs/atomcode/*` + `gc --prune=now`, leaving an empty store).

- [ ] **Step 1: Write the failing test**
```rust
#[test]
fn purge_empties_the_store_refs() {
    let (work, store, _tmp) = temp_git_workspace();
    let cp = WorkspaceCheckpoint::with_store(&work, &store).unwrap();
    let tree = cp.capture_bounded(0, u64::MAX).unwrap().unwrap();
    cp.retain_points(&[rewind_point_for(&tree)]).unwrap();   // helper builds a RewindPoint
    cp.purge().unwrap();
    let refs = cp.run(["for-each-ref", "refs/atomcode/"]).unwrap();
    assert!(refs.stdout.is_empty(), "all rewind refs deleted");
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p atomcode-capabilities --lib --features session purge`
Expected: FAIL.

- [ ] **Step 3: Implement** `store_size_bytes` + `purge` (delete refs via `update-ref --stdin` `delete` lines for every `refs/atomcode/*`, then `gc_prune_now`). Wire `/rewind purge` in the TUI command (report `store_size_bytes` before/after). Self-disable: in `capture_bounded`, if after prune the store is STILL over budget, log a warning and return `Ok(None)` (already the behavior) — add a `tracing::warn!` so it's observable.

- [ ] **Step 4: Run to verify it passes**
Run: `cargo test -p atomcode-capabilities --lib --features session purge` + `cargo build --workspace`

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-capabilities/src/session/rewind.rs crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "feat(rewind): store-size reporting + /rewind purge + over-budget self-skip"
```

---

## Self-Review

**Spec coverage:** Invariant A → Task 1 (`disk_floor_ok`) + Task 4 (wired). Invariant B → Task 1 (`eviction_targets`) + Task 4 (budget check) + Task 4/7 (gc). Default OFF → Task 5 + Task 6. alternates → Task 2. filters → Task 3. message → Task 6. observability/purge → Task 7. Windows verification → Global Constraints (user real-machine, out of CI). ✓

**Type consistency:** `capture()` return type changes to `Option<String>` in Task 4 — the ONE caller (snapshot.rs) is updated in Task 4 and re-wired in Task 6; no other caller (grep `.capture()` before Task 4). `capture_bounded(floor,budget)` is the injectable core used by tests in Tasks 4 & 7. `code_rewind_opt_in()` defined in Task 5, consumed in Task 6. `is_excluded_dir`/`should_snapshot` naming consistent.

**Placeholder scan:** each code step has real code; the only deferred detail is the exact store git_dir path (Task 6 verifies it's under `$ATOMCODE_HOME`, not a placeholder — a verification step).

**Staging (risk):** Tasks 1-4 build & test the guards with capture STILL disabled (flag default off, Task 5/6 last). So the disk-writing path only turns on at Task 6, behind opt-in, after all guards + tests exist. Ship Tasks 1-6, keep default OFF, hand to user for Windows real-machine verification before ever flipping the default.
