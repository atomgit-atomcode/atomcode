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

/// Whether startup may safely probe the terminal background with OSC 11.
///
/// Orca 1.4.180 has been observed delivering the OSC 11 reply after the
/// following DA1 reply. That violates the ordering used by our startup drain
/// anchor and lets the delayed reply enter crossterm as ordinary input. Prefer
/// the dark Auto fallback on this terminal; explicit Light/Dark themes remain
/// unaffected.
pub(crate) fn should_query_background(term_program: Option<&str>) -> bool {
    !term_program.is_some_and(|program| program.eq_ignore_ascii_case("orca"))
}

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
    // OSC 11 query (request background colour) IMMEDIATELY followed by a
    // Primary Device Attributes query (DA1, `ESC [ c`). The DA1 reply is
    // our drain anchor:
    //
    //   * Terminals answer queries strictly in order, so the OSC 11 reply
    //     (if the terminal supports it) is ALWAYS already buffered by the
    //     time the DA1 reply arrives.
    //   * DA1 is supported by virtually every terminal (xterm, VTE,
    //     Konsole, kitty, WezTerm, Alacritty, Windows Terminal, tmux/
    //     screen passthrough, …) and round-trips in ~1 RTT.
    //
    // So instead of betting a fixed timeout against the reply latency —
    // which a high-latency SSH session (or tmux passthrough) reliably
    // beats, leaving `]11;rgb:ffff/ffff/ffff\` to leak into the input
    // box once the crossterm reader thread starts — we drain until the
    // DA1 terminator (`c`) and KNOW the OSC 11 reply is fully consumed.
    // The wait is exactly one round-trip on a responsive terminal; only
    // a terminal that answers NEITHER query pays the absolute cap below.
    stdout.write_all(b"\x1b]11;?\x07\x1b[c").ok()?;
    stdout.flush().ok()?;

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();

    let start = std::time::Instant::now();
    // First-byte budget: generous enough for an SSH round-trip. We only
    // pay this in full when the terminal answers neither query (rare for
    // an interactive TTY); a responsive terminal returns the instant the
    // DA1 terminator lands, regardless of how large this is.
    let first_byte_deadline = start + timeout.max(Duration::from_millis(400));
    // Absolute safety cap so a malformed / never-terminating reply (or a
    // terminal that streams without ever sending `c`) can't hang startup.
    let hard_deadline = start + Duration::from_millis(800);

    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut got_any = false;
    loop {
        let deadline = if got_any {
            hard_deadline
        } else {
            first_byte_deadline
        };
        let mut chunk = [0u8; 128];
        // SAFETY: chunk is stack-allocated and lives for the call; fd is
        // owned by stdin for the process lifetime.
        let nread = unsafe { poll_read(fd, deadline, &mut chunk) };
        if nread == 0 {
            break; // timeout / EOF — terminal answered neither query
        }
        got_any = true;
        buf.extend_from_slice(&chunk[..nread]);
        // DA1 terminator seen ⇒ any preceding OSC 11 reply is fully
        // buffered ⇒ nothing left to leak.
        if has_da1_terminator(&buf) {
            break;
        }
        if std::time::Instant::now() >= hard_deadline {
            break;
        }
    }

    parse_osc11_response(&buf)
}

/// True when `buf` contains a complete Primary Device Attributes (DA1)
/// reply: `ESC [ … c`, where the parameter bytes between `[` and the
/// final `c` are only digits, `;`, or `?` (the DA1 reply shape, e.g.
/// `ESC [ ? 6 2 ; c`). Used as the drain anchor in [`detect_light_unix`]:
/// because terminals answer queries in order, seeing the DA1 terminator
/// guarantees any preceding OSC 11 reply is already buffered.
///
/// Scanning for a *well-formed* DA1 reply (rather than any byte `c`)
/// avoids false-positives on a stray keystroke like a cursor key
/// (`ESC [ A`) that lands in stdin before the replies.
#[cfg(any(unix, test))]
fn has_da1_terminator(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' {
            for &b in &buf[i + 2..] {
                if b == b'c' {
                    return true;
                }
                // DA1 params are digits / ';' / '?'. Anything else means
                // this CSI isn't a DA1 reply — stop scanning this one.
                if !(b.is_ascii_digit() || b == b';' || b == b'?') {
                    break;
                }
            }
        }
        i += 1;
    }
    false
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
    fn orca_skips_background_query_case_insensitively() {
        assert!(!should_query_background(Some("Orca")));
        assert!(!should_query_background(Some("orca")));
        assert!(should_query_background(Some("iTerm.app")));
        assert!(should_query_background(None));
    }

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

    #[test]
    fn da1_terminator_detected() {
        // Typical DA1 replies.
        assert!(has_da1_terminator(b"\x1b[?62;c"));
        assert!(has_da1_terminator(b"\x1b[?1;2c"));
        assert!(has_da1_terminator(b"\x1b[?64;1;2;6;9;15;22c"));
    }

    #[test]
    fn da1_terminator_after_osc11_reply() {
        // The real on-the-wire shape: OSC 11 reply (ST-terminated) then
        // the DA1 reply. The anchor must fire, and the OSC 11 reply must
        // still parse out of the combined buffer.
        let combined = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;c";
        assert!(has_da1_terminator(combined));
        assert_eq!(parse_osc11_response(combined), Some(true));
    }

    #[test]
    fn da1_terminator_not_falsely_triggered() {
        // Incomplete / unrelated input must NOT look like a DA1 reply.
        assert!(!has_da1_terminator(b""));
        assert!(!has_da1_terminator(b"\x1b[?62;")); // no final `c` yet
        assert!(!has_da1_terminator(b"\x1b[A")); // cursor-up keystroke
        assert!(!has_da1_terminator(b"abc")); // bare `c`, no CSI opener
        assert!(!has_da1_terminator(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\")); // OSC only
    }
}
