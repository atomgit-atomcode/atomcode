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

    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // poll() ms argument is i32; clamp the duration to its range.
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: pollfd is a properly-initialised single-element array;
    // libc::poll mutates its `revents` field in-place. fd is valid for
    // the duration of the call (stdin owns it for the process lifetime).
    let n = unsafe { libc::poll(&mut pollfd, 1, ms) };
    if n <= 0 {
        return None;
    }

    let mut buf = [0u8; 128];
    // SAFETY: buf is a stack-allocated array; len matches its size.
    let nread = unsafe {
        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if nread <= 0 {
        return None;
    }
    parse_osc11_response(&buf[..nread as usize])
}

/// Parse an OSC 11 reply of shape `ESC ] 11 ; rgb:RRRR/GGGG/BBBB BEL`
/// (or `ESC \\` ST terminator). Returns `Some(is_light)` when a usable
/// RGB triplet is found.
///
/// Tolerates leading garbage (pre-existing keystrokes in stdin) by
/// scanning for `rgb:`. Tolerates trailing garbage (BEL / ST / partial
/// next response) by stopping at the first non-hex char.
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
