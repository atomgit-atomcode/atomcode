// crates/atomcode-tuix/src/highlight/theme.rs
//
// Theme-aware SGR sequences for markdown inline elements (headings,
// inline code, bold/italic, muted chrome). Two variants — `dark` and
// `light` — selected at startup from `Config::ui.theme` via
// `set_theme_mode()`. Constants that don't change between themes
// (RESET, bold/italic attribute toggles, muted SGR 90) stay as plain
// `pub const &str`.
//
// History: this module used to also expose per-token colour accessors
// for the syntect-driven code-block highlighter (`keyword()`, `string()`,
// `function()`, etc.). Those were removed when per-token colouring in
// code blocks was dropped — macOS Terminal.app's semi-transparent grey
// selection overlay made truecolor tokens unreadable inside selections.
// See `highlight::highlight_block`'s doc for the full rationale. Git
// history retains the removed accessors and their palettes.

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
    MODE.store(
        if light { MODE_LIGHT } else { MODE_DARK },
        Ordering::Relaxed,
    );
}

#[cfg(test)]
pub fn test_lock() -> ThemeTestGuard {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    ThemeTestGuard {
        original: MODE.load(Ordering::Relaxed),
        _guard: guard,
    }
}

#[cfg(test)]
pub struct ThemeTestGuard {
    original: u8,
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for ThemeTestGuard {
    fn drop(&mut self) {
        MODE.store(self.original, Ordering::Relaxed);
    }
}

#[inline]
fn is_light() -> bool {
    MODE.load(Ordering::Relaxed) == MODE_LIGHT
}

/// `render/theme.rs` reads this when picking the session-name pill SGR
/// (and any other render-side choice that depends on the active theme).
#[inline]
pub fn is_light_for_render() -> bool {
    is_light()
}

/// Full SGR clear. Use after every wrapped token span.
///
/// Historically this was `"\x1b[23;39m"` (italic off + default fg only)
/// to minimise emitted bytes. That under-cleared: any upstream UI chrome
/// that left `reverse` (SGR 7), `bold` (SGR 1), or `faint` (SGR 2) on
/// the working style — e.g. the ApprovalPrompt Y chip or the top-rule
/// session pill — leaked across token boundaries inside
/// `parse_markdown_to_cells`, baking those bits onto syntect-coloured
/// cells. On Terminal.app this rendered as solid coloured blocks at
/// Number/String token positions (fg-as-bg with default-fg glyph); iTerm2
/// happened to mask the bug with more lenient SGR state handling.
///
/// `"\x1b[0m"` is the full ECMA-48 reset (4 bytes, no perceptible perf
/// impact) and is the only safe close when we don't know what attributes
/// the surrounding stream left enabled.
pub const RESET: &str = "\x1b[0m";

// ── Markdown inline element colours ──────────────────────────────────

/// Heading H1-H3.
/// `dark`: bold + bright cyan (SGR 1;96, matches `Palette::ACCENT`).
/// `light`: bold + bright blue (SGR 1;94) — bright cyan renders too pale
/// on white in most light-theme terminal profiles; blue still maps to a
/// dark, readable variant on light profiles.
pub fn md_heading_open() -> &'static str {
    if is_light() {
        "\x1b[1;34m"
    } else {
        "\x1b[1;96m"
    }
}

/// Close heading: bold off + fg default (SGR 22;39). Theme-invariant.
pub const MD_HEADING_CLOSE: &str = "\x1b[22;39m";

/// Inline code — NOT bold. Bold + bright cyan made every `` `code` `` span in
/// dense prose flare (and rendered harshly on terminals that don't soften
/// bright cyan); weight is reserved for `**bold**`. Color alone marks it as
/// code, calmly and consistently across terminals.
/// `dark`: bright cyan (matches headings, minus the bold).
/// `light`: standard magenta (SGR 35) — distinct from headings, readable on white.
pub fn md_inline_code_open() -> &'static str {
    if is_light() {
        "\x1b[35m"
    } else {
        "\x1b[96m"
    }
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

/// Muted / structural chrome (list markers): bright black / dark grey
/// (SGR 90). Adequate as a small accent glyph next to bright text on
/// either background. NOTE: NOT adequate for large structures like table
/// grids on dark themes — those use the theme-aware [`md_border_open`].
/// List markers use the theme-aware [`md_marker_open`] for the same reason.
pub const MD_MUTED_OPEN: &str = "\x1b[90m";
pub const MD_MUTED_CLOSE: &str = "\x1b[39m";

