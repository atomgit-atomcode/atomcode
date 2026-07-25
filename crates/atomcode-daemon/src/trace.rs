// Diagnostic file-trace shared with `atomcode_tuix::trace`. Enabled via
// `ATOMCODE_TUIX_LOG=/path/to/file`. Opens in append mode so both crates
// can target the same file without one truncating the other's entries.
//
// Format matches the tuix trace: `+{us} [{CAT}] {tid} {message}`. Use
// short 2-4 char categories so columns stay grep-able:
//   AGT — agent loop (Cancel intake)
//   RNR — turn runner (tool select / cancel branches)
//   TOOL — tool implementations (web_search etc.)
//
// Opt-in only — `enabled()` is a single atomic load + branch when the
// env var is unset, so leaving trace points scattered through hot paths
// costs nothing in release.

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
    SINK.get_or_init(|| {
        let path = std::env::var("ATOMCODE_TUIX_LOG").ok()?;
        if path.is_empty() {
            return None;
        }
        // Append (not truncate) — atomcode_tuix::trace opens with
        // truncate during reader init; we'd otherwise lose its early
        // events if init order flipped. POSIX O_APPEND keeps writes
        // atomic for our line-sized payloads, so interleaving with
        // the tuix sink is safe even though they own separate Mutex
        // instances.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(Mutex::new(file))
    })
    .as_ref()
}

fn origin() -> Instant {
    *ORIGIN.get_or_init(Instant::now)
}

pub fn write_line(cat: &str, args: std::fmt::Arguments<'_>) {
    let Some(sink) = sink() else {
        return;
    };
    let us = origin().elapsed().as_micros();
    let tid = std::thread::current().name().unwrap_or("?").to_string();
    let line = format!("+{:>10}us [{:>3}] {:>14} {}\n", us, cat, tid, args);
    if let Ok(mut f) = sink.lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! ctrace {
    ($cat:expr, $($arg:tt)*) => {{
        if $crate::trace::enabled() {
            $crate::trace::write_line($cat, format_args!($($arg)*));
        }
    }};
}
