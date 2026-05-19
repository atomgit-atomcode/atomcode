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
}
