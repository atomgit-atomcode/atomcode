//! Bounds the Code Rewind stores across sessions.
//!
//! Every session keeps its own shadow Git store under
//! `<user>/rewind/<bucket>/<session id>`, and each store's budget
//! (`rewind::STORE_BUDGET_BYTES`) only bounds that one store. Without this
//! module the stores of every session ever run stay on disk, and untracked
//! files are stored again by each new session, so a busy project grows by the
//! size of its untracked files per session.
//!
//! A store is evidence for `/rewind` only; the conversation never depends on
//! it (a missing checkpoint quarantines its points, see
//! `SnapshotHook::new_with_context`). So a store is removed:
//! - with its session (`SessionManager::delete`);
//! - by [`sweep_with`], keeping the newest stores of each project and a global
//!   budget, never touching a session some runtime holds open.
//!
//! The rewind bucket is the hash of the Git worktree root while the session
//! bucket is the hash of the working directory, so a session started in a
//! subdirectory keeps its store under a different bucket. Stores are therefore
//! found by session id across buckets, never by assuming the two match.

use super::manager::SessionManager;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// File every rewind store carries (`WorkspaceCheckpoint::initialize`); a
/// directory without it is not ours and is never removed.
const STORE_MARKER: &str = "atomcode-rewind-version";

/// Stores kept per project, newest first.
pub(crate) const KEEP_PER_PROJECT: usize = 20;
/// All stores together, after the per-project trim.
pub(crate) const TOTAL_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// A store with no session files is left alone this long: a session may
/// create its store before anything else of it reaches the disk.
const ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// What a sweep keeps.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    pub keep_per_project: usize,
    pub total_budget_bytes: u64,
    pub orphan_grace: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            keep_per_project: KEEP_PER_PROJECT,
            total_budget_bytes: TOTAL_BUDGET_BYTES,
            orphan_grace: ORPHAN_GRACE,
        }
    }
}

/// `<user>/rewind`, from a session store's root (`<user>/sessions/<bucket>`).
/// `None` for a store not laid out that way (a test's custom root).
pub(crate) fn rewind_root_for(session_root: &Path) -> Option<PathBuf> {
    let sessions = session_root.parent()?;
    if sessions.file_name()? != "sessions" {
        return None;
    }
    Some(sessions.parent()?.join("rewind"))
}

fn is_store(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir())
        && fs::symlink_metadata(dir.join(STORE_MARKER)).is_ok_and(|m| m.file_type().is_file())
}

/// The ids `WorkspaceCheckpoint::for_session` accepts as a store directory
/// name. Anything else (`..`, a separator, an id read from a file) never
/// becomes a path here.
fn valid_store_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

/// The project directories under `rewind_root`: real directories only, so a
/// symlinked bucket is never followed out of the tree. Dot-named entries
/// (the trash) are not projects.
fn buckets(rewind_root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(rewind_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            (!name.starts_with('.')).then(|| (name, entry.path()))
        })
        .collect()
}

/// Whether something is there, is not, or could not be told. A sweep only
/// acts on `No`: an unreadable journal or session directory is not proof
/// that nothing needs the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    Yes,
    No,
    Unknown,
}

impl Seen {
    fn of_error(error: &std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            Seen::No
        } else {
            Seen::Unknown
        }
    }

    fn exists(path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Ok(_) => Seen::Yes,
            Err(error) => Seen::of_error(&error),
        }
    }

    /// `Yes` if any is, else `Unknown` if any could not be told, else `No`.
    fn any(seen: impl IntoIterator<Item = Seen>) -> Self {
        seen.into_iter()
            .fold(Seen::No, |acc, seen| match (acc, seen) {
                (Seen::Yes, _) | (_, Seen::Yes) => Seen::Yes,
                (Seen::Unknown, _) | (_, Seen::Unknown) => Seen::Unknown,
                _ => Seen::No,
            })
    }
}

