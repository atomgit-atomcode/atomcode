//! What this terminal can actually do — the first layer's half of the OS shield.
//!
//! There are two kinds of platform difference in a TUI and only one of them is
//! about drawing:
//!
//! * **I/O**: key encodings, resize signals, raw mode, bracketed paste, alt
//!   screen. Shielded by [`Surface`](crate::surface::Surface), which is already
//!   a row — a headless surface and a terminal surface are swappable.
//! * **Drawing**: which glyphs render, how many columns they take, how many
//!   colours there are. That is this file.
//!
//! Calling them one layer is how the first kind leaks: key handling never
//! passes through any box-drawing code, so a shield built only around glyphs
//! would silently not cover it.
//!
//! # Capabilities are injected, never detected in `render`
//!
//! Same rule as time (`docs/adr/0008`), for the same reason. A `render` that
//! read `TERM` would be green on the developer's machine and wrong on the
//! user's, and no test would say so. Detection happens once, in the surface
//! row; everything above receives a [`Caps`] value.
//!
//! # The downgrade happens at paint, not at composition
//!
//! Modules write `✓` and `┌` unconditionally. [`downgrade`] rewrites them on
//! the way to the terminal when the terminal cannot show them. Putting it here
//! rather than at every call site is what makes the layering enforceable rather
//! than aspirational: a module *cannot* get this wrong, because it never had a
//! choice to make.
//!
//! It also fixes a real width bug rather than just a legibility one. `✓`, `─`
//! and friends are East Asian **Ambiguous**: one column in most terminals, two
//! in a CJK-configured one. Our width arithmetic assumes one. The ASCII
//! stand-ins are unambiguously one column, so downgrading makes the assumption
//! true instead of hoping it is.
//!
//! Ported from `atomcode-tuix`'s `glyph.rs` and `TerminalCaps`, which learned
//! these rules the expensive way (Windows conhost tofu, `LANG=C` in CI).

use std::borrow::Cow;

/// How many colours the terminal has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Colors {
    /// No SGR at all — a pipe, a dumb terminal, `NO_COLOR`.
    None,
    /// The 16 classic ANSI colours.
    Ansi16,
    /// 256 indexed colours.
    #[default]
    Ansi256,
    /// 24-bit.
    True,
}

/// Whether this terminal can be shown a picture, and how.
///
/// Terminals grew a lot more capable than a grid of characters: 22 emulators
/// now do images, 9 speak kitty's protocol, 15 speak sixel, and people are
/// putting 3D viewports and rendered diagrams in them. The pattern that makes
/// that usable is not "detect and branch at every call site" — it is one
/// component that asks for a picture and degrades to ASCII where it cannot have
/// one, which is the same shape as the glyph fallback below.
///
/// Detected here only where the environment says so honestly. **Sixel cannot
/// be**: it needs a DA1 query and a reply, which is I/O — so the terminal
/// surface may raise this after probing, and this pure function never guesses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Graphics {
    /// Characters only.
    #[default]
    None,
    /// DEC sixel. Only ever set by a surface that probed for it.
    Sixel,
    /// iTerm2's inline images.
    ITerm2,
    /// kitty's protocol: PNG, animation, GPU-accelerated.
    Kitty,
}

/// What the terminal on the other end can render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Decorative Unicode (box drawing, `✓`, `▸`) renders rather than tofu.
    pub unicode: bool,
    pub colors: Colors,
    /// Pictures, if any. Nothing draws one yet — the field is here because it
    /// belongs to the shield, and the shield is the thing being built. A
    /// component that wants a picture will ask this rather than the
    /// environment.
    pub graphics: Graphics,
}

impl Default for Caps {
    /// Everything on. The default is what a modern terminal does, so a test or
    /// an embedder that never thinks about this gets the good rendering.
    fn default() -> Self {
        Self {
            unicode: true,
            colors: Colors::Ansi256,
            graphics: Graphics::None,
        }
    }
}

impl Caps {
    /// The plainest terminal: ASCII, no colour. What CI and a pipe get.
    pub fn plain() -> Self {
        Self {
            unicode: false,
            colors: Colors::None,
            graphics: Graphics::None,
        }
    }

    /// Read the environment. Called once, by the surface row.
    ///
    /// The rules are `atomcode-tuix`'s, unchanged — they encode real bug
    /// reports (legacy conhost showing `□`, `LANG=C` containers), and a second
    /// set of heuristics would mean the two front ends disagree about the same
    /// terminal.
    pub fn detect() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let term = env("TERM").unwrap_or_default();

