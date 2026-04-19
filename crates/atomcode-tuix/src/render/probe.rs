// crates/atomcode-tuix/src/render/probe.rs
//
// Terminal DSR (Device Status Report) round-trip probe.
//
// Measures how long it takes the terminal emulator to *actually finish
// processing* whatever we've written to stdout — not just the "kernel
// pipe has accepted the bytes" time that `Flush flush=Nµs` measures.
// The difference matters for Mac Terminal.app, which happily gulps
// bytes from the pipe but renders them to pixels asynchronously on its
// own GUI loop. User-perceived "sluggish / swallowed typing" is almost
// always on the GUI-pipeline side, invisible to `flush()`.
//
// ## Protocol
//
// Write `\x1b[6n` (Cursor Position Report). Terminal must process every
// byte queued before this query — including all the footer redraws — to
// know what cursor row/col to report. Then it responds on stdin with
// `\x1b[{row};{col}R`.
//
// Round-trip time ≈ "time terminal needed to catch up to the point
// where the probe byte was injected". High numbers = backlog building
// up in the terminal's internal queue.
//
// ## Why this is invasive
//
// The response lands on stdin, which our `reader` thread owns and
// parses through crossterm. Since crossterm may or may not recognise
// CPR responses as a well-typed event (and would at best emit them as
// stray Key events that pollute the input buffer), the probe suspends
// the reader with the same Pause/Resume protocol used by the OAuth
// flow, does a synchronous read itself, and restores the reader.
//
// Per-probe overhead = pause_blocking() + one write + one poll/read +
// resume() ≈ 200-400µs even in the "everything is fast" case. So the
// probe runs on a timer (ATOMCODE_TUIX_PROBE_MS env var; default off)
// rather than per-paint. 500ms is a good default — enough to catch
// sustained backlog without hammering the reader.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crate::input::reader::ReaderHandle;

/// Read CPR response from stdin with a timeout. Returns the response
/// bytes (up to and including the terminating `R`) if one arrived, or
/// `None` on timeout.
///
/// Uses raw `libc::poll` on unix so we can bound wait-time cleanly
/// without fighting crossterm's own event::poll (which might parse
/// the CPR as a stray key event).
#[cfg(unix)]
fn read_cpr_with_timeout(timeout: Duration) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let remain_ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
        let n = unsafe { libc::poll(&mut pollfd, 1, remain_ms) };
        if n <= 0 {
            // 0 = timeout, -1 = interrupt; we treat both as "no data yet".
            return if !buf.is_empty() { Some(buf) } else { None };
        }
        if pollfd.revents & libc::POLLIN == 0 {
            return if !buf.is_empty() { Some(buf) } else { None };
        }
        // Read what's available. stdin is in raw mode so `read` returns
        // whatever bytes are currently queued without waiting for LF.
        let mut chunk = [0u8; 32];
        let r = unsafe {
            libc::read(
                fd,
                chunk.as_mut_ptr() as *mut libc::c_void,
                chunk.len(),
            )
        };
        if r <= 0 {
            return if !buf.is_empty() { Some(buf) } else { None };
        }
        buf.extend_from_slice(&chunk[..r as usize]);
        // CPR response ends in 'R'. If we see one, we're done.
        if buf.contains(&b'R') {
            return Some(buf);
        }
    }
}

#[cfg(not(unix))]
fn read_cpr_with_timeout(_timeout: Duration) -> Option<Vec<u8>> {
    // Windows path not implemented — probe only runs on Unix for now.
    None
}

/// Issue one DSR probe and return the round-trip time.
///
/// Pauses the reader (so crossterm doesn't eat the CPR response),
/// writes `\x1b[6n`, polls stdin for the response with `timeout`,
/// resumes the reader. Returns `Some(rtt)` on success, `None` on
/// pause/resume failure or read timeout.
pub fn probe(reader: Option<&ReaderHandle>, timeout: Duration) -> Option<Duration> {
    let reader = reader?;
    // Pause reader so the CPR response comes to us, not crossterm.
    if reader.pause_blocking().is_err() {
        return None;
    }
    let rtt = {
        let start = Instant::now();
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let write_ok = out.write_all(b"\x1b[6n").is_ok() && out.flush().is_ok();
        drop(out);
        if !write_ok {
            reader.resume();
            return None;
        }
        let response = read_cpr_with_timeout(timeout);
        let elapsed = start.elapsed();
        if response.is_some() {
            Some(elapsed)
        } else {
            None
        }
    };
    reader.resume();
    rtt
}
