//! ASCII fallback for decorative Unicode glyphs on non-unicode terminals.
//!
//! Many glyphs are hardcoded into rendered text — most pervasively the `✓`/`✗`/`⚠`
//! status marks baked into `atomcode-core`'s i18n strings, which live BELOW the terminal
//! layer and so can't consult `TerminalCaps`. On a terminal flagged `!unicode_symbols`
//! (legacy Windows conhost, no-unicode fonts) those glyphs render as `□` tofu — even
//! though atomcode's *own* symbols (chevron, spinner, continuation bar) already fall back
//! to ASCII via `TerminalCaps`.
//!
//! [`downgrade_glyphs`] closes that gap centrally: the renderer runs body/status text
//! through it when `!unicode_symbols`, transliterating a curated set of decorative glyphs
//! to ASCII. It is a monotonic improvement — a glyph that would tofu is replaced by a
//! readable ASCII stand-in; text with no mapped glyph is returned borrowed (zero-copy).
//! CJK and ordinary punctuation are deliberately NOT touched (they render fine, and are
//! real content, not chrome).

use std::borrow::Cow;

/// Map a single decorative glyph to its ASCII stand-in, or `None` to leave it as-is.
/// One display column in → one column out wherever possible, so alignment survives.
fn ascii_for(ch: char) -> Option<&'static str> {
    Some(match ch {
        // ── status marks (the i18n `✓`/`✗`/`⚠` family — the reported tofu) ──
        '\u{2713}' | '\u{2705}' | '\u{2714}' => "v", // ✓ ✅ ✔
        '\u{2717}' | '\u{2718}' | '\u{274C}' | '\u{2716}' => "x", // ✗ ✘ ❌ ✖
        '\u{26A0}' => "!",                           // ⚠
        '\u{2139}' | '\u{24D8}' => "i",              // ℹ ⓘ
        '\u{1F4A1}' => "*",                          // 💡
        // ── bullets / circles / diamonds (tool headers, list markers) ──
        '\u{25CF}' | '\u{25C6}' | '\u{25CE}' | '\u{23FA}' | '\u{2B24}' => "*", // ● ◆ ◎ ⏺ ⬤
        '\u{25CB}' | '\u{25E6}' | '\u{25C7}' | '\u{25A2}' => "o",              // ○ ◦ ◇ ▢
        '\u{2022}' | '\u{2219}' => "*", // • ∙  (·  U+00B7 is Latin-1, renders fine — left as-is)
        // ── triangles / play / pointers ──
        '\u{25B8}' | '\u{25B6}' | '\u{25BA}' | '\u{276F}' => ">", // ▸ ▶ ► ❯
        '\u{25C2}' | '\u{25C0}' => "<",                           // ◂ ◀
        // ── arrows ──
        '\u{2192}' | '\u{21D2}' | '\u{21A6}' | '\u{21B3}' | '\u{2794}' | '\u{27A4}' => ">", // → ⇒ ↦ ↳ ➔ ➤
        '\u{2190}' | '\u{21D0}' | '\u{21A9}' | '\u{21B5}' | '\u{23CE}' => "<", // ← ⇐ ↩ ↵ ⏎
        '\u{2191}' | '\u{21D1}' => "^",                                        // ↑ ⇑
        '\u{2193}' | '\u{21D3}' => "v",                                        // ↓ ⇓
        '\u{2194}' | '\u{21D4}' | '\u{21BB}' | '\u{21BA}' | '\u{1F504}' => "~", // ↔ ⇔ ↻ ↺ 🔄
        // ── media/state controls ──
        '\u{23F8}' => "=",              // ⏸
        '\u{23F9}' => "#",              // ⏹
        '\u{23F3}' | '\u{231B}' => "~", // ⏳ ⌛
        // ── box drawing (rules, trees, tables) → classic ASCII box ──
        '\u{2500}' | '\u{2550}' | '\u{2501}' => "-", // ─ ═ ━
        '\u{2502}' | '\u{2551}' | '\u{2503}' | '\u{258E}' => "|", // │ ║ ┃ ▎
        '\u{23BD}' | '\u{23BC}' => "_",              // ⎽ ⎼ horizontal scan lines
        '\u{23BF}' | '\u{2514}' | '\u{2570}' => "`", // ⎿ └ ╰
        '\u{250C}' | '\u{2510}' | '\u{2518}' | '\u{251C}' | '\u{2524}' | '\u{252C}'
        | '\u{2534}' | '\u{253C}' | '\u{256D}' | '\u{256E}' | '\u{256F}' | '\u{2554}'
        | '\u{2557}' | '\u{255A}' | '\u{255D}' | '\u{2560}' | '\u{2563}' | '\u{2566}'
        | '\u{2569}' | '\u{256C}' => "+", // ┌┐┘├┤┬┴┼╭╮╯ + double variants
        // ── blocks / shades (progress bars, banners) ──
        '\u{2588}' | '\u{2580}' | '\u{2584}' | '\u{2592}' | '\u{2593}' => "#", // █ ▀ ▄ ▒ ▓
        '\u{2591}' => ".",                                                     // ░
        // ── weather / status emoji that leak into output ──
        '\u{1F7E2}' | '\u{1F7E1}' | '\u{1F534}' | '\u{1F535}' | '\u{1F7E0}' => "*", // 🟢🟡🔴🔵🟠
        _ => return None,
    })
}

