// crates/atomcode-tuix/src/highlight/theme.rs
//
// Theme-aware colour palette for markdown rendering + syntect code-block
// highlight. Two variants:
//
// - `dark`  (default): legacy palette tuned for dark terminal backgrounds
//                      (≈ #1E1E1E). Light, washed-out tones.
// - `light`           : darker, more saturated tones hitting ≥ 4.5:1 WCAG
//                      AA contrast against `#FFFFFF`.
//
// The active variant is selected at startup from `Config::ui.theme` via
// `set_theme_mode()`; readers go through the small accessor fns below.
// Constants that don't change between themes (RESET, bold/italic SGR
// attribute toggles, muted SGR 90) stay as plain `pub const &str`.
//
// VARIABLE and PUNCTUATION are deliberately empty: they're the majority
// of source-code characters, and painting them would make the screen
// "flicker." Caller's mapping logic must skip the SGR wrap when the
// colour string is empty (otherwise an unmatched RESET would clobber
// any previously open SGR).

use std::sync::atomic::{AtomicU8, Ordering};

const MODE_DARK: u8 = 0;
const MODE_LIGHT: u8 = 1;

/// Runtime theme selector. Updated once at startup by the TUIX entry
/// point after reading `Config::ui.theme`; readers see eventual
/// consistency via `Relaxed` ordering.
static MODE: AtomicU8 = AtomicU8::new(MODE_DARK);

/// Switch the palette. Idempotent. Call once during startup before the
/// first markdown / highlight emission.
pub fn set_theme_mode(light: bool) {
    MODE.store(if light { MODE_LIGHT } else { MODE_DARK }, Ordering::Relaxed);
}

#[inline]
fn is_light() -> bool {
    MODE.load(Ordering::Relaxed) == MODE_LIGHT
}

/// Sibling-module accessor: `highlight/mod.rs` reads this to pick the
/// right cached syntect Theme. Exposed as a small named fn rather than
/// making `is_light` pub so the rest of the crate can't accidentally
/// gate behaviour on the mode bit (the right entry point is the
/// per-token accessors).
#[inline]
pub(super) fn is_light_for_highlight() -> bool {
    is_light()
}

/// `render/alt_screen.rs` reads this to swap the session-name pill SGR
/// (reverse + bright cyan on dark; bold + standard magenta on light).
/// Named so the call site documents intent; behaviourally identical to
/// `is_light_for_highlight`.
#[inline]
pub fn is_light_for_render() -> bool {
    is_light()
}

// ── Code-block (syntect) token colours ───────────────────────────────
//
// Truecolor SGRs are written-out RGB values that the terminal cannot
// remap; both palettes must independently hit the contrast bar against
// their target background.

/// `dark`: soft purple `#C678DD`. `light`: deep purple `#7B1FA2`.
pub fn keyword() -> &'static str {
    if is_light() { "\x1b[38;2;123;31;162m" } else { "\x1b[38;2;198;120;221m" }
}

/// `dark`: green `#98C379`. `light`: forest green `#1B5E20`.
pub fn string() -> &'static str {
    if is_light() { "\x1b[38;2;27;94;32m" } else { "\x1b[38;2;152;195;121m" }
}

/// `dark`: amber `#D19A66`. `light`: burnt sienna `#A04B00`.
pub fn number() -> &'static str {
    if is_light() { "\x1b[38;2;160;75;0m" } else { "\x1b[38;2;209;154;102m" }
}

/// `dark`: slate gray `#7C8499` + italic. `light`: slate `#4A5060` + italic.
pub fn comment() -> &'static str {
    if is_light() { "\x1b[3;38;2;74;80;96m" } else { "\x1b[3;38;2;124;132;153m" }
}

/// `dark`: blue `#61AFEF`. `light`: deep navy `#0D47A1` (was the most
/// visible failure on `#FFFFFF` — old #61AFEF contrast 2.04:1).
pub fn function() -> &'static str {
    if is_light() { "\x1b[38;2;13;71;161m" } else { "\x1b[38;2;97;175;239m" }
}

/// `dark`: sand `#E5C07B`. `light`: deep ochre `#825A00`.
pub fn type_color() -> &'static str {
    if is_light() { "\x1b[38;2;130;90;0m" } else { "\x1b[38;2;229;192;123m" }
}

/// Both palettes intentionally use terminal default fg.
pub fn variable() -> &'static str { "" }

/// Both palettes intentionally use terminal default fg.
pub fn punctuation() -> &'static str { "" }

/// Closes color + italic. Use after every wrapped token span.
/// SGR 23 = italic off, SGR 39 = default foreground.
pub const RESET: &str = "\x1b[23;39m";

// ── Markdown inline element colours ──────────────────────────────────

/// Heading H1-H3.
/// `dark`: bold + bright cyan (SGR 1;96, matches `Palette::ACCENT`).
/// `light`: bold + bright blue (SGR 1;94) — bright cyan renders too pale
/// on white in most light-theme terminal profiles; blue still maps to a
/// dark, readable variant on light profiles.
pub fn md_heading_open() -> &'static str {
    if is_light() { "\x1b[1;34m" } else { "\x1b[1;96m" }
}

