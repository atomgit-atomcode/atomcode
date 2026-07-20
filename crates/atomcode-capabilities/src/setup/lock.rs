//! Project-level advisory file lock for setup. Dual-rail: `fs2::FileExt::try_lock_exclusive`
//! + PID/start_time sentinel JSON. Sentinel handles sandbox/NFS/container edge cases
//! where flock alone is unreliable.
//!
//! Acquire order:
//! 1. Read `.atomcode/.setup.lock.sentinel` if present. If recorded PID is alive **and**
//!    its start_time matches, return [`LockError::Held`] (unless `force = true`).
//!    Stale sentinel is removed.
//! 2. `try_lock_exclusive` on `.atomcode/.setup.lock` — second rail.
//! 3. Write a fresh sentinel JSON with current PID, start_time, host, version.
//!
//! Drop releases both rails (unlock fs2, rm sentinel) but keeps the `.setup.lock`
//! file so future opens reuse the inode.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

const LOCK_FILE: &str = ".setup.lock";
const SENTINEL_FILE: &str = ".setup.lock.sentinel";

#[derive(Debug, Error)]
pub enum LockError {
    #[error(
        "Setup is already running (PID {pid} @ {host}, started {start_time}). Use --force to override."
    )]
    Held {
        pid: u32,
        start_time: String,
        host: String,
    },
    #[error("Lock acquisition io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct SetupLock {
    fd: File,
    /// Path to the project's `.atomcode/.setup.lock` file. Kept around for
    /// diagnostics and future callers (e.g. error messages, force-cleanup CLI).
    #[allow(dead_code)]
    pub(super) lock_path: PathBuf,
    pub(super) sentinel_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Sentinel {
    pid: u32,
    start_time_nanos: u128,
    host: String,
    atomcode_version: String,
}

fn lock_dir(project_root: &Path) -> PathBuf {
    project_root.join(".atomcode")
}

fn current_pid() -> u32 {
    std::process::id()
}

fn hostname() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string())
}

/// Returns the current process's start_time as nanoseconds since UNIX epoch.
/// sysinfo's `start_time()` returns seconds (u64); multiply to nanos for finer-grained
/// future-proofing (Linux clocktick granularity is jiffy-ish, but the JSON field
/// stays uniform regardless of OS).
fn current_start_time_nanos() -> u128 {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = Pid::from_u32(current_pid());
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new(),
    );
    sys.process(pid)
        .map(|p| (p.start_time() as u128) * 1_000_000_000)
        .unwrap_or(0)
}

fn read_sentinel(path: &Path) -> Option<Sentinel> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// True iff a process with `pid` is currently running **and** its observed
/// start_time (nanos) equals `start_time_nanos`. PID reuse after the previous
/// setup crashed will not falsely report alive because start_time differs.
fn process_alive_at(pid: u32, start_time_nanos: u128) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let target = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        false,
        ProcessRefreshKind::new(),
    );
    match sys.process(target) {
        Some(p) => (p.start_time() as u128) * 1_000_000_000 == start_time_nanos,
        None => false,
    }
}