/// Cell-level variant: the ASCII stand-in as a single `char`, for downgrading a
/// rendered cell in place. All map entries are one ASCII column, so this always
/// succeeds where [`ascii_for`] does — the `Option` is defensive against a future
/// multi-char entry (which would not fit one cell and is skipped).
pub fn single_char_ascii(ch: char) -> Option<char> {
    let s = ascii_for(ch)?;
    let mut it = s.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// Replace decorative glyphs with ASCII when the terminal lacks unicode support.
/// Returns `Borrowed` (zero-copy) when `unicode_symbols` is true or nothing maps.
pub fn downgrade_glyphs(text: &str, unicode_symbols: bool) -> Cow<'_, str> {
    if unicode_symbols || text.is_ascii() {
        return Cow::Borrowed(text);
    }
    if !text.chars().any(|c| ascii_for(c).is_some()) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ascii_for(ch) {
            Some(rep) => out.push_str(rep),
            None => out.push(ch),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_terminal_unchanged() {
        assert!(matches!(
            downgrade_glyphs("✓ Baked · ─────", true),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn ascii_input_is_zero_copy() {
        assert!(matches!(
            downgrade_glyphs("v Stopped - 0 rounds", false),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn no_mapped_glyph_is_zero_copy() {
        // CJK is content, never downgraded — and triggers no allocation.
        assert!(matches!(
            downgrade_glyphs("写一个网页", false),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn status_marks_downgrade() {
        assert_eq!(downgrade_glyphs("✗ Stopped", false), "x Stopped");
        assert_eq!(downgrade_glyphs("✓ Baked", false), "v Baked");
        assert_eq!(downgrade_glyphs("⚠ heads up", false), "! heads up");
    }

    #[test]
    fn tool_and_box_glyphs_downgrade() {
        assert_eq!(downgrade_glyphs("● Bash", false), "* Bash");
        assert_eq!(downgrade_glyphs("└ → done", false), "` > done");
        assert_eq!(
            downgrade_glyphs("──── label ────", false),
            "---- label ----"
        );
    }

    #[test]
    fn cjk_and_ascii_preserved_around_glyphs() {
        assert_eq!(downgrade_glyphs("✓ 写网页 done", false), "v 写网页 done");
    }

    #[test]
    fn single_char_ascii_maps_1col_glyphs() {
        assert_eq!(single_char_ascii('\u{2717}'), Some('x')); // ✗
        assert_eq!(single_char_ascii('\u{2713}'), Some('v')); // ✓
        assert_eq!(single_char_ascii('\u{25CF}'), Some('*')); // ●
        assert_eq!(single_char_ascii('\u{2500}'), Some('-')); // ─
        assert_eq!(single_char_ascii('\u{2514}'), Some('`')); // └
        assert_eq!(single_char_ascii('A'), None); // ASCII passes through
        assert_eq!(single_char_ascii('写'), None); // CJK untouched
    }
}