/// List-marker (• / `1.`) colour — theme-aware, unlike the fixed
/// [`MD_MUTED_OPEN`]. Light themes keep SGR 90 (readable mid-gray on
/// white, ~4.5:1); dark themes switch to SGR 37 (soft light-gray) because
/// a fixed SGR 90 collapses to ~`#3F3F3F` (~3:1) against dark backgrounds —
/// the same "invisible until selected" contrast bug table borders had
/// (see [`md_border_open`]), and the reported cause of ordered-list
/// numbers being unreadable until the text was selected (Issue #1426).
pub fn md_marker_open() -> &'static str {
    if is_light() {
        "\x1b[90m"
    } else {
        "\x1b[37m"
    }
}

/// Table-border / structural-rule colour — theme-aware, unlike the fixed
/// [`MD_MUTED_OPEN`]. Light themes: SGR 90 (bright-black → mid-gray,
/// ~4.5:1 on white). Dark themes: SGR 90 maps to ~`#3F3F3F` (~3:1 — the
/// whole grid is swallowed by the background, the "table lines invisible
/// until selected" bug), so use SGR 37 (regular white → soft light-gray),
/// the shade `Palette::MUTED_DARK` uses for text. `parse_markdown_to_cells`
/// maps SGR 37 → `Color::Grey`; close with [`MD_MUTED_CLOSE`] (SGR 39,
/// reset fg) on both themes.
pub fn md_border_open() -> &'static str {
    if is_light() {
        "\x1b[90m"
    } else {
        "\x1b[37m"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_dark<F: FnOnce()>(f: F) {
        let _g = test_lock();
        set_theme_mode(false);
        f();
    }

    fn with_light<F: FnOnce()>(f: F) {
        let _g = test_lock();
        set_theme_mode(true);
        f();
    }

    #[test]
    fn reset_is_full_sgr_clear() {
        // Must be a full SGR reset (`\x1b[0m`), not the historical
        // partial close (`\x1b[23;39m`) which only cleared italic + fg
        // and let reverse / bold / faint leak across token boundaries
        // inside `parse_markdown_to_cells`. See the doc on `RESET` for
        // the full bug class this guards against.
        assert_eq!(RESET, "\x1b[0m");
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
    fn dark_md_inline_code_is_bright_cyan_not_bold() {
        with_dark(|| assert_eq!(md_inline_code_open(), "\x1b[96m"));
    }

    #[test]
    fn dark_md_border_is_visible_gray_not_swallowed_darkgrey() {
        // Regression: on dark themes a fixed SGR 90 (DarkGrey) collapsed to
        // ~3:1 against the bg and the whole table grid went invisible until
        // selected. Dark mode must emit SGR 37 (soft light-gray), which the
        // cell parser maps to Color::Grey.
        with_dark(|| assert_eq!(md_border_open(), "\x1b[37m"));
    }

    #[test]
    fn light_md_border_is_darkgrey() {
        // On light themes SGR 90 is a readable mid-gray (~4.5:1 on white);
        // SGR 37 would be near-invisible there, so light keeps SGR 90.
        with_light(|| assert_eq!(md_border_open(), "\x1b[90m"));
    }

    #[test]
    fn dark_md_marker_is_visible_gray_not_swallowed_darkgrey() {
        // Regression (Issue #1426): on dark themes a fixed SGR 90 (DarkGrey)
        // collapsed to ~3:1 against the bg and ordered-list numbers went
        // invisible until selected. Dark mode must emit SGR 37 (soft
        // light-gray), mirroring md_border_open's fix for table borders.
        with_dark(|| assert_eq!(md_marker_open(), "\x1b[37m"));
    }

    #[test]
    fn light_md_marker_is_darkgrey() {
        // On light themes SGR 90 is a readable mid-gray (~4.5:1 on white);
        // keep it, same as md_border_open.
        with_light(|| assert_eq!(md_marker_open(), "\x1b[90m"));
    }

    #[test]
    fn light_md_inline_code_is_magenta_not_bold() {
        with_light(|| assert_eq!(md_inline_code_open(), "\x1b[35m"));
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
