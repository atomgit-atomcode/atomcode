//! OSC 11 terminal-background-colour detection.
//!
//! Queries the active terminal for its background colour and decides
//! light vs. dark. Used at startup when `Config::ui.theme == Auto` to
//! pick the right colour palette. On terminals that don't respond
//! (macOS Terminal.app, Windows conhost), returns `None` and the
//! caller falls back to the legacy dark palette.
//!
//! Must be called with raw mode active — otherwise the response is
//! line-buffered by the kernel and never reaches us within the timeout.

use std::time::Duration;

/// Query the terminal for its background colour and decide light vs.
/// dark. Returns `Some(true)` for light, `Some(false)` for dark,
/// `None` when the terminal didn't respond within `timeout`.
///
/// Implementation (Unix):
///   1. Write OSC 11 query (`ESC ] 11 ; ? BEL`) to stdout.
///   2. Wait up to `timeout` for stdin to become readable
///      (`libc::poll`, single fd).
///   3. Read available bytes via `libc::read`, parse the
///      `rgb:RRRR/GGGG/BBBB` payload.
///   4. Compute Rec. 709 relative luminance, threshold at 128/255.
///
/// Windows / non-Unix: returns `None` immediately. Windows conhost
/// doesn't respond to OSC 11 at all; Windows Terminal does but the
/// `libc::poll` path isn't available there. A future improvement can
/// add a Win32-specific path via PeekConsoleInput.
pub fn detect_light(timeout: Duration) -> Option<bool> {
    #[cfg(unix)]
    {
        detect_light_unix(timeout)
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        None
    }
}

#[cfg(unix)]
fn detect_light_unix(timeout: Duration) -> Option<bool> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let mut stdout = std::io::stdout();
    // OSC 11 query — request background colour. BEL terminator
    // (`\x07`) is the de-facto default for xterm-family terminals;
    // emulators that prefer ST (`\x1b\\`) accept BEL too in practice.
    stdout.write_all(b"\x1b]11;?\x07").ok()?;
    stdout.flush().ok()?;

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();

    // Two-phase budget.
    //
    // Phase 1 (main read): caller's contract — up to `timeout` for the
    // FIRST byte. If nothing arrives in this window the terminal is
    // non-responsive (macOS Terminal.app, classic conhost, OSC-stripping
    // SSH relays) and we fall straight through to the dark-mode default
    // without blocking startup further. If we DO see an OSC opener
    // (`\x1b]`) within `timeout`, we extend by 250ms so the trailing
    // bytes of a slow / chunked response can land — without this a
    // partial OSC 11 reply (e.g. JediTerm, remote relays, Windows
    // Terminal under load) leaks past the original single-shot read
    // and the crossterm reader thread later picks those bytes up as
    // keystrokes. The visible bug was `]11;rgb:0000/0000/0000\` (and
    // shorter prefixes like `0c/0c0c\`) appearing in the input box.
    //
    // Phase 2 (tail-drain): handles the case where the OSC response
    // started arriving AFTER `timeout` — Phase 1 already broke out
    // empty-handed, but the bytes are about to land. We spend an extra
    // 80ms peeking; we only commit to bulk-draining once we've confirmed
    // a `\x1b]` opener, so a user keystroke that happens to land in
    // this window loses at most one or two bytes (vs. the bulk read in
    // Phase 1 which would swallow up to 128). In practice the input
    // prompt isn't on screen during the 100–180ms window so the user
    // hasn't started typing yet, but the byte-at-a-time probe keeps
    // the worst case bounded if they have.
    let start = std::time::Instant::now();
    let initial_deadline = start + timeout;
    let extended_deadline = initial_deadline + Duration::from_millis(250);

    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut saw_osc_start = false;

    // Phase 1.
    loop {
        let deadline = if saw_osc_start { extended_deadline } else { initial_deadline };
        let mut chunk = [0u8; 128];
        // SAFETY: chunk is stack-allocated and lives for the call; fd
        // is owned by stdin for the process lifetime.
        let nread = unsafe { poll_read(fd, deadline, &mut chunk) };
        if nread == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..nread]);

        // Lock in the deadline extension the first time we see the OSC
        // opener; from here on Phase 1 keeps draining until terminator.
        if !saw_osc_start {
            saw_osc_start = buf.windows(2).any(|w| w == b"\x1b]");
            // First bytes weren't an OSC opener — almost certainly a
            // stray keystroke that landed in the input queue before
            // our query. Don't keep slurping their input in bulk-read
            // mode; if the OSC reply is still coming, Phase 2 will
            // catch it.
            if !saw_osc_start {
                break;
            }
        }

        if has_osc_terminator(&buf) {
            break;
        }
    }

    // Phase 2: tail-drain. Only runs when Phase 1 didn't already
    // consume a complete OSC reply (terminator absent from `buf`).
    if !has_osc_terminator(&buf) {
        let tail_deadline = std::time::Instant::now() + Duration::from_millis(80);
        // If Phase 1 already saw the opener, we're committed to bulk
        // draining. Otherwise we probe a byte at a time.
        let mut committed = saw_osc_start;
        loop {
            if committed {
                let mut chunk = [0u8; 128];
                // SAFETY: same invariants as the Phase 1 read.
                let nread = unsafe { poll_read(fd, tail_deadline, &mut chunk) };
                if nread == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..nread]);
            } else {
                // Peek the first byte. Anything other than ESC means
                // it's not an OSC reply — stop immediately so we don't
                // keep eating user input.
                let mut probe = [0u8; 1];
                // SAFETY: see Phase 1.
                let nread = unsafe { poll_read(fd, tail_deadline, &mut probe) };
                if nread == 0 {
                    break;
                }
                if probe[0] != b'\x1b' {
                    buf.push(probe[0]);
                    break;
                }
                buf.push(probe[0]);
                // ESC seen — disambiguate `\x1b]` (OSC, what we want)
                // from `\x1b[` / `\x1bO` / bare ESC (which we leave
                // alone). The terminal may send ESC and pause briefly
                // before the next byte; poll again with what's left
                // of the tail budget.
                let mut probe2 = [0u8; 1];
                // SAFETY: see Phase 1.
                let nread2 = unsafe { poll_read(fd, tail_deadline, &mut probe2) };
                if nread2 == 0 {
                    break;
                }
                buf.push(probe2[0]);
                if probe2[0] != b']' {
                    break;
                }
                committed = true;
            }

            if has_osc_terminator(&buf) {
                break;
            }
        }
    }

    parse_osc11_response(&buf)
}