/// Close heading: bold off + fg default (SGR 22;39). Theme-invariant.
pub const MD_HEADING_CLOSE: &str = "\x1b[22;39m";

/// Inline code.
/// `dark`: bold + bright cyan (matches headings).
/// `light`: bold + standard magenta (SGR 1;35) — distinct from headings,
/// terminal profiles map 35 to a dark magenta that's readable on white.
pub fn md_inline_code_open() -> &'static str {
    if is_light() { "\x1b[1;35m" } else { "\x1b[1;96m" }
}

/// Close inline code: bold off + fg default. Theme-invariant.
pub const MD_INLINE_CODE_CLOSE: &str = "\x1b[22;39m";

/// Bold text: SGR 1 (bold on). Theme-invariant — bold is an attribute,
/// not a colour.
pub const MD_BOLD_OPEN: &str = "\x1b[1m";
pub const MD_BOLD_CLOSE: &str = "\x1b[22m";

/// Italic text: SGR 3 (italic on). Theme-invariant.
pub const MD_ITALIC_OPEN: &str = "\x1b[3m";
pub const MD_ITALIC_CLOSE: &str = "\x1b[23m";

/// Muted / structural chrome (list markers, table borders): bright
/// black / dark grey (SGR 90). The terminal's profile maps this to a
/// shade with adequate contrast on either background — keep as constant.
pub const MD_MUTED_OPEN: &str = "\x1b[90m";
pub const MD_MUTED_CLOSE: &str = "\x1b[39m";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Guard around theme-switching tests so they don't race each other
    // (the static `MODE` is per-process). Each test takes the lock,
    // switches, asserts, switches back.
    static THEME_LOCK: Mutex<()> = Mutex::new(());

    fn with_dark<F: FnOnce()>(f: F) {
        let _g = THEME_LOCK.lock().unwrap();
        set_theme_mode(false);
        f();
        set_theme_mode(false); // restore default
    }

    fn with_light<F: FnOnce()>(f: F) {
        let _g = THEME_LOCK.lock().unwrap();
        set_theme_mode(true);
        f();
        set_theme_mode(false); // restore default
    }

    #[test]
    fn dark_keyword_is_legacy_soft_purple() {
        with_dark(|| assert_eq!(keyword(), "\x1b[38;2;198;120;221m"));
    }

    #[test]
    fn light_keyword_is_deep_purple() {
        with_light(|| assert_eq!(keyword(), "\x1b[38;2;123;31;162m"));
    }

    #[test]
    fn dark_function_is_legacy_blue() {
        with_dark(|| assert_eq!(function(), "\x1b[38;2;97;175;239m"));
    }

    #[test]
    fn light_function_is_deep_navy() {
        // The exact failure mode from the user screenshot: legacy
        // `#61AFEF` had 2.04:1 contrast on white and `main` vanished.
        // `#0D47A1` hits ≥ 7:1.
        with_light(|| assert_eq!(function(), "\x1b[38;2;13;71;161m"));
    }

    #[test]
    fn variable_and_punctuation_stay_empty_in_both_palettes() {
        with_dark(|| {
            assert_eq!(variable(), "");
            assert_eq!(punctuation(), "");
        });
        with_light(|| {
            assert_eq!(variable(), "");
            assert_eq!(punctuation(), "");
        });
    }

    #[test]
    fn comment_includes_italic_attr_in_both_palettes() {
        with_dark(|| assert!(comment().starts_with("\x1b[3;38;2;"),
                             "dark comment must lead with SGR 3 + truecolor"));
        with_light(|| assert!(comment().starts_with("\x1b[3;38;2;"),
                              "light comment must lead with SGR 3 + truecolor"));
    }

    #[test]
    fn reset_closes_italic_and_fg() {
        assert_eq!(RESET, "\x1b[23;39m");
    }

    #[test]
    fn dark_md_heading_is_bold_bright_cyan() {
        with_dark(|| assert_eq!(md_heading_open(), "\x1b[1;96m"));
    }

    #[test]
    fn light_md_heading_is_bold_blue() {
        with_light(|| assert_eq!(md_heading_open(), "\x1b[1;34m"));
    }

    #[test]
    fn dark_md_inline_code_is_bold_bright_cyan() {
        with_dark(|| assert_eq!(md_inline_code_open(), "\x1b[1;96m"));
    }

    #[test]
    fn light_md_inline_code_is_bold_magenta() {
        with_light(|| assert_eq!(md_inline_code_open(), "\x1b[1;35m"));
    }

    #[test]
    fn close_codes_are_theme_invariant() {
        // Close codes only manipulate SGR attributes (bold off / italic
        // off / fg default), never set a colour — should be identical
        // regardless of theme.
        assert_eq!(MD_HEADING_CLOSE, "\x1b[22;39m");
        assert_eq!(MD_INLINE_CODE_CLOSE, "\x1b[22;39m");
        assert_eq!(MD_BOLD_CLOSE, "\x1b[22m");
        assert_eq!(MD_ITALIC_CLOSE, "\x1b[23m");
        assert_eq!(MD_MUTED_CLOSE, "\x1b[39m");
    }
}
