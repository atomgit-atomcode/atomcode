// crates/atomcode-tuix/src/highlight/theme.rs
//
// Mid-lightness palette for code-block syntax highlight. Each token color
// is a truecolor SGR open-sequence; pair with `RESET` to close. Colors
// chosen for WCAG AA 4.5:1 contrast on both #FFFFFF and #1E1E1E
// backgrounds — see docs/superpowers/specs/2026-05-18-code-block-...
//
// VARIABLE and PUNCTUATION are deliberately empty: they're the majority
// of source-code characters, and painting them would make the screen
// "flicker." Caller's mapping logic must skip the SGR wrap when the
// color string is empty (otherwise an unmatched RESET would clobber any
// previously open SGR).
//
// ── Markdown inline colours ──────────────────────────────────────────
//
// Markdown-level colours live here alongside the syntect palette so
// future theme work (e.g. a dark/light/monochrome switch) only needs
// to swap one file.  All markdown colours use the 16-colour SGR range
// (30-37 / 90-97) so the terminal's own theme engine interprets them
// — same strategy as `render::theme::Palette`.

// ── Code-block (syntect) token colours ───────────────────────────────

pub const KEYWORD: &str     = "\x1b[38;2;198;120;221m"; // soft purple
pub const STRING: &str      = "\x1b[38;2;152;195;121m"; // green
pub const NUMBER: &str      = "\x1b[38;2;209;154;102m"; // amber
pub const COMMENT: &str     = "\x1b[3;38;2;124;132;153m"; // italic + slate gray
pub const FUNCTION: &str    = "\x1b[38;2;97;175;239m"; // blue
pub const TYPE: &str        = "\x1b[38;2;229;192;123m"; // sand
pub const VARIABLE: &str    = ""; // terminal default fg
pub const PUNCTUATION: &str = ""; // terminal default fg

/// Closes color + italic. Use after every wrapped token span.
/// SGR 23 = italic off, SGR 39 = default foreground.
pub const RESET: &str = "\x1b[23;39m";

// ── Markdown inline element colours ──────────────────────────────────
//
// These use 16-colour SGR codes (not truecolor) so the terminal theme
// remaps them, keeping contrast on both light and dark backgrounds.
// Pair each "open" with its matching "close" to avoid SGR state leaks.

/// Heading H1-H3: bold + bright cyan (SGR 1;96).
/// Matches `Palette::ACCENT` / `Palette::CODE` (SGR 96).
pub const MD_HEADING_OPEN: &str  = "\x1b[1;96m";
/// Close heading: bold off + fg default (SGR 22;39).
pub const MD_HEADING_CLOSE: &str = "\x1b[22;39m";

/// Inline code: bold + bright cyan (SGR 1;96).
/// Same hue as headings — inline code and headings are both "emphasised
/// text" that should stand out from prose.  Using the same colour keeps
/// the visual palette tight (two accent colours: cyan + muted gray).
pub const MD_INLINE_CODE_OPEN: &str  = "\x1b[1;96m";
/// Close inline code: bold off + fg default (SGR 22;39).
pub const MD_INLINE_CODE_CLOSE: &str = "\x1b[22;39m";

/// Bold text: SGR 1 (bold on).
pub const MD_BOLD_OPEN: &str  = "\x1b[1m";
/// Close bold: SGR 22 (bold off).
pub const MD_BOLD_CLOSE: &str = "\x1b[22m";

/// Italic text: SGR 3 (italic on).
pub const MD_ITALIC_OPEN: &str  = "\x1b[3m";
/// Close italic: SGR 23 (italic off).
pub const MD_ITALIC_CLOSE: &str = "\x1b[23m";

/// Muted / structural chrome (list markers, table borders): bright
/// black / dark grey (SGR 90).  Matches `Palette::MUTED`.
pub const MD_MUTED_OPEN: &str  = "\x1b[90m";
/// Close muted: fg default (SGR 39).
pub const MD_MUTED_CLOSE: &str = "\x1b[39m";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_is_truecolor_sgr() {
        assert_eq!(KEYWORD, "\x1b[38;2;198;120;221m");
    }

    #[test]
    fn string_is_truecolor_sgr() {
        assert_eq!(STRING, "\x1b[38;2;152;195;121m");
    }

    #[test]
    fn comment_includes_italic_attr() {
        assert!(
            COMMENT.starts_with("\x1b[3;38;2;"),
            "comment must lead with SGR 3 (italic) then truecolor fg, got: {:?}",
            COMMENT
        );
    }

    #[test]
    fn variable_and_punctuation_are_empty() {
        assert_eq!(VARIABLE, "");
        assert_eq!(PUNCTUATION, "");
    }

    #[test]
    fn reset_closes_italic_and_fg() {
        assert_eq!(RESET, "\x1b[23;39m");
    }

    // ── Markdown colour constants ──────────────────────────────────

    #[test]
    fn md_heading_uses_bold_bright_cyan() {
        assert_eq!(MD_HEADING_OPEN, "\x1b[1;96m");
        assert_eq!(MD_HEADING_CLOSE, "\x1b[22;39m");
    }

    #[test]
    fn md_inline_code_uses_bold_bright_cyan() {
        assert_eq!(MD_INLINE_CODE_OPEN, "\x1b[1;96m");
        assert_eq!(MD_INLINE_CODE_CLOSE, "\x1b[22;39m");
    }

    #[test]
    fn md_bold_uses_sgr_1() {
        assert_eq!(MD_BOLD_OPEN, "\x1b[1m");
        assert_eq!(MD_BOLD_CLOSE, "\x1b[22m");
    }

    #[test]
    fn md_italic_uses_sgr_3() {
        assert_eq!(MD_ITALIC_OPEN, "\x1b[3m");
        assert_eq!(MD_ITALIC_CLOSE, "\x1b[23m");
    }

    #[test]
    fn md_muted_uses_sgr_90() {
        assert_eq!(MD_MUTED_OPEN, "\x1b[90m");
        assert_eq!(MD_MUTED_CLOSE, "\x1b[39m");
    }
}