/// True when `buf` contains an OSC terminator (BEL or ESC \). Used by
/// both phases of [`detect_light_unix`] to know when an in-flight OSC
/// reply is fully drained.
#[cfg(unix)]
fn has_osc_terminator(buf: &[u8]) -> bool {
    buf.contains(&b'\x07') || buf.windows(2).any(|w| w == b"\x1b\\")
}

/// Wait until `deadline` for `fd` to be readable, then read up to
/// `out.len()` bytes into `out`. Returns the number of bytes read, or
/// `0` on timeout / poll error / EOF / read error. Factored out so
/// the two phases of [`detect_light_unix`] share one poll-then-read
/// path with identical clamping and SAFETY invariants.
///
/// # Safety
/// `fd` must be a valid file descriptor for the entire call; `out`
/// must be writable for `out.len()` bytes.
#[cfg(unix)]
unsafe fn poll_read(fd: i32, deadline: std::time::Instant, out: &mut [u8]) -> usize {
    let now = std::time::Instant::now();
    if now >= deadline {
        return 0;
    }
    // poll() ms argument is i32; clamp to its range.
    let ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = libc::poll(&mut pollfd, 1, ms);
    if n <= 0 {
        return 0;
    }
    let nread = libc::read(fd, out.as_mut_ptr() as *mut libc::c_void, out.len());
    if nread <= 0 {
        0
    } else {
        nread as usize
    }
}

/// Parse an OSC 11 reply of shape `ESC ] 11 ; rgb:RRRR/GGGG/BBBB BEL`
/// (or `ESC \\` ST terminator). Returns `Some(is_light)` when a usable
/// RGB triplet is found.
///
/// Tolerates leading garbage (pre-existing keystrokes in stdin) by
/// scanning for `rgb:`. Tolerates trailing garbage (BEL / ST / partial
/// next response) by stopping at the first non-hex char.
///
/// Gated to `unix` + `test` builds: the only non-test caller is
/// `detect_light_unix`, and Windows has no OSC 11 read path (its
/// stub `detect_light` returns `None`). Without this gate `cargo
/// build` on Windows warns `dead_code` here.
#[cfg(any(unix, test))]
pub(crate) fn parse_osc11_response(bytes: &[u8]) -> Option<bool> {
    // Allow non-UTF-8 prefix bytes (a stray keystroke could be any
    // byte); slice to the start of `rgb:` and parse from there as
    // ASCII (which it is — the OSC 11 reply is pure ASCII).
    let needle = b"rgb:";
    let rgb_pos = bytes.windows(needle.len()).position(|w| w == needle)?;
    let after = std::str::from_utf8(&bytes[rgb_pos + needle.len()..]).ok()?;

    let mut parts = after.split('/');
    let r_raw = parts.next()?;
    let g_raw = parts.next()?;
    let b_raw = parts.next()?;

    let r = parse_hex_component(r_raw)?;
    let g = parse_hex_component(g_raw)?;
    let b = parse_hex_component(b_raw)?;

    // Rec. 709 relative luminance, components in 0..=255.
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    Some(lum > 128.0)
}