        let ascii_forced = env("ATOMCODE_ASCII").is_some_and(|v| v != "0");
        let posix_locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|k| env(k))
            .any(|v| {
                let v = v.to_ascii_uppercase();
                v == "C" || v == "POSIX" || v.contains("ANSI_X3.4-1968")
            });
        // Windows before Terminal: conhost sets neither of these.
        let legacy_conhost =
            cfg!(windows) && env("WT_SESSION").is_none() && env("TERM_PROGRAM").is_none();

        let unicode = !(ascii_forced || term == "dumb" || posix_locale || legacy_conhost);

        let colors = if env("NO_COLOR").is_some() || term == "dumb" {
            Colors::None
        } else if env("COLORTERM").is_some_and(|v| v.contains("truecolor") || v.contains("24bit")) {
            Colors::True
        } else if term.contains("256") || term.contains("kitty") || env("WT_SESSION").is_some() {
            Colors::Ansi256
        } else if term.is_empty() {
            Colors::None
        } else {
            Colors::Ansi16
        };

        // Only what the environment states outright. Kitty and iTerm2 announce
        // themselves; sixel does not, and a guess here would have a component
        // send image escapes to a terminal that prints them as garbage.
        let graphics = if env("KITTY_WINDOW_ID").is_some()
            || term == "xterm-kitty"
            || env("TERM_PROGRAM").is_some_and(|v| v == "ghostty" || v == "WezTerm")
        {
            Graphics::Kitty
        } else if env("TERM_PROGRAM").is_some_and(|v| v == "iTerm.app") {
            Graphics::ITerm2
        } else {
            Graphics::None
        };

        Self {
            unicode,
            colors,
            graphics,
        }
    }

    /// Rewrite text this terminal cannot render.
    pub fn text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        downgrade(text, self.unicode)
    }
}

/// The ASCII stand-in for one decorative glyph, or `None` to leave it alone.
///
/// One display column in, one column out, so alignment survives the swap — the
/// property that makes this safe to apply after widths were computed.
///
/// CJK and ordinary punctuation are deliberately absent: they render fine, and
/// they are content rather than chrome. Rewriting a user's Chinese because the
/// terminal is old would be a much worse bug than a `□`.
///
/// **Wide characters are absent for the same reason, plus a harder one.** `✅`,
/// `⌛`, `💡` and the coloured circles are two columns; every ASCII stand-in is
/// one, so swapping them narrows the line and moves everything to its right —
/// on precisely the terminals that cannot report the damage back to us. They
/// could be padded to two, but they should not be here at all: this UI's own
/// chrome goes through [`Glyph`], whose whole set is verified narrow. A wide
/// emoji only ever arrives inside text from somewhere else — tool output, a
/// model's answer — and that is content.
fn ascii_for(ch: char) -> Option<&'static str> {
    Some(match ch {
        // status marks
        '\u{2713}' => "v",              // ✓ （✅ ✔ 是宽字符，见下方说明）
        '\u{2717}' | '\u{2718}' => "x", // ✗ ✘
        '\u{26A0}' => "!",              // ⚠
        '\u{2139}' | '\u{24D8}' => "i", // ℹ ⓘ

        // bullets / circles / diamonds
        '\u{25CF}' | '\u{25C6}' | '\u{25CE}' => "*", // ● ◆ ◎
        '\u{25CB}' | '\u{25E6}' | '\u{25C7}' | '\u{25A2}' => "o", // ○ ◦ ◇ ▢
        '\u{2022}' | '\u{2219}' => "*",              // • ∙
        // pointers
        '\u{25B8}' | '\u{25B6}' | '\u{25BA}' | '\u{276F}' => ">", // ▸ ▶ ► ❯
        '\u{25C2}' | '\u{25C0}' => "<",                           // ◂ ◀
        // arrows
        '\u{2192}' | '\u{21D2}' | '\u{21A6}' | '\u{21B3}' | '\u{2794}' | '\u{27A4}' => ">",
        '\u{2190}' | '\u{21D0}' | '\u{21A9}' | '\u{21B5}' | '\u{23CE}' => "<",
        '\u{2191}' | '\u{21D1}' => "^",
        '\u{2193}' | '\u{21D3}' => "v",
        '\u{2194}' | '\u{21D4}' | '\u{21BB}' | '\u{21BA}' => "~",
        // ellipsis and middle dot: chrome in this UI, and both ambiguous-width
        '\u{22EF}' | '\u{2026}' => ".", // ⋯ … — one column in, one out
        // media / state
        '\u{23F8}' => "=",
        '\u{23F9}' => "#",

        // box drawing
        '\u{2500}' | '\u{2550}' | '\u{2501}' => "-",
        '\u{2502}' | '\u{2551}' | '\u{2503}' | '\u{258E}' => "|",
        '\u{23BD}' | '\u{23BC}' => "_",
        // `⎿` is a tree-line tail, not a corner; box corners all become `+`
        // so a frame does not get one corner from each source. The glyph set
        // and this table are checked against each other.
        '\u{23BF}' => "`",
        '\u{2514}' | '\u{2570}' | '\u{250C}' | '\u{2510}' | '\u{2518}' | '\u{251C}'
        | '\u{2524}' | '\u{252C}' | '\u{2534}' | '\u{253C}' | '\u{256D}' | '\u{256E}'
        | '\u{256F}' | '\u{2554}' | '\u{2557}' | '\u{255A}' | '\u{255D}' | '\u{2560}'
        | '\u{2563}' | '\u{2566}' | '\u{2569}' | '\u{256C}' => "+",
        // blocks / shades
        '\u{2588}' | '\u{2580}' | '\u{2584}' | '\u{2592}' | '\u{2593}' => "#",
        '\u{2591}' => ".",

        _ => return None,
    })
}

