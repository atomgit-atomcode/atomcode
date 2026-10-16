// Daemon file-trace. The historical ATOMCODE_TUIX_LOG environment name remains
// accepted for diagnostic scripts, independently of the retired terminal UI.
// Entries are appended as `+{us} [{CAT}] {tid} {message}`.
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
        // Append so repeated daemon launches retain earlier diagnostics.
        // O_APPEND also keeps line-sized writes ordered across processes.
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
