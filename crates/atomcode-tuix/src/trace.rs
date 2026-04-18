// crates/atomcode-tuix/src/trace.rs
//
// Opt-in file logger for diagnosing event-loop / render timing issues.
//
// Enabled via env var `ATOMCODE_TUIX_LOG=/path/to/file`. When unset
// every `tuix_trace!` call compiles into a no-op fast path (single
// atomic load + branch predict), so leaving trace points scattered
// through hot paths costs nothing in release.
//
// Why not env_logger / tracing? We write to stderr-redirected-to-tty
// in raw mode; any sloppy write corrupts the display. A dedicated file
// sink with explicit category tags keeps the noise off the terminal
// and keeps the format parseable (one event per line, microsecond
// monotonic timestamps).
//
// Format: `+{elapsed_us} [{CAT}] {tid} {message}`
//   elapsed_us — microseconds since the first log event in this process
//   CAT        — 2-4 char category (IN, KEY, PH, THR, REN, RD, QUE)
//   tid        — short thread name or id (so `event_loop` vs `tuix-render`
//                is visible at a glance)

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static ORIGIN: OnceLock<Instant> = OnceLock::new();

pub fn enabled() -> bool {
    sink().is_some()
}

fn sink() -> Option<&'static Mutex<File>> {
    // Skip under `cargo test` — otherwise every test run truncates
    // the user's reproduction log at `/tmp/tuix.log`. Tests opt in
    // explicitly via `ATOMCODE_TUIX_LOG=/path` when they need it.
    #[cfg(test)]
    {
        let Some(_) = std::env::var("ATOMCODE_TUIX_LOG").ok() else {
            return None;
        };
    }

    SINK.get_or_init(|| {
        // Default on — writes to /tmp/tuix.log every run. Set
        // ATOMCODE_TUIX_LOG=/other/path to redirect; set it to an
        // empty string to disable entirely.
        let path = match std::env::var("ATOMCODE_TUIX_LOG") {
            Ok(p) if p.is_empty() => return None,
            Ok(p) => p,
            Err(_) => default_log_path(),
        };
        // Truncate on each run so stale events from prior sessions
        // don't confuse diagnosis.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok()?;
        Some(Mutex::new(file))
    })
    .as_ref()
}

#[cfg(unix)]
fn default_log_path() -> String {
    "/tmp/tuix.log".into()
}

#[cfg(not(unix))]
fn default_log_path() -> String {
    std::env::temp_dir()
        .join("tuix.log")
        .to_string_lossy()
        .into_owned()
}

fn origin() -> Instant {
    *ORIGIN.get_or_init(Instant::now)
}

/// Low-level write. Don't call directly — use the `tuix_trace!` macro.
pub fn write_line(cat: &str, args: std::fmt::Arguments<'_>) {
    let Some(sink) = sink() else {
        return;
    };
    let us = origin().elapsed().as_micros();
    let tid = std::thread::current()
        .name()
        .unwrap_or("?")
        .to_string();
    let line = format!("+{:>10}us [{:>3}] {:>14} {}\n", us, cat, tid, args);
    if let Ok(mut f) = sink.lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `tuix_trace!("CAT", "fmt {}", args)` — compiles to a cheap `enabled()`
/// check when the env var is unset. Use short 2-4 char categories so
/// log lines remain grep-able by column:
///   IN  — input event entering handle_input
///   KEY — key-handler outcome (Redraw / Commit / NoOp)
///   PH  — UiPhase transition (Streaming → Idle, etc.)
///   QUE — type-ahead queue push / pop
///   THR — InputThrottle paint/park decision
///   REN — render worker command processed
///   RD  — raw reader thread event
#[macro_export]
macro_rules! tuix_trace {
    ($cat:expr, $($arg:tt)*) => {{
        if $crate::trace::enabled() {
            $crate::trace::write_line($cat, format_args!($($arg)*));
        }
    }};
}
