// crates/atomcode-tuix/src/sanitize.rs

/// Strip ANSI escape sequences and C0/C1 control codes except tab/newline/CR.
///
/// Defends against:
/// - CSI sequences (\x1b[...) — can clear screen, move cursor, query position
/// - OSC sequences (\x1b]...\x07 or \x1b]...\x1b\\) — can set terminal title,
///   manipulate clipboard, or write to hyperlink targets
/// - C0 controls (0x00..0x1F) except \t \n \r — can ring bell, backspace, etc.
/// - C1 controls (0x80..0x9F in UTF-8 as U+0080..U+009F) — alternate CSI forms
/// - Bare ESC (\x1b not followed by a recognised intro)
pub fn scrub_controls(input: &str) -> String {
    scrub_inner(input, false)
}

/// Same as [`scrub_controls`] but preserves CSI sequences whose final
/// byte is `m` — i.e. **SGR (Select Graphic Rendition)**: colour,
/// bold, italic, underline, strikethrough, faint, reverse-video, etc.
///
/// SGR is purely cosmetic — it changes how subsequent text is drawn
/// but never moves the cursor, queries terminal state, touches the
/// clipboard, sets the window title, or otherwise reaches outside
/// the display rectangle. Allowing it through is what `less`, `git`,
/// `bat`, and every other "safe ANSI" tool does, and it lets trusted
/// internal output (e.g. the `/codingplan` SetupReport's locked-model
/// rows that render in the terminal's theme red) survive sanitisation
/// without each caller having to roll its own emission path.
///
/// Use this on **trusted** output — strings the app itself builds
/// (slash-command return text, status lines, setup reports). Do NOT
/// use it on text that came from a remote LLM or any other untrusted
/// channel: SGR can still be used to hide content (faint, black-on-
/// black) or impersonate UI chrome (✓ in green next to a lie), so
/// LLM streams continue to go through the strict [`scrub_controls`].
pub fn scrub_controls_keep_sgr(input: &str) -> String {
    scrub_inner(input, true)
}

fn scrub_inner(input: &str, keep_sgr: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\t' | '\n' | '\r' => out.push(c),
            '\x00'..='\x1F' => {
                if c == '\x1B' {
                    // ESC — consume one of: CSI, OSC, SS2, SS3, or lone byte
                    match chars.peek() {
                        Some(&'[') => {
                            chars.next(); // consume [
                                          // CSI: params, intermediates, final byte 0x40..=0x7E
                            let mut buf = String::new();
                            buf.push('\x1b');
                            buf.push('[');
                            let mut final_byte: Option<char> = None;
                            while let Some(&p) = chars.peek() {
                                chars.next();
                                buf.push(p);
                                if ('\x40'..='\x7E').contains(&p) {
                                    final_byte = Some(p);
                                    break;
                                }
                            }
                            // SGR (CSI ... m) is pure presentation —
                            // when the caller asked to keep SGR, emit
                            // the buffered sequence verbatim. Other
                            // CSI finals (cursor moves, DSR queries,
                            // erase-in-display, etc.) stay dropped on
                            // both paths.
                            if keep_sgr && final_byte == Some('m') {
                                out.push_str(&buf);
                            }
                        }
                        Some(&']') => {
                            chars.next(); // consume ]
                                          // OSC: end on BEL (\x07) or ST (ESC \)
                            while let Some(&p) = chars.peek() {
                                chars.next();
                                if p == '\x07' {
                                    break;
                                }
                                if p == '\x1B' {
                                    if chars.peek() == Some(&'\\') {
                                        chars.next();
                                    }
                                    break;
                                }
                            }
                        }
                        Some(_) => {
                            // bare ESC: drop only the ESC; let next char pass through
                        }
                        None => {} // lone ESC at EOF, drop
                    }
                }
                // other C0: drop
            }
            '\u{0080}'..='\u{009F}' => {
                // C1 controls — some terminals interpret these as CSI alternatives.
                // For U+009B (alt CSI introducer), consume the full sequence up to
                // its final byte so the payload cannot reach the terminal as literal
                // text. Other C1 controls are dropped as-is.
                if c == '\u{009B}' {
                    while let Some(&p) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7E').contains(&p) {
                            break;
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_passes_through() {
        assert_eq!(scrub_controls("hello world"), "hello world");
    }

    #[test]
    fn newline_tab_cr_preserved() {
        assert_eq!(scrub_controls("a\nb\tc\rd"), "a\nb\tc\rd");
    }

    #[test]
    fn csi_escape_stripped() {
        // \x1b[2J = clear screen, \x1b[H = cursor home
        assert_eq!(scrub_controls("\x1b[2J\x1b[Hhello"), "hello");
    }

    #[test]
    fn osc_escape_stripped() {
        // \x1b]0;title\x07 = set terminal title
        assert_eq!(scrub_controls("\x1b]0;pwned\x07safe"), "safe");
    }

    #[test]
    fn cursor_position_query_stripped() {
        // \x1b[6n = query cursor position (leaks via stdin!)
        assert_eq!(scrub_controls("a\x1b[6nb"), "ab");
    }

    #[test]
    fn c0_controls_except_tnlcr_removed() {
        assert_eq!(scrub_controls("a\x00b\x01c\x07d\x08e"), "abcde");
    }

    #[test]
    fn c1_controls_removed() {
        // \x9b = CSI alternate form — introducer AND payload must be stripped
        assert_eq!(scrub_controls("a\u{009b}2Jb"), "ab");
    }

    #[test]
    fn utf8_text_preserved() {
        assert_eq!(scrub_controls("你好\nworld"), "你好\nworld");
    }

    #[test]
    fn bare_esc_removed() {
        assert_eq!(scrub_controls("a\x1bb"), "ab");
    }

    #[test]
    fn keep_sgr_lets_color_through() {
        // SGR 31 (red fg) + SGR 39 (default fg) survives.
        assert_eq!(
            scrub_controls_keep_sgr("\x1b[31mred\x1b[39m tail"),
            "\x1b[31mred\x1b[39m tail"
        );
    }

    #[test]
    fn keep_sgr_still_strips_cursor_csi() {
        // Cursor moves (\x1b[2J, \x1b[H, \x1b[6n) and any non-SGR CSI
        // are still rejected even on the SGR-allowing path.
        assert_eq!(scrub_controls_keep_sgr("\x1b[2J\x1b[Hhi\x1b[6n"), "hi");
    }

    #[test]
    fn keep_sgr_still_strips_osc() {
        // OSC payloads (clipboard injection, set title) stay rejected.
        assert_eq!(scrub_controls_keep_sgr("\x1b]0;pwned\x07safe"), "safe");
    }

    #[test]
    fn keep_sgr_preserves_multi_param_sgr() {
        // SGR can carry multiple parameters in one sequence —
        // e.g. `\x1b[1;31m` = bold + red. Verify both the
        // separator and the parameter chain pass through intact.
        assert_eq!(
            scrub_controls_keep_sgr("\x1b[1;31mbold-red\x1b[0m"),
            "\x1b[1;31mbold-red\x1b[0m"
        );
    }
}