/// Replace decorative glyphs with ASCII when the terminal lacks Unicode.
///
/// Borrowed (zero-copy) when nothing needs doing, which is the common case on
/// every modern terminal — the shield costs a scan, not an allocation.
pub fn downgrade(text: &str, unicode: bool) -> Cow<'_, str> {
    if unicode || text.is_ascii() || !text.chars().any(|c| ascii_for(c).is_some()) {
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

/// The chrome a component asks for by meaning, never by character.
///
/// A module writes `Glyph::Ok`, not `'✓'`. That is what lets the ASCII set be
/// swapped centrally, and what the layering gate checks for: a literal box
/// character above this layer is the shield having been bypassed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Horizontal,
    Vertical,
    Ok,
    Fail,
    Pending,
    Interrupted,
    Bullet,
    Pointer,
    /// The prompt marker.
    ///
    /// `❯` and not `›` on purpose: `›` is also ordinary punctuation, and the
    /// downgrade table must leave punctuation alone (rewriting `‹model›` in
    /// something the model said would be a far worse bug than a `□`). A glyph
    /// that is only ever chrome can live in both the glyph set and the table,
    /// and the two are checked against each other.
    Prompt,
    Separator,
    /// The filled part of a scrollbar.
    Thumb,
    /// Its unfilled track.
    Track,
}