/// Parse one OSC 11 colour component. xterm returns 4 hex chars (16-bit
/// precision); some emulators return 2 (8-bit) or even 1. Reads
/// hex-digit-prefix-only and normalises to 0..=255 based on observed
/// width — so `rgb:ff/ff/ff` and `rgb:ffff/ffff/ffff` both come out
/// as 255.0.
///
/// Mirror cfg gate of `parse_osc11_response` — only that function and
/// the tests reach this helper, so Windows non-test builds would
/// otherwise flag it as dead code.
#[cfg(any(unix, test))]
fn parse_hex_component(s: &str) -> Option<f64> {
    let hex: String = s.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if hex.is_empty() {
        return None;
    }
    let val = u32::from_str_radix(&hex, 16).ok()?;
    // 4-char hex → max 0xFFFF = 65535; 2-char → 0xFF = 255; 1-char → 0xF = 15.
    let max = (1u64 << (4 * hex.len())).saturating_sub(1) as u32;
    if max == 0 {
        return None;
    }
    Some((val as f64 * 255.0) / max as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pure_white_as_light() {
        let response = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc11_response(response), Some(true));
    }

    #[test]
    fn parses_pure_black_as_dark() {
        let response = b"\x1b]11;rgb:0000/0000/0000\x07";
        assert_eq!(parse_osc11_response(response), Some(false));
    }

    #[test]
    fn parses_8bit_response() {
        // Some emulators (older xterm builds) return 2-hex-char per channel.
        let response = b"\x1b]11;rgb:ff/ff/ff\x07";
        assert_eq!(parse_osc11_response(response), Some(true));
    }

    #[test]
    fn parses_vscode_dark_plus() {
        // VSCode "Dark+" editor background ≈ #1E1E1E (30,30,30).
        let response = b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07";
        assert_eq!(parse_osc11_response(response), Some(false));
    }

    #[test]
    fn parses_vscode_light_plus() {
        // VSCode "Light+" editor background ≈ #FFFFFF.
        let response = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc11_response(response), Some(true));
    }

    #[test]
    fn parses_st_terminator() {
        // ESC \ is the spec-correct terminator; some emulators (notably
        // st itself) emit it instead of BEL.
        let response = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        assert_eq!(parse_osc11_response(response), Some(true));
    }

    #[test]
    fn tolerates_leading_garbage() {
        // A stray keystroke landed in stdin before the OSC reply.
        let response = b"q\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc11_response(response), Some(true));
    }

    #[test]
    fn rejects_no_rgb_prefix() {
        assert_eq!(parse_osc11_response(b""), None);
        assert_eq!(parse_osc11_response(b"random bytes"), None);
        assert_eq!(parse_osc11_response(b"\x1b[A"), None); // arrow key
    }

    #[test]
    fn rejects_truncated_response() {
        assert_eq!(parse_osc11_response(b"\x1b]11;rgb:"), None);
        assert_eq!(parse_osc11_response(b"\x1b]11;rgb:ff/"), None);
        assert_eq!(parse_osc11_response(b"\x1b]11;rgb:ff/ff"), None);
    }

    #[test]
    fn threshold_at_50_percent_grey_is_dark() {
        // Pure 50% grey: lum = 128 exactly. `> 128` means 128 stays
        // dark. Pin this so a refactor doesn't flip the boundary
        // (typical "near-50% grey theme" should default to dark since
        // most users intend dark with mid-grey backgrounds).
        let response = b"\x1b]11;rgb:8080/8080/8080\x07";
        assert_eq!(parse_osc11_response(response), Some(false));
    }

    #[test]
    fn threshold_one_above_50_percent_grey_is_light() {
        // 129/255 → luminance just over 128 → light.
        let response = b"\x1b]11;rgb:8181/8181/8181\x07";
        assert_eq!(parse_osc11_response(response), Some(true));
    }

    #[test]
    fn luminance_weights_green_more_than_red_or_blue() {
        // Rec. 709: G dominates. Pure green should be brighter than
        // pure red. (255 * 0.7152 = 182.4 > 128 → light.)
        let pure_green = b"\x1b]11;rgb:0000/ffff/0000\x07";
        assert_eq!(parse_osc11_response(pure_green), Some(true));

        // Pure red: 255 * 0.2126 = 54.2 → dark.
        let pure_red = b"\x1b]11;rgb:ffff/0000/0000\x07";
        assert_eq!(parse_osc11_response(pure_red), Some(false));

        // Pure blue: 255 * 0.0722 = 18.4 → dark.
        let pure_blue = b"\x1b]11;rgb:0000/0000/ffff\x07";
        assert_eq!(parse_osc11_response(pure_blue), Some(false));
    }
}
