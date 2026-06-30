//! Async-signal-safe terminal restore on fatal signals (Unix).
//!
//! `TerminalGuard::Drop` (lib.rs) restores the terminal on a graceful exit,
//! and the panic hook covers `panic = "abort"`. But a process **killed by a
//! signal** (`kill`/SIGTERM, terminal close/SIGHUP, or `kill -INT`) runs
//! NEITHER — the kernel tears the process down without unwinding. The shell
//! then inherits a terminal still in raw mode with the Kitty keyboard protocol
//! and bracketed paste armed, plus the leftover `❯` input row, so subsequent
//! keystrokes echo as CSI-u / `200~` gibberish.
//!
//! This is the reported "Ctrl-C twice to exit writes junk into the input box":
//! the v2 quit chain wedged past the force-exit watchdog, so the TUI was
//! ultimately signal-killed (`zsh: terminated …`) instead of exiting cleanly,
//! and nothing restored the terminal.
//!
//! We install a raw `sigaction` handler — NOT a `tokio` signal task, which the
//! very wedge that triggers the kill would starve. Using only async-signal-safe
//! calls (`write`, `tcsetattr`, `raise`), it emits the restore byte sequence,
//! takes the terminal out of raw mode via the cooked `termios` captured at arm
//! time, then re-raises the signal under the default disposition so the parent
//! still observes the true signal exit status.

use core::ptr::{addr_of, addr_of_mut};
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static TERMIOS_SAVED: AtomicBool = AtomicBool::new(false);
/// Cooked terminal attributes captured before raw mode is enabled, restored
/// verbatim from signal context (`tcsetattr` is async-signal-safe).
static mut ORIG_TERMIOS: core::mem::MaybeUninit<libc::termios> =
    core::mem::MaybeUninit::uninit();

/// The bytes the signal handler emits to restore the terminal: the canonical
/// `panic_restore_sequence` (Kitty-keyboard pop, mouse off, cursor show,
/// autowrap, scroll-region release, bracketed-paste off, CRLF). Reused so the
/// restore contract lives in ONE place rather than re-appended per exit path.
///
/// Returns a static slice (no allocation) so it is callable from signal context,
/// and pure, hence unit-testable without delivering a real signal.
pub(crate) fn restore_writes() -> &'static [u8] {
    crate::panic_restore_sequence()
}

extern "C" fn handler(signo: c_int) {
    // async-signal-safe calls ONLY below.
    let seq = restore_writes();
    unsafe {
        let _ = libc::write(libc::STDOUT_FILENO, seq.as_ptr().cast(), seq.len());
    }
    // Acquire pairs with the SeqCst (release) store in `arm()` so the termios
    // bytes tcgetattr wrote are visible here even when the signal is delivered
    // on a different thread (weakly-ordered targets, e.g. aarch64).
    if TERMIOS_SAVED.load(Ordering::Acquire) {
        unsafe {
            libc::tcsetattr(
                libc::STDIN_FILENO,
                libc::TCSANOW,
                addr_of!(ORIG_TERMIOS).cast::<libc::termios>(),
            );
        }
    }
    // Re-raise under the default disposition so the exit status still reflects
    // the signal (the shell's "terminated"/"interrupt" message is correct — we
    // only cleaned the terminal first).
    unsafe {
        libc::signal(signo, libc::SIG_DFL);
        libc::raise(signo);
    }
}

/// Capture the cooked `termios` (before raw mode flips it) and install the
/// terminal-restore handler for SIGTERM / SIGINT / SIGHUP. Idempotent — only the
/// first call takes effect. Call this immediately before `enable_raw_mode()`.
pub(crate) fn arm() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if libc::tcgetattr(
            libc::STDIN_FILENO,
            addr_of_mut!(ORIG_TERMIOS).cast::<libc::termios>(),
        ) == 0
        {
            TERMIOS_SAVED.store(true, Ordering::SeqCst);
        }
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = handler as *const () as libc::sighandler_t;
        // Block the signals we manage during the handler so a second one can't
        // re-enter it mid-restore and kill us under the wrong signal's status.
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaddset(&mut sa.sa_mask, libc::SIGTERM);
        libc::sigaddset(&mut sa.sa_mask, libc::SIGINT);
        libc::sigaddset(&mut sa.sa_mask, libc::SIGHUP);
        sa.sa_flags = 0;
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(sig, &sa, core::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::restore_writes;

    /// The whole point over the panic sequence: a signal-kill must ALSO disable
    /// bracketed paste, or the shell wraps every paste in `200~`/`201~`. And it
    /// must still pop the Kitty protocol + show the cursor (else CSI-u echo /
    /// invisible cursor). Pins the exact restore contract the handler emits.
    #[test]
    fn signal_restore_disables_bracketed_paste_and_pops_kitty() {
        let text = String::from_utf8_lossy(restore_writes());
        assert!(
            text.contains("\x1b[?2004l"),
            "must disable bracketed paste (the panic sequence omits it): {text:?}"
        );
        assert!(text.contains("\x1b[<1u"), "must pop Kitty keyboard: {text:?}");
        assert!(text.contains("\x1b[?25h"), "must show the cursor: {text:?}");
    }
}