impl SetupLock {
    pub fn acquire(project_root: &Path, force: bool) -> Result<Self, LockError> {
        let dir = lock_dir(project_root);
        std::fs::create_dir_all(&dir)?;
        let lock_path = dir.join(LOCK_FILE);
        let sentinel_path = dir.join(SENTINEL_FILE);

        // Stage 1: inspect sentinel (primary rail).
        //
        // - If recorded owner is **alive** and !force: report Held with full identity.
        // - If recorded owner is **alive** and force: do NOT delete sentinel yet; let
        //   fs2 be the authority. If fs2 also held, force genuinely cannot take over
        //   (peer still running). If fs2 is releasable, the alive-check raced and the
        //   peer just exited — proceed with takeover (re-read in Stage 2 surfaces who).
        // - If recorded owner is **stale** (dead PID or start_time mismatch): delete
        //   sentinel so fs2 isn't confused by a leftover file.
        let sentinel_owner: Option<Sentinel> = read_sentinel(&sentinel_path);
        let owner_alive = sentinel_owner
            .as_ref()
            .is_some_and(|s| process_alive_at(s.pid, s.start_time_nanos));

        if let Some(meta) = sentinel_owner.as_ref() {
            if owner_alive && !force {
                return Err(LockError::Held {
                    pid: meta.pid,
                    start_time: format!("{} ns", meta.start_time_nanos),
                    host: meta.host.clone(),
                });
            }
            if !owner_alive {
                // Stale — clean up so fs2 won't see a leftover file from prior crash.
                let _ = std::fs::remove_file(&sentinel_path);
            }
            // owner_alive && force: keep sentinel for now; fs2 is the authority.
        }

        // Stage 2: fs2 try_lock_exclusive (secondary rail).
        let fd = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;

        if fd.try_lock_exclusive().is_err() {
            // fs2 failed. Re-read sentinel to surface the *real* holder identity.
            // Covers two race/edge cases:
            //   (a) TOCTOU between our stale-sentinel removal and our fs2 attempt:
            //       a sibling wrote a fresh sentinel + grabbed fs2 in the gap.
            //   (b) force=true but the live sibling still holds fs2 — force cannot
            //       take over a running peer; report the real PID so the user knows
            //       whom to kill.
            let live_owner = read_sentinel(&sentinel_path);
            return Err(match live_owner {
                Some(meta) => LockError::Held {
                    pid: meta.pid,
                    start_time: format!("{} ns", meta.start_time_nanos),
                    host: meta.host,
                },
                None => LockError::Held {
                    pid: 0,
                    start_time: "concurrent (sentinel missing/corrupt)".to_string(),
                    host: hostname(),
                },
            });
        }

        // Stage 3: we hold both rails. If force was used against a previously-alive
        // owner, the peer must have released fs2 between Stage 1 and Stage 2 — warn
        // so the operator knows takeover actually fired (the previous version warned
        // unconditionally before fs2 succeeded, which was misleading on failure).
        if force && owner_alive {
            if let Some(meta) = sentinel_owner.as_ref() {
                tracing::warn!(
                    pid = meta.pid,
                    host = %meta.host,
                    "forced setup lock takeover after sibling released fs2 lock"
                );
            }
            // Clean up the prior owner's sentinel before we write our own.
            let _ = std::fs::remove_file(&sentinel_path);
        }

        // Stage 4: write our sentinel.
        let sentinel = Sentinel {
            pid: current_pid(),
            start_time_nanos: current_start_time_nanos(),
            host: hostname(),
            atomcode_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let json = serde_json::to_string(&sentinel).expect("Sentinel serialize never fails");
        let mut f = File::create(&sentinel_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;

        Ok(Self {
            fd,
            lock_path,
            sentinel_path,
        })
    }
}

impl Drop for SetupLock {
    fn drop(&mut self) {
        // Best-effort: errors during drop are intentionally swallowed. If unlock
        // fails the OS will release on process exit; sentinel removal failure
        // just leaves a stale file the next acquire will overwrite.
        let _ = fs2::FileExt::unlock(&self.fd);
        let _ = std::fs::remove_file(&self.sentinel_path);
        // Keep `.setup.lock` file itself so future opens reuse the inode.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_creates_lock_in_fresh_project() {
        let dir = tempfile::tempdir().unwrap();
        let lock = SetupLock::acquire(dir.path(), false).unwrap();
        assert!(lock.lock_path.exists());
        assert!(lock.sentinel_path.exists());
    }

    #[test]
    fn second_acquire_fails_when_first_held() {
        let dir = tempfile::tempdir().unwrap();
        let _lock1 = SetupLock::acquire(dir.path(), false).unwrap();
        let err = SetupLock::acquire(dir.path(), false).unwrap_err();
        assert!(matches!(err, LockError::Held { .. }));
    }

    #[test]
    fn drop_releases_lock_so_next_acquire_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _lock1 = SetupLock::acquire(dir.path(), false).unwrap();
        }
        // drop happened; next acquire should succeed
        let _lock2 = SetupLock::acquire(dir.path(), false).unwrap();
    }

    #[test]
    fn force_with_alive_holder_still_fails_if_fs2_held() {
        // First lock takes both sentinel + fs2.
        let dir = tempfile::tempdir().unwrap();
        let _lock1 = SetupLock::acquire(dir.path(), false).unwrap();

        // Force=true cannot succeed when fs2 is genuinely held by the live sibling.
        // Holder PID should be reported as our own pid (since we wrote the sentinel ourselves).
        let err = SetupLock::acquire(dir.path(), true).unwrap_err();
        match err {
            LockError::Held { pid, .. } => {
                assert_eq!(
                    pid,
                    std::process::id(),
                    "Held should surface real holder pid, not 0"
                );
            }
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn fs2_race_loses_holder_identity_gracefully() {
        // This is hard to truly race in a unit test, but we can simulate the
        // post-condition: a sentinel exists and we attempt to acquire without force.
        // Verify the error carries the sentinel's identity, not pid=0.
        let dir = tempfile::tempdir().unwrap();
        let _lock1 = SetupLock::acquire(dir.path(), false).unwrap();
        let err = SetupLock::acquire(dir.path(), false).unwrap_err();
        match err {
            LockError::Held { pid, .. } => {
                assert_eq!(pid, std::process::id());
            }
            other => panic!("expected Held with real pid, got {other:?}"),
        }
    }
}
