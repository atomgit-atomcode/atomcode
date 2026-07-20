//! Public error type. thiserror because atomcode-core is library code.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SetupError {
    // ── Lock ──
    #[error("Setup is already running (PID {pid} @ {host}, started {start_time}). Use --force to override.")]
    LockHeld {
        pid: u32,
        start_time: String,
        host: String,
    },

    #[error("Setup lock io error: {0}")]
    LockIo(#[source] std::io::Error),

    // ── Catch-all wrapper ──
    #[error("Unexpected io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Unexpected error: {0}")]
    Other(#[from] anyhow::Error),
}

pub type SetupResult<T> = Result<T, SetupError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_lock_held_includes_pid_and_host() {
        let e = SetupError::LockHeld {
            pid: 1234,
            start_time: "2026-05-19T10:00:00Z".into(),
            host: "macbook".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("1234"));
        assert!(s.contains("macbook"));
        assert!(s.contains("--force"));
    }

}
