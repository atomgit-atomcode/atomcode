// crates/atomcode-core/src/coding_plan/sync_marker.rs
//
// Persist the last time `/codingplan` successfully wrote provider entries
// to `config.toml`. The monitor (tuix side) reads this to decide whether
// to surface a "list has changed, re-run /codingplan" hint when server
// state drifts from local config > 24h later.
//
// The file lives next to `config.toml` (same `$ATOMCODE_HOME` / `~/.atomcode`
// resolution) and carries a single ISO-8601 timestamp. Any I/O failure —
// missing file, corrupt JSON, unparseable timestamp — is treated as
// "never synced" rather than an error: the caller then applies the
// stale-threshold logic against `None`, which conservatively surfaces
// a hint as soon as lists actually differ.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const FILE_NAME: &str = "codingplan_sync.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncMarker {
    /// Unix seconds. We use a numeric timestamp instead of an RFC3339
    /// string because the only consumer compares against "now - 24h";
    /// no need for a chrono dep just to format a human-readable string
    /// that isn't displayed anywhere.
    last_sync_unix_secs: u64,
}

fn marker_path() -> PathBuf {
    Config::config_dir().join(FILE_NAME)
}

/// Read the last-sync timestamp. Returns `None` when the file doesn't
/// exist, fails to parse, or is unreadable — all of which the caller
/// interprets as "treat as stale". This function never returns an error.
pub fn read_last_sync() -> Option<SystemTime> {
    let path = marker_path();
    let bytes = std::fs::read(&path).ok()?;
    let marker: SyncMarker = serde_json::from_slice(&bytes).ok()?;
    UNIX_EPOCH.checked_add(std::time::Duration::from_secs(marker.last_sync_unix_secs))
}

/// Write the current wall-clock as the last-sync timestamp. Creates the
/// parent directory if missing. On any I/O failure returns an error so
/// the caller (the `/codingplan` persist path) can log it — but callers
/// should NOT treat a failed marker write as fatal: the provider config
/// already landed on disk in that case, only the monitor hint is lost.
pub fn write_last_sync_now() -> std::io::Result<()> {
    let path = marker_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let marker = SyncMarker {
        last_sync_unix_secs: now,
    };
    let json = serde_json::to_vec_pretty(&marker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integration-style test using a scoped ATOMCODE_HOME override.
    /// We can't safely mutate the process env in parallel tests, so each
    /// test creates its own tempdir and restores the env on exit via a
    /// guard. Tests are serialized by `#[serial]`? — we don't have the
    /// crate; instead rely on cargo test's default single-threaded per
    /// target is false, so we scope env changes inside a Mutex.
    use std::sync::Mutex;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct ScopedHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl ScopedHome {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("ATOMCODE_HOME").ok();
            let dir = tempfile::tempdir().expect("tempdir");
            std::env::set_var("ATOMCODE_HOME", dir.path());
            Self {
                _guard: guard,
                prev,
                _dir: dir,
            }
        }
    }

    impl Drop for ScopedHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("ATOMCODE_HOME", v),
                None => std::env::remove_var("ATOMCODE_HOME"),
            }
        }
    }

    #[test]
    fn write_then_read_round_trips_timestamp() {
        let _home = ScopedHome::new();
        write_last_sync_now().expect("write");
        let t = read_last_sync().expect("read");
        // Should be within a few seconds of `now` — we don't assert
        // exact equality because the serialize/deserialize path passes
        // through integer-second truncation.
        let now = SystemTime::now();
        let diff = now
            .duration_since(t)
            .expect("read timestamp should be <= now");
        assert!(
            diff.as_secs() < 5,
            "round-tripped timestamp should be within 5s of now, got {}s",
            diff.as_secs()
        );
    }

    #[test]
    fn read_last_sync_returns_none_when_file_absent() {
        let _home = ScopedHome::new();
        // No write — file doesn't exist.
        assert!(read_last_sync().is_none());
    }

    #[test]
    fn read_last_sync_returns_none_on_corrupt_json() {
        let _home = ScopedHome::new();
        let path = marker_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"not json at all").unwrap();
        assert!(read_last_sync().is_none());
    }
}