impl Caps {
    pub fn g(&self, glyph: Glyph) -> &'static str {
        use Glyph::*;
        if self.unicode {
            match glyph {
                TopLeft => "┌",
                TopRight => "┐",
                BottomLeft => "└",
                BottomRight => "┘",
                Horizontal => "─",
                Vertical => "│",
                Ok => "✓",
                Fail => "✗",
                Pending => "⋯",
                Interrupted => "—",
                Bullet => "•",
                Pointer => "▸",
                Prompt => "❯",
                Separator => "·",
                Thumb => "█",
                Track => "│",
            }
        } else {
            match glyph {
                TopLeft | TopRight | BottomLeft | BottomRight => "+",
                Horizontal => "-",
                Vertical => "|",
                Ok => "v",
                Fail => "x",
                Pending => ".",
                Interrupted => "-",
                Bullet => "*",
                Pointer => ">",
                Prompt => ">",
                Separator => ".",
                Thumb => "#",
                Track => "|",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width;

    #[test]
    fn a_capable_terminal_pays_nothing() {
        assert!(matches!(
            downgrade("✓ done · ─────", true),
            Cow::Borrowed(_)
        ));
        assert!(matches!(downgrade("v done - ok", false), Cow::Borrowed(_)));
    }

    #[test]
    fn chinese_is_content_and_is_never_rewritten() {
        // The bug this guards against is worse than tofu: silently mangling
        // what the user actually said because their terminal is old.
        assert!(matches!(downgrade("写一个网页", false), Cow::Borrowed(_)));
        assert_eq!(downgrade("✓ 写网页 done", false), "v 写网页 done");
        let punctuation = "‹model› — 正文… ＋内容";
        assert_eq!(downgrade(punctuation, false), punctuation.replace('…', "."));
    }

    #[test]
    fn every_swap_keeps_the_column_count() {
        // The property the whole scheme rests on: downgrading may change what a
        // row says, never how wide it is. A swap that widened a line would move
        // a border by one column on exactly the terminals that cannot show the
        // problem to us.
        for glyph in [
            Glyph::TopLeft,
            Glyph::TopRight,
            Glyph::BottomLeft,
            Glyph::BottomRight,
            Glyph::Horizontal,
            Glyph::Vertical,
            Glyph::Ok,
            Glyph::Fail,
            Glyph::Pending,
            Glyph::Interrupted,
            Glyph::Bullet,
            Glyph::Pointer,
            Glyph::Prompt,
            Glyph::Separator,
            Glyph::Thumb,
            Glyph::Track,
        ] {
            let rich = Caps::default().g(glyph);
            let plain = Caps::plain().g(glyph);
            assert_eq!(
                width::str_width(rich),
                width::str_width(plain),
                "{glyph:?}: {rich:?} is {} cells, {plain:?} is {}",
                width::str_width(rich),
                width::str_width(plain)
            );
            assert_eq!(width::str_width(plain), 1, "{glyph:?} must be one column");
        }
    }

    /// Every character the table knows, so a new entry cannot quietly widen a
    /// line. There is no list to keep in step: the range walk *is* the list.
    fn mapped() -> Vec<(char, &'static str)> {
        (0u32..0x1FFFF)
            .filter_map(char::from_u32)
            .filter_map(|c| ascii_for(c).map(|a| (c, a)))
            .collect()
    }

    #[test]
    fn no_swap_in_the_whole_table_changes_a_line_width() {
        // The invariant the scheme rests on, checked over every entry rather
        // than over the handful a hand-written list would remember. The first
        // version of this file mapped `…` to `"..."` — one column becoming
        // three, which moves every border to its right by two, on exactly the
        // terminals that cannot report the problem back to us.
        let mut bad = Vec::new();
        for (ch, ascii) in mapped() {
            let before = width::str_width(&ch.to_string());
            let after = width::str_width(ascii);
            if before != after {
                bad.push(format!("{ch:?} ({before}) -> {ascii:?} ({after})"));
            }
            if !ascii.is_ascii() {
                bad.push(format!("{ch:?} -> {ascii:?} is not ASCII"));
            }
        }
        assert!(bad.is_empty(), "width-changing swaps:\n{}", bad.join("\n"));
    }

    #[test]
    fn the_table_is_not_empty_so_the_check_above_means_something() {
        // A range walk that found nothing would make every assertion vacuous.
        assert!(mapped().len() > 40, "only {} entries", mapped().len());
    }

    #[test]
    fn the_two_paths_to_ascii_agree() {
        // There are two ways a `┌` becomes ASCII: a module asks for
        // `Glyph::TopLeft` and gets `+`, or a literal `┌` goes through the
        // downgrade table on its way to the terminal. If they disagree, a box
        // gets one corner from each and looks broken — which is exactly what
        // happened: the table mapped `└` to a backtick (good for tree lines,
        // wrong for a box) while the glyph set said `+`.
        for glyph in [
            Glyph::TopLeft,
            Glyph::TopRight,
            Glyph::BottomLeft,
            Glyph::BottomRight,
            Glyph::Horizontal,
            Glyph::Vertical,
            Glyph::Ok,
            Glyph::Fail,
            Glyph::Bullet,
            Glyph::Pointer,
            Glyph::Prompt,
            Glyph::Thumb,
            Glyph::Track,
        ] {
            let rich = Caps::default().g(glyph);
            assert_eq!(
                downgrade(rich, false),
                Caps::plain().g(glyph),
                "{glyph:?}: the table and the glyph set disagree about {rich:?}"
            );
        }
    }

    #[test]
    fn the_ascii_set_really_is_ascii() {
        // Otherwise the fallback tofus on exactly the terminal it exists for.
        for glyph in [Glyph::TopLeft, Glyph::Ok, Glyph::Bullet, Glyph::Pointer] {
            assert!(Caps::plain().g(glyph).is_ascii(), "{glyph:?}");
        }
    }

    #[test]
    fn a_downgraded_string_is_pure_ascii_for_chrome() {
        let framed = "┌─ title ─┐ ✓ ▸ • ⋯";
        let out = downgrade(framed, false);
        assert!(out.is_ascii(), "{out}");
    }

    #[test]
    fn detection_reads_the_environment_once_and_says_what_it_found() {
        // Not asserting a particular machine's answer — that would be a test of
        // the developer's shell. Asserting the shape: detection terminates and
        // produces a usable value.
        let caps = Caps::detect();
        assert!(matches!(
            caps.colors,
            Colors::None | Colors::Ansi16 | Colors::Ansi256 | Colors::True
        ));
    }
}