/// A restore that stopped part way still needs its store: the next resume
/// compensates from the recovery tree pinned there (`SnapshotHook` crash
/// recovery). Its journal is a session sidecar; the pin is a ref in the store.
fn recovery_pending(store: &Path, session_bucket: Option<&Path>, session_id: &str) -> Seen {
    let journals = session_bucket.into_iter().flat_map(|bucket| {
        ["rewind.txn", "rewind.txn.json"]
            .map(|suffix| Seen::exists(&bucket.join(format!("{session_id}.{suffix}"))))
    });
    let loose = match fs::read_dir(store.join("refs").join("atomcode").join("recovery")) {
        Ok(mut refs) => match refs.next() {
            None => Seen::No,
            Some(Ok(_)) => Seen::Yes,
            Some(Err(_)) => Seen::Unknown,
        },
        Err(error) => Seen::of_error(&error),
    };
    let packed = match fs::read_to_string(store.join("packed-refs")) {
        Ok(refs) if refs.contains("refs/atomcode/recovery/") => Seen::Yes,
        Ok(_) => Seen::No,
        Err(error) => Seen::of_error(&error),
    };
    Seen::any(journals.chain([loose, packed]))
}

/// `<rewind root>/.trash`, only as a real directory: a symlink or file there
/// is refused rather than followed. Created when `create` and absent.
fn trash_dir(root: &Path, create: bool) -> std::io::Result<Option<PathBuf>> {
    let trash = root.join(".trash");
    match fs::symlink_metadata(&trash) {
        Ok(meta) if meta.file_type().is_dir() => Ok(Some(trash)),
        Ok(_) => Err(std::io::ErrorKind::InvalidInput.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            match fs::create_dir(&trash) {
                Ok(()) => Ok(Some(trash)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    trash_dir(root, false)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Move `<rewind_root>/<bucket>/<id>` into the trash by one rename, after
/// checking it still resolves inside `rewind_root`. Quick, so a lease held
/// around it is held only for the rename; the slow delete happens after.
fn retire(rewind_root: &Path, store: &Path) -> std::io::Result<PathBuf> {
    let root = fs::canonicalize(rewind_root)?;
    let bucket = store.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    if fs::canonicalize(bucket)?.parent() != Some(root.as_path()) || !is_store(store) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let trash = trash_dir(&root, true)?.ok_or(std::io::ErrorKind::NotFound)?;
    let name = format!(
        "{}-{}-{}",
        bucket.file_name().unwrap_or_default().to_string_lossy(),
        store.file_name().unwrap_or_default().to_string_lossy(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    let target = trash.join(name);
    fs::rename(store, &target)?;
    Ok(target)
}

/// Delete stores an earlier run moved to the trash but did not finish
/// deleting. Only real directories that still carry the store marker: what a
/// retire put there, nothing else that may have appeared.
fn empty_trash(rewind_root: &Path) {
    let Ok(root) = fs::canonicalize(rewind_root) else {
        return;
    };
    let Ok(Some(trash)) = trash_dir(&root, false) else {
        return;
    };
    let Ok(entries) = fs::read_dir(trash) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) && is_store(&entry.path()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Remove every store of `session_id`, whichever bucket it is in. The caller
/// holds the session's lease, so no runtime is writing to it. Best-effort per
/// store; returns how many were removed.
pub(crate) fn remove_session_stores(rewind_root: &Path, session_id: &str) -> usize {
    if !valid_store_id(session_id) {
        return 0;
    }
    let mut removed = 0;
    for (_, bucket) in buckets(rewind_root) {
        let store = bucket.join(session_id);
        if !is_store(&store) {
            continue;
        }
        match retire(rewind_root, &store).and_then(|trash| fs::remove_dir_all(trash)) {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(
                path = %store.display(),
                %error,
                "could not remove a deleted session's rewind store"
            ),
        }
    }
    removed
}

struct Store {
    path: PathBuf,
    bucket: String,
    session_id: String,
    last_used: SystemTime,
    bytes: u64,
}

/// When the store was last written: a capture rewrites its `index` and
/// moves its point refs.
fn last_used(dir: &Path) -> SystemTime {
    [
        dir.join("index"),
        dir.join("packed-refs"),
        dir.join("refs").join("atomcode"),
        dir.to_path_buf(),
    ]
    .iter()
    .filter_map(|path| fs::metadata(path).and_then(|m| m.modified()).ok())
    .max()
    .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn list_stores(rewind_root: &Path) -> Vec<Store> {
    let mut stores = Vec::new();
    for (bucket, dir) in buckets(rewind_root) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = entry.path();
            if !valid_store_id(&session_id) || !is_store(&path) {
                continue;
            }
            stores.push(Store {
                bucket: bucket.clone(),
                session_id,
                last_used: last_used(&path),
                bytes: super::rewind::dir_size_bytes(&path),
                path,
            });
        }
    }
    stores
}

/// Where a store's session lives, if anywhere.
enum Owner {
    Session(PathBuf),
    /// No session files: the session was deleted, or has not written yet.
    None,
    /// The session directories could not all be read.
    Unknown,
}

/// The session bucket holding `session_id`'s files.
fn session_bucket(sessions_root: &Path, session_id: &str) -> Owner {
    let entries = match fs::read_dir(sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Owner::None,
        Err(_) => return Owner::Unknown,
    };
    let names = [format!("{session_id}.index"), format!("{session_id}.meta")];
    let mut unknown = false;
    for entry in entries {
        let Ok(entry) = entry else {
            unknown = true;
            continue;
        };
        let bucket = entry.path();
        match fs::metadata(&bucket) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(_) => {
                unknown = true;
                continue;
            }
        }
        match Seen::any(names.iter().map(|name| Seen::exists(&bucket.join(name)))) {
            Seen::Yes => return Owner::Session(bucket),
            Seen::Unknown => unknown = true,
            Seen::No => {}
        }
    }
    if unknown {
        Owner::Unknown
    } else {
        Owner::None
    }
}

/// Whether the store was written after the sweep ranked it.
fn changed_since_scan(store: &Store) -> bool {
    last_used(&store.path) != store.last_used
}

/// Remove one store unless its session is open or owes a recovery. An orphan
/// (no session files) is removed only past the grace. A session's store is
/// moved to the trash under that session's lease — so a resume racing it waits
/// only for one rename, never writes into a half-deleted store — and deleted
/// after the lease is released.
fn evict(
    store: &Store,
    rewind_root: &Path,
    sessions_root: &Path,
    keep: &str,
    now: SystemTime,
    policy: &Policy,
) -> bool {
    if store.session_id == keep {
        return false;
    }
    let retired = match session_bucket(sessions_root, &store.session_id) {
        Owner::Unknown => return false,
        Owner::None => {
            let old = now
                .duration_since(store.last_used)
                .is_ok_and(|age| age >= policy.orphan_grace);
            if !old
                || changed_since_scan(store)
                || recovery_pending(&store.path, None, &store.session_id) != Seen::No
            {
                return false;
            }
            retire(rewind_root, &store.path)
        }
        Owner::Session(bucket) => {
            let manager = SessionManager::with_root(&bucket);
            let Ok(lease) = manager.acquire_lease(&store.session_id) else {
                return false;
            };
            // Checked under the lease, so nothing can write in between: a session
            // resumed, captured and closed since the scan is no longer the store
            // that was ranked, and waits for the next sweep to rank it again; and
            // no runtime can start a restore now.
            if changed_since_scan(store)
                || recovery_pending(&store.path, Some(&bucket), &store.session_id) != Seen::No
            {
                return false;
            }
            let retired = retire(rewind_root, &store.path);
            drop(lease);
            retired
        }
    };
    match retired.and_then(|trash| fs::remove_dir_all(trash)) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(path = %store.path.display(), %error, "could not remove a rewind store");
            false
        }
    }
}

/// Keep the newest `keep_per_project` stores of each project, then evict the
/// least recently used until all stores fit `total_budget_bytes`. `keep` (the
/// session starting now) and any session another runtime holds are never
/// removed. Returns the bytes freed.
pub(crate) fn sweep_with(
    dirs: &crate::ProductDirs,
    keep: &str,
    now: SystemTime,
    policy: &Policy,
) -> u64 {
    let rewind_root = dirs.user().join("rewind");
    let sessions_root = SessionManager::sessions_root(dirs);
    empty_trash(&rewind_root);
    let mut stores = list_stores(&rewind_root);
    // Newest first, so a project's first `keep_per_project` are its newest.
    stores.sort_by(|a, b| b.last_used.cmp(&a.last_used));
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let mut freed = 0;
    let mut kept = Vec::new();
    for store in stores {
        let rank = seen.entry(store.bucket.clone()).or_default();
        *rank += 1;
        if *rank > policy.keep_per_project
            && evict(&store, &rewind_root, &sessions_root, keep, now, policy)
        {
            freed += store.bytes;
        } else {
            kept.push(store);
        }
    }
    let mut total: u64 = kept.iter().map(|store| store.bytes).sum();
    // Oldest first.
    for store in kept.iter().rev() {
        if total <= policy.total_budget_bytes {
            break;
        }
        if evict(store, &rewind_root, &sessions_root, keep, now, policy) {
            total = total.saturating_sub(store.bytes);
            freed += store.bytes;
        }
    }
    freed
}

static SWEEPING: AtomicBool = AtomicBool::new(false);

/// [`sweep_with`] the default policy on a background thread, at most one at a
/// time per process. Called when a session opens its rewind store; walking
/// every store must not delay that.
pub(crate) fn sweep_in_background(dirs: crate::ProductDirs, keep: String) {
    if SWEEPING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("rewind-retention".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let freed = sweep_with(&dirs, &keep, SystemTime::now(), &Policy::default());
            if freed > 0 {
                tracing::info!(
                    freed_bytes = freed,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "rewind stores swept"
                );
            }
            SWEEPING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        SWEEPING.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dirs a test's temp tree stands for.
    ///
    /// The sessions root is a host choice, so it is read through
    /// [`ProductDirs`] rather than assumed to be `<user>/sessions` — the same
    /// way production resolves it.
    fn dirs(user: &Path) -> crate::ProductDirs {
        crate::ProductDirs::new(user.to_path_buf(), ".atomcode")
    }

    /// [`sweep_with`] over a test tree.
    fn sweep(user: &Path, keep: &str, now: SystemTime, policy: &Policy) -> u64 {
        sweep_with(&dirs(user), keep, now, policy)
    }

    fn store(user: &Path, bucket: &str, id: &str, bytes: usize, age: Duration) -> PathBuf {
        let dir = user.join("rewind").join(bucket).join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(STORE_MARKER), "atomcode-rewind-v1").unwrap();
        fs::write(dir.join("blob"), vec![0u8; bytes]).unwrap();
        let index = dir.join("index");
        fs::write(&index, b"").unwrap();
        let when = SystemTime::now() - age;
        set_mtime(&index, when);
        // The directory's own mtime moved when its files were written.
        set_mtime(&dir, when);
        dir
    }

    /// Set a path's modification time, on a file or a directory.
    ///
    /// `File::set_modified` cannot do a directory, and on Windows
    /// `File::open` on a directory is refused outright (ERROR_ACCESS_DENIED) —
    /// so the store's age could not be given on that platform at all.
    /// `filetime` opens a directory with `FILE_FLAG_BACKUP_SEMANTICS`, which is
    /// what that takes, and it works the same on Unix.
    fn set_mtime(path: &Path, when: SystemTime) {
        let when = filetime::FileTime::from_system_time(when);
        filetime::set_file_mtime(path, when).unwrap();
    }

    fn session(user: &Path, bucket: &str, id: &str) {
        let dir = SessionManager::sessions_root(&dirs(user)).join(bucket);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{id}.index")), b"{}").unwrap();
    }

    fn policy(keep: usize, budget: u64) -> Policy {
        Policy {
            keep_per_project: keep,
            total_budget_bytes: budget,
            orphan_grace: Duration::from_secs(3600),
        }
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn each_project_keeps_its_newest_stores() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let mut dirs = Vec::new();
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            session(user, "s1", id);
            dirs.push(store(user, "p1", id, 10, HOUR * (i as u32 + 1)));
        }
        session(user, "s2", "other");
        let other = store(user, "p2", "other", 10, HOUR * 9);
        sweep(user, "", SystemTime::now(), &policy(2, u64::MAX));
        assert!(dirs[0].exists() && dirs[1].exists(), "the two newest stay");
        assert!(!dirs[2].exists(), "the oldest of the project goes");
        assert!(other.exists(), "another project counts on its own");
    }

    #[test]
    fn the_global_budget_evicts_the_least_recently_used() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        session(user, "s", "new");
        session(user, "s", "mid");
        session(user, "s", "old");
        let new = store(user, "p1", "new", 100, HOUR);
        let mid = store(user, "p2", "mid", 100, HOUR * 2);
        let old = store(user, "p3", "old", 100, HOUR * 3);
        // Each store also holds its marker and index, so 250 fits two.
        sweep(user, "", SystemTime::now(), &policy(20, 250));
        assert!(new.exists() && mid.exists());
        assert!(!old.exists());
    }

    #[test]
    fn an_open_or_current_session_keeps_its_store() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        session(user, "s", "open");
        session(user, "s", "current");
        let open = store(user, "p", "open", 10, HOUR * 5);
        let current = store(user, "p", "current", 10, HOUR * 6);
        let manager =
            SessionManager::with_root(SessionManager::sessions_root(&dirs(user)).join("s"));
        let lease = manager.acquire_lease("open").unwrap();
        sweep(user, "current", SystemTime::now(), &policy(0, 0));
        assert!(open.exists(), "a session some runtime holds is never swept");
        assert!(current.exists(), "nor the one opening now");
        drop(lease);
        sweep(user, "current", SystemTime::now(), &policy(0, 0));
        assert!(!open.exists());
    }

    #[test]
    fn an_orphan_store_waits_out_its_grace() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let young = store(user, "p", "young", 10, Duration::from_secs(60));
        let stale = store(user, "p", "stale", 10, HOUR * 2);
        sweep(user, "", SystemTime::now(), &policy(0, 0));
        assert!(young.exists(), "a store may precede its session's files");
        assert!(!stale.exists());
    }

    #[test]
    fn only_marked_store_directories_are_touched() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let foreign = user.join("rewind").join("p").join("not-a-store");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("file"), b"x").unwrap();
        sweep(user, "", SystemTime::now(), &policy(0, 0));
        assert_eq!(
            remove_session_stores(&user.join("rewind"), "not-a-store"),
            0
        );
        assert!(foreign.exists());
    }

    #[test]
    fn a_session_store_is_found_in_any_bucket() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let a = store(user, "worktree-root", "id", 1, HOUR);
        let keep = store(user, "worktree-root", "other", 1, HOUR);
        assert_eq!(remove_session_stores(&user.join("rewind"), "id"), 1);
        assert!(!a.exists() && keep.exists());
    }

    #[test]
    fn a_store_owed_a_recovery_is_kept() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        // A restore stopped part way: the journal sits beside the session.
        session(user, "s", "journal");
        fs::write(
            SessionManager::sessions_root(&dirs(user))
                .join("s")
                .join("journal.rewind.txn"),
            b"{}",
        )
        .unwrap();
        let journal = store(user, "p", "journal", 10, HOUR * 5);
        // Or only the pin in the store survives, its session long gone.
        let pinned = store(user, "p", "pinned", 10, HOUR * 5);
        let refs = pinned.join("refs").join("atomcode").join("recovery");
        fs::create_dir_all(&refs).unwrap();
        fs::write(refs.join("current"), b"0000").unwrap();
        let packed = store(user, "p", "packed", 10, HOUR * 5);
        fs::write(
            packed.join("packed-refs"),
            b"0000 refs/atomcode/recovery/current\n",
        )
        .unwrap();
        sweep(user, "", SystemTime::now(), &policy(0, 0));
        assert!(journal.exists() && pinned.exists() && packed.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_bucket_is_never_followed() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("id");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join(STORE_MARKER), "atomcode-rewind-v1").unwrap();
        fs::create_dir_all(user.join("rewind")).unwrap();
        std::os::unix::fs::symlink(outside.path(), user.join("rewind").join("link")).unwrap();
        sweep(user, "", SystemTime::now() + HOUR * 48, &policy(0, 0));
        assert_eq!(remove_session_stores(&user.join("rewind"), "id"), 0);
        assert!(victim.exists());
    }

    #[test]
    fn an_id_that_is_not_a_store_name_is_refused() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        // `..` would name the bucket itself, which carries no marker here; a
        // marked one proves the id never reaches a path.
        let bucket = user.join("rewind").join("p");
        fs::create_dir_all(&bucket).unwrap();
        fs::write(bucket.join(STORE_MARKER), "atomcode-rewind-v1").unwrap();
        for id in ["..", ".", "a/b", "", "x~y"] {
            assert_eq!(remove_session_stores(&user.join("rewind"), id), 0, "{id:?}");
        }
        assert!(bucket.exists());
    }

    #[test]
    fn an_unfinished_delete_is_finished_by_the_next_sweep() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let leftover = user.join("rewind").join(".trash").join("p-old-1");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join(STORE_MARKER), "atomcode-rewind-v1").unwrap();
        sweep(user, "", SystemTime::now(), &policy(20, u64::MAX));
        assert!(!leftover.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_trash_is_neither_used_nor_emptied() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let outside = tempfile::tempdir().unwrap();
        let marked = outside.path().join("marked");
        fs::create_dir_all(&marked).unwrap();
        fs::write(marked.join(STORE_MARKER), "atomcode-rewind-v1").unwrap();
        fs::write(outside.path().join("file"), b"x").unwrap();
        let old = store(user, "p", "old", 10, HOUR * 48);
        std::os::unix::fs::symlink(outside.path(), user.join("rewind").join(".trash")).unwrap();
        sweep(user, "", SystemTime::now(), &policy(0, 0));
        assert!(marked.exists() && outside.path().join("file").exists());
        assert!(
            old.exists(),
            "nothing is moved into a trash outside the tree"
        );
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 2);
    }

    #[test]
    fn the_trash_keeps_what_a_retire_did_not_put_there() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let unmarked = user.join("rewind").join(".trash").join("someone-elses");
        fs::create_dir_all(&unmarked).unwrap();
        fs::write(unmarked.join("file"), b"x").unwrap();
        sweep(user, "", SystemTime::now(), &policy(20, u64::MAX));
        assert!(unmarked.exists());
    }

    /// `chmod 000` does not stop a superuser; such a run proves nothing.
    #[cfg(unix)]
    fn unreadable(dir: &Path) -> Option<impl Drop> {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
        let restore = Restore(dir.to_path_buf());
        fs::read_dir(dir).is_err().then_some(restore)
    }

    #[cfg(unix)]
    #[test]
    fn a_recovery_state_that_cannot_be_read_keeps_the_store() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        let orphan = store(user, "p", "orphan", 10, HOUR * 48);
        let refs = orphan.join("refs").join("atomcode").join("recovery");
        fs::create_dir_all(&refs).unwrap();
        let Some(_restore) = unreadable(&refs) else {
            return;
        };
        sweep(user, "", SystemTime::now(), &policy(0, 0));
        assert!(orphan.exists(), "unreadable is not the same as absent");
    }

    #[cfg(unix)]
    #[test]
    fn a_session_directory_that_cannot_be_read_does_not_make_an_orphan() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        session(user, "s", "hidden");
        let hidden = store(user, "p", "hidden", 10, HOUR * 48);
        let Some(_restore) = unreadable(&SessionManager::sessions_root(&dirs(user)).join("s"))
        else {
            return;
        };
        sweep(user, "", SystemTime::now(), &policy(0, 0));
        assert!(hidden.exists(), "its session may still owe a recovery");
    }

    #[test]
    fn a_store_written_after_the_scan_waits_for_the_next_sweep() {
        let user = tempfile::tempdir().unwrap();
        let user = user.path();
        session(user, "s", "resumed");
        let resumed = store(user, "p", "resumed", 10, HOUR * 48);
        let orphan = store(user, "p", "orphan", 10, HOUR * 48);
        let rewind_root = user.join("rewind");
        let sessions_root = SessionManager::sessions_root(&dirs(user));
        let scanned = list_stores(&rewind_root);
        // Between the scan and the eviction both were written again.
        for dir in [&resumed, &orphan] {
            fs::File::options()
                .write(true)
                .open(dir.join("index"))
                .unwrap()
                .set_modified(SystemTime::now())
                .unwrap();
        }
        let policy = policy(0, 0);
        for candidate in &scanned {
            assert!(!evict(
                candidate,
                &rewind_root,
                &sessions_root,
                "",
                SystemTime::now() + HOUR * 48,
                &policy
            ));
        }
        assert!(resumed.exists() && orphan.exists());
        // Ranked again, the resumed session's store is judged on what it is now.
        sweep(user, "", SystemTime::now(), &policy);
        assert!(!resumed.exists(), "nothing protects it once ranked afresh");
    }

    #[test]
    fn the_rewind_root_sits_beside_sessions() {
        assert_eq!(
            rewind_root_for(Path::new("/u/sessions/abc")),
            Some(PathBuf::from("/u/rewind"))
        );
        assert_eq!(rewind_root_for(Path::new("/tmp/custom")), None);
    }
}
