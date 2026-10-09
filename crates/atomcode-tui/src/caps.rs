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
/// What a build or a person says about the terminal, over what detection found.
///
/// Detection reads the environment, and the environment lies in both
/// directions: a corporate image ships `TERM=xterm` on an emulator that does
/// 24-bit colour, and a CI runner claims a colour terminal while rendering into
/// a log file. `ATOMCODE_ASCII`/`ATOMCODE_UNICODE` were the only way to say
/// otherwise, and they answer one of the three questions — which is why a
/// downstream build that ships to a fixed fleet of terminals ended up patching
/// detection itself.
///
/// Empty by default: nothing overridden, detection stands. Set from the surface
/// row's config (`unicode`, `colors`, `cell_background`), so a fleet states what
/// its terminals do once, in the tree, rather than per machine in an
/// environment variable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub unicode: Option<bool>,
    pub colors: Option<Colors>,
    pub cell_background: Option<bool>,
}

impl Overrides {
    /// Whether anything is overridden at all.
    pub fn any(&self) -> bool {
        self.unicode.is_some() || self.colors.is_some() || self.cell_background.is_some()
    }

    /// Put these over `caps`. A field nobody set keeps what was detected.
    ///
    /// `palette` is deliberately not overridable: it is a *measurement* of what
    /// the terminal rendered, and a build asserting a measurement it never took
    /// is how both unreadable palettes shipped.
    pub fn over(&self, caps: Caps) -> Caps {
        Caps {
            unicode: self.unicode.unwrap_or(caps.unicode),
            // A stated answer either way is the whole answer: "the font has the
            // glyphs" and "ASCII only" both leave no basic middle ground.
            basic_glyphs: self.unicode.is_none() && caps.basic_glyphs,
            colors: self.colors.unwrap_or(caps.colors),
            cell_background: self.cell_background.unwrap_or(caps.cell_background),
            ..caps
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Decorative Unicode (box drawing, `✓`, `▸`) renders rather than tofu.
    pub unicode: bool,
    /// With `unicode` off: the font still draws the line-drawing and the few
    /// symbols every console font carries — box drawing, `•` `·` `○` `●` `◆`,
    /// arrows, block elements — so those are kept, and only the rest
    /// (`❯` `▸` `✓` `⚑` `⏸` braille…) goes to ASCII ([`console_safe`]).
    ///
    /// A classic Windows console window (conhost: no `WT_SESSION`, no
    /// `TERM_PROGRAM`). Its fonts — the CJK raster and TrueType ones a Chinese
    /// Windows ships, Consolas — have the box set but not the decorative one,
    /// so with full Unicode the prompt, the notice flag and every mode icon
    /// arrived as `□`; with plain ASCII the panels became `+--+`. This is the
    /// third answer between the two.
    pub basic_glyphs: bool,
    pub colors: Colors,
    /// The colours the terminal actually renders: its background, and its
    /// sixteen slots, as measured by the surface row.
    ///
    /// Not a light/dark flag. One bit cannot answer "will this be legible" —
    /// two terminals can both be dark and disagree by half the luminance range,
    /// and every signal that produces the bit (OSC 11, `COLORFGBG`, a config
    /// line) can be missing, stale or inverted. Both palettes that shipped
    /// before this were unreadable, in opposite directions, for exactly that
    /// reason. What is stored is a measurement; `theme.rs` does arithmetic on
    /// it.
    pub palette: crate::theme::Palette,
    /// Whether the terminal paints a cell's **background** colour.
    ///
    /// Not the same question as [`colors`](Self::colors): a terminal can have 256
    /// of them and still drop the background, and when it does, anything drawn
    /// with one comes apart. Tuix learned this the hard way — its mascot packs two
    /// vertical pixels per cell (`▀` with fg above, bg below), and on a bare ssh
    /// client the glyphs arrived but the backgrounds did not, so the art
    /// fragmented into its top half. It gates on `modern_emulator`/`jediterm` for
    /// that reason; those two variables are what a shield can honestly read.
    ///
    /// False by default, like everything a terminal has not said. A caller that
    /// needs it can encode its own gate (a plain foreground glyph always works);
    /// what it must not do is assume.
    pub cell_background: bool,
    /// Pictures, if any. Nothing draws one yet — the field is here because it
    /// belongs to the shield, and the shield is the thing being built. A
    /// component that wants a picture will ask this rather than the
    /// environment.
    pub graphics: Graphics,
    /// Which key takes a picture off the clipboard in this terminal — what a
    /// hint about one has to name.
    pub paste_image: PasteImage,
}

/// How a picture on the clipboard gets onto the line.
///
/// Its own answer, and the shield's, because it is about the platform: Windows
/// Terminal and conhost bind `ctrl+v` to their own paste, which forwards the
/// clipboard's *text* only — an image-only clipboard sends nothing, so the key
/// never reaches this screen. `ctrl+alt+v` gets through there, and `/paste`
/// gets through everywhere. Elsewhere `ctrl+v` arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PasteImage {
    /// `ctrl+v`.
    #[default]
    CtrlV,
    /// `ctrl+alt+v`, or `/paste`.
    CtrlAltVOrCommand,
}

impl Default for Caps {
    /// Everything on. The default is what a modern terminal does, so a test or
    /// an embedder that never thinks about this gets the good rendering.
    fn default() -> Self {
        Self {
            unicode: true,
            basic_glyphs: false,
            colors: Colors::Ansi256,
            palette: crate::theme::Palette::assumed(crate::theme::Theme::Dark),
            // What a modern terminal does, which is the whole meaning of this
            // default. `detect` is where a real answer comes from.
            cell_background: true,
            graphics: Graphics::None,
            paste_image: PasteImage::CtrlV,
        }
    }
}

/// Whether to ask the terminal for the keyboard protocol ([`crate::ansi::KEYS_ON`]).
///
/// tuix's rule (`should_enable_kitty_keyboard`), which is not kept twice for
/// nothing — both exclusions are bug reports:
///
/// - **Windows.** Keys are read through the console API, which has no `CSI u`
///   decoder: a terminal that honours the request (VS Code since 1.109,
///   Windows Terminal) encodes esc as `ESC [ 27 u`, and ConPTY delivers it as
///   the characters `[27u` into the composer. Esc stopped nothing, ctrl-c
///   (`[99;5u`) cancelled nothing. Shift-enter is distinguishable there
///   without the protocol: the console API reports the modifier.
/// - **JediTerm** (IntelliJ-platform terminals). It advertises the protocol and
///   re-frames mouse reports as `CSI u` keys while it is on, so moving the
///   pointer typed coordinates into the composer.
///
/// `ATOMCODE_KITTY` forces it either way — except on Windows, where nothing can
/// read what it turns on, so forcing it would only bring the bug back (tuix
/// gates the same way). `ATOMCODE_JEDITERM` says what the terminal is when a
/// launcher dropped `TERMINAL_EMULATOR`.
pub fn wants_keyboard_protocol() -> bool {
    keyboard_protocol_for(cfg!(windows), |k| std::env::var(k).ok())
}

fn keyboard_protocol_for(windows: bool, env: impl Fn(&str) -> Option<String>) -> bool {
    let truthy = |v: String| v == "1" || v.eq_ignore_ascii_case("true");
    if windows {
        return false;
    }
    if let Some(forced) = env("ATOMCODE_KITTY").filter(|v| !v.is_empty()) {
        return truthy(forced);
    }
    let jediterm = match env("ATOMCODE_JEDITERM").filter(|v| !v.is_empty()) {
        Some(forced) => truthy(forced),
        None => env("TERMINAL_EMULATOR").as_deref() == Some("JetBrains-JediTerm"),
    };
    !jediterm
}

/// Whether the terminal reports the mouse at all — whether asking it to
/// (`ESC[?1002h ESC[?1006h`) gets anything back.
///
/// HarmonyOS's own Terminal does not: it sends nothing for a wheel, a drag or a
/// click (measured on HarmonyOS PC 6.1, `cat -v` with both modes set), **and**
/// it stops its own text selection the moment it is asked to report. So taking
/// the mouse there bought nothing and cost everything — no selecting, no
/// scrolling with the wheel — and `ctrl+g` (handing it back) was the only way
/// to select a word. There the mouse stays the terminal's.
///
/// `ATOMCODE_MOUSE=1` says otherwise, for a terminal there that learns to.
pub fn mouse_reported() -> bool {
    mouse_reported_for(cfg!(target_env = "ohos"), |k| std::env::var(k).ok())
}

fn mouse_reported_for(ohos: bool, env: impl Fn(&str) -> Option<String>) -> bool {
    match env("ATOMCODE_MOUSE").filter(|v| !v.is_empty()) {
        Some(forced) => forced == "1" || forced.eq_ignore_ascii_case("true"),
        None => !ohos,
    }
}

impl Caps {
    /// The plainest terminal: ASCII, no colour. What CI and a pipe get.
    pub fn plain() -> Self {
        Self {
            unicode: false,
            basic_glyphs: false,
            colors: Colors::None,
            palette: crate::theme::Palette::assumed(crate::theme::Theme::Dark),
            cell_background: false,
            graphics: Graphics::None,
            paste_image: PasteImage::CtrlV,
        }
    }

    /// Read the environment, then let `overrides` have the last word.
    pub fn detect_with(overrides: Overrides) -> Self {
        overrides.over(Self::detect())
    }

    /// Read the environment. Called once, by the surface row.
    ///
    /// The rules are `atomcode-tuix`'s, with one deliberate divergence: Windows
    /// no longer forces ASCII when it announces neither `WT_SESSION` nor
    /// `TERM_PROGRAM` (see [`unicode_for`]), because this screen only runs on a
    /// Windows console that already speaks ANSI — the same guarantee
    /// [`colors_for`] leans on. The rest is kept because it encodes real bug
    /// reports (`LANG=C` containers, `TERM=dumb`), and a second set of
    /// heuristics would mean the two front ends disagree about the same
    /// terminal.
    pub fn detect() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let term = env("TERM").unwrap_or_default();

        let basic_glyphs = basic_glyphs_for(&env, cfg!(windows));
        let unicode = unicode_for(&env) && !basic_glyphs;

        let colors = colors_for(&env, cfg!(windows));

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
            basic_glyphs,
            colors,
            // Assumed until the surface row measures it. Everything detected
            // here comes from the environment; asking the terminal what colour
            // it is is I/O, and belongs to the row that owns the tty.
            palette: crate::theme::Palette::assumed(crate::theme::Theme::Dark),
            // The one bit the environment *can* answer, and tuix's answer is kept
            // verbatim: an emulator that announces itself (or JediTerm, a local
            // IDE terminal) paints cell backgrounds; a bare ssh client or a
            // legacy console does not, and art that assumes otherwise fragments.
            // See the field's docs.
            //
            // Windows is no longer in that list of doubts: this screen only runs
            // there on a console that executes escapes
            // ([`crate::surface::console_speaks_ansi`]), and a conhost with VT
            // processing paints backgrounds — so a PowerShell window gets the
            // mascot Git Bash's mintty always got (the same reasoning
            // [`colors_for`] and [`unicode_for`] already apply).
            cell_background: cfg!(windows)
                || env("WT_SESSION").is_some()
                || env("TERM_PROGRAM").is_some()
                || env("TERM").is_some_and(|t| t.contains("jediterm")),
            graphics,
            paste_image: if cfg!(windows) {
                PasteImage::CtrlAltVOrCommand
            } else {
                PasteImage::CtrlV
            },
        }
    }

    /// Rewrite text this terminal cannot render.
    pub fn text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        downgrade_keeping(text, self.unicode, self.basic_glyphs)
    }
}

/// Whether this character survives a terminal that has no Unicode.
///
/// `true` for ASCII and for the decorative characters [`ascii_for`] rewrites;
/// `false` for braille and anything else with no stand-in. A **bitmap** has to
/// ask this: a grid of tofu is not a picture. It is the same reasoning
/// [`Caps::spinner`] applies to its own set — see [`SPINNER`].
pub fn has_ascii_stand_in(ch: char) -> bool {
    ch.is_ascii() || ascii_for(ch).is_some()
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
        '\u{273B}' => "*",              // ✻ 回合收尾标记(窄字符)
        '\u{26A0}' => "!",              // ⚠
        '\u{2139}' | '\u{24D8}' => "i", // ℹ ⓘ
        // A mark that asks to be looked at: the background panel's "needs
        // input" star and the notice block's flag. `!` rather than `*`, which
        // the sparkles above already spend — these two say something is
        // waiting, not that something finished.
        '\u{2731}' | '\u{2691}' => "!", // ✱ ⚑

        // bullets / circles / diamonds
        '\u{25CF}' | '\u{25C6}' | '\u{25CE}' => "*", // ● ◆ ◎
        '\u{25CB}' | '\u{25E6}' | '\u{25C7}' | '\u{25A2}' => "o", // ○ ◦ ◇ ▢
        // check boxes
        '\u{2610}' => "o",              // ☐
        '\u{2612}' => "x",              // ☒
        '\u{2022}' | '\u{2219}' => "*", // • ∙
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
        // The diff view's "these two hunks are far apart" mark. A one-cell `:`
        // rather than the `...` a `⋮` reads like: every other swap here trades
        // one column for one, and `...` is three, which moves the rest of the
        // row. `atomcode-tuix` has this same gap — its comment at
        // `render/diff.rs` says the mark is downgraded, but U+22EE is not in its
        // table either, so both front ends drew it raw.
        '\u{22EE}' => ":", // ⋮
        // media / state
        '\u{23F8}' => "=",
        '\u{23F9}' => "#",
        '\u{23F5}' => ">",

        // box drawing
        '\u{2500}' | '\u{2550}' | '\u{2501}' => "-",
        '\u{2502}' | '\u{2551}' | '\u{2503}' | '\u{258E}' | '\u{258F}' => "|",
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
/// How many colours, from what the environment says.
///
/// An empty `TERM` means nothing on Windows, where no console sets it: Windows
/// Terminal, VS Code and the console a PowerShell window opens in all leave it
/// unset. Read as "no colour", that turned the screen black-and-white whenever
/// the one variable that is set — Windows Terminal's `WT_SESSION` — did not
/// survive the launch, which it does not when PowerShell is started first and
/// handed to Windows Terminal as the default terminal. The classic screen
/// coloured that same session (it colours any tty without `NO_COLOR`), so it
/// read as a regression of the new one. On Windows this screen only runs once
/// the console has agreed to execute escape sequences
/// ([`crate::surface::console_speaks_ansi`]), and every console that can has
/// 256 colours — so that is the answer there. Elsewhere an empty `TERM` is
/// still a terminal that has said nothing about itself, and stays uncoloured.
pub fn colors_for(env: &dyn Fn(&str) -> Option<String>, windows: bool) -> Colors {
    let term = env("TERM").unwrap_or_default();
    if env("NO_COLOR").is_some() || term == "dumb" {
        Colors::None
    } else if env("COLORTERM").is_some_and(|v| v.contains("truecolor") || v.contains("24bit")) {
        Colors::True
    } else if term.contains("256") || term.contains("kitty") || env("WT_SESSION").is_some() {
        Colors::Ansi256
    } else if term.is_empty() {
        if windows {
            Colors::Ansi256
        } else {
            Colors::None
        }
    } else {
        Colors::Ansi16
    }
}

/// Whether decorative Unicode renders, from what the environment says.
///
/// Mirror of [`colors_for`]'s Windows reasoning, and the reason there is no
/// Windows arm here any more. This screen only runs on Windows once the console
/// has agreed to execute escape sequences ([`crate::surface::console_speaks_ansi`])
/// — a modern console, or a real VT pseudo-terminal like mintty — both of which
/// draw box-drawing and the Geometric-Shapes chrome this UI uses. The old rule
/// forced ASCII on any Windows console that announced neither `WT_SESSION` nor
/// `TERM_PROGRAM`, written when bare conhost shipped fonts that drew `┌`/`✓`/`❯`
/// as `□`; it predated that gate and handed a capable console `+--+`, which the
/// classic screen drew as `┌──┐`, so the new screen read as a regression there —
/// the same shape of bug as the black-and-white colours [`colors_for`] fixed. So
/// Windows is no longer special: the two explicit overrides have the last word,
/// and a font that truly lacks the glyphs is handled by `ATOMCODE_ASCII=1`.
pub fn unicode_for(env: &dyn Fn(&str) -> Option<String>) -> bool {
    if env("ATOMCODE_UNICODE").is_some_and(|v| v != "0") {
        // "My font has the glyphs" — the opt-in kept from tuix, winning over
        // every guess below.
        return true;
    }
    let term = env("TERM").unwrap_or_default();
    let posix_locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|k| env(k))
        .any(|v| {
            let v = v.to_ascii_uppercase();
            v == "C" || v == "POSIX" || v.contains("ANSI_X3.4-1968")
        });
    !(env("ATOMCODE_ASCII").is_some_and(|v| v != "0") || term == "dumb" || posix_locale)
}

/// Whether this is a console whose font has only the basic glyph set
/// ([`Caps::basic_glyphs`]): Windows, no terminal that announces itself — not
/// Windows Terminal (`WT_SESSION`), not mintty or VS Code (`TERM_PROGRAM`), not
/// a JetBrains terminal — and nobody saying the font has them
/// (`ATOMCODE_UNICODE`) or that it has none (`ATOMCODE_ASCII`, plain ASCII).
pub fn basic_glyphs_for(env: &dyn Fn(&str) -> Option<String>, windows: bool) -> bool {
    windows
        && env("WT_SESSION").is_none()
        && env("TERM_PROGRAM").is_none()
        && !env("TERM").is_some_and(|t| t.contains("jediterm"))
        && env("TERMINAL_EMULATOR").as_deref() != Some("JetBrains-JediTerm")
        && !env("ATOMCODE_UNICODE").is_some_and(|v| v != "0")
        && !env("ATOMCODE_ASCII").is_some_and(|v| v != "0")
}

/// The decorative characters a basic console font draws ([`Caps::basic_glyphs`]):
/// box drawing, block elements, the arrows and the handful of bullets and shapes
/// every CJK and Western console font carries. Everything else [`ascii_for`]
/// knows is rewritten there.
pub fn console_safe(ch: char) -> bool {
    matches!(ch,
        '\u{2500}'..='\u{257F}'          // box drawing
        // Block elements a GBK font carries: the lower eighths to the full
        // block, the left eighths, and the dark shade. Not `▀` (U+2580): it is
        // outside GBK, and drawn as tofu on a Chinese console — half-block
        // pictures there are drawn with `▄` alone.
        | '\u{2581}'..='\u{258F}' | '\u{2593}'
        | '\u{2190}'..='\u{2193}'        // ← ↑ → ↓
        | '\u{2022}' | '\u{00B7}'         // • ·
        | '\u{25CB}' | '\u{25CF}' | '\u{25C6}' | '\u{25C7}' | '\u{25CE}' // ○ ● ◆ ◇ ◎
        | '\u{2026}'                      // …
    )
}

/// Whether a basic console gets an ASCII stand-in for `ch` rather than `ch`.
pub fn rewritten_on_basic(ch: char) -> bool {
    !console_safe(ch) && ascii_for(ch).is_some()
}

/// A glyph for a terminal that is full Unicode, basic ([`Caps::basic_glyphs`])
/// or ASCII: on a basic console the Unicode one when its font draws it, else
/// the ASCII one — except the result gutter, whose `⎿` the font lacks and whose
/// ASCII `` ` `` reads worse than the corner it is a cousin of.
pub fn glyph_for(unicode: bool, basic: bool, which: Glyph) -> &'static str {
    if unicode || !basic {
        return glyph(unicode, which);
    }
    if which == Glyph::Gutter {
        return "\u{2514}";
    }
    let rich = glyph(true, which);
    if rich.chars().all(console_safe) {
        rich
    } else {
        glyph(false, which)
    }
}

/// [`downgrade`], keeping what a basic console font draws when `basic`.
pub fn downgrade_keeping(text: &str, unicode: bool, basic: bool) -> Cow<'_, str> {
    if !basic || unicode {
        return downgrade(text, unicode);
    }
    // The emoji variation selector goes too: a console font has no emoji, and
    // the selector it does not know arrives as a `□` after the symbol. It takes
    // no cell, so dropping it moves nothing.
    if text.is_ascii()
        || !text
            .chars()
            .any(|c| c == '\u{FE0F}' || rewritten_on_basic(c))
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match ascii_for(c) {
            _ if c == '\u{FE0F}' => {}
            // The result gutter as [`glyph_for`] draws it here: the corner, not
            // the `` ` `` the ASCII table has for it. A tool row is laid out in
            // full Unicode and reaches this rewrite with its `⎿` still on it.
            _ if c == '\u{23BF}' => out.push('\u{2514}'),
            Some(stand_in) if !console_safe(c) => out.push_str(stand_in),
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

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
    /// A turn's clean close — a `✻` sparkle, the way Claude Code leads its
    /// completion line. Distinct from [`Glyph::Ok`] (the tool-result tick) so the
    /// turn summary carries a mark of its own rather than borrowing the check.
    Sparkle,
    Bullet,
    /// Present, but not in force: a tool a person turned off, drawn beside the
    /// filled [`Glyph::Bullet`] of one that is on.
    Hollow,
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
    /// The marker a tool call opens with.
    ToolMark,
    /// The gutter its result hangs from.
    Gutter,
    /// Points at what is below the fold.
    Down,
    /// This session is paused where it is: `plan` mode, which explores and
    /// declines to touch. Chrome for the mode badge, so it lives in both tables.
    Pause,
    /// This session goes ahead: `accept edits` and `auto`. Drawn twice for the
    /// one mode that asks nothing at all.
    Play,
    /// A ticked box: an answer picked in a multiple choice, a question of a
    /// batch that has its answer.
    Checked,
    /// Its empty twin.
    Unchecked,
    /// Points back and forward — a question panel's page tabs.
    Left,
    Right,
}

/// The frames a spinner cycles through, one per redraw.
///
/// Here rather than in a module because there is one answer to "what does
/// work in progress look like": two panels cycling two different sets, or the
/// same set out of phase, is two front ends on one screen. Both callers index
/// it by `Moment::tick`, so they turn together.
///
/// Braille, and deliberately not a [`Glyph`]: the downgrade table trades one
/// glyph for one ASCII cell, and there is no one-cell ASCII spinner that reads
/// as motion. These are one column everywhere, which is the property that
/// matters — a frame two cells wide would move the text beside it every tick.
///
/// Which is why the set is chosen by terminal rather than rewritten afterwards:
/// a column of tofu is not a spinner, and [`Caps::spinner`] is the only way to
/// take a frame. See [`ASCII_SPINNER`].
pub const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The spinner for a terminal with no Unicode: the oldest one there is.
///
/// `- \ | /` rather than braille blanks or ASCII digits: motion is the whole
/// job of this row, and it is the only property that has to survive the swap.
/// All four are ASCII, one column, and unambiguous in every font — including
/// the legacy Windows console, which draws the first set of eight as eight
/// tofu boxes.
pub const ASCII_SPINNER: [&str; 4] = ["-", "\\", "|", "/"];

impl Caps {
    /// The spinner frame for this tick, in a set this terminal can draw.
    ///
    /// The one way to take a frame. A module that indexed [`SPINNER`] itself
    /// would draw braille on a terminal that has already been told to stay
    /// inside ASCII, and the tofu would arrive on the one row that is *all*
    /// motion — the downgrade table cannot rescue it (see [`SPINNER`]), so the
    /// set has to change instead.
    pub fn spinner(&self, tick: u64) -> &'static str {
        if self.unicode {
            SPINNER[(tick as usize) % SPINNER.len()]
        } else {
            ASCII_SPINNER[(tick as usize) % ASCII_SPINNER.len()]
        }
    }

    pub fn g(&self, glyph: Glyph) -> &'static str {
        glyph_for(self.unicode, self.basic_glyphs, glyph)
    }
}

/// A decorative glyph, as a terminal that does or does not do Unicode writes it.
///
/// A free function because [`crate::block::ShapeCaps`] needs the same table: a
/// block cannot hold a whole `Caps` (it also carries the palette), but a glyph
/// depends on `unicode` alone.
///
/// Both tables are **one column in, one column out** — the property that makes
/// the swap safe to apply after widths were computed, and the reason this set
/// holds only narrow characters (see [`ascii_for`]).
pub fn glyph(unicode: bool, glyph: Glyph) -> &'static str {
    use Glyph::*;
    if unicode {
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
            Sparkle => "✻",
            Bullet => "•",
            Hollow => "○",
            Pointer => "▸",
            Prompt => "❯",
            Separator => "·",
            Thumb => "█",
            Track => "│",
            ToolMark => "●",
            Gutter => "⎿",
            Down => "↓",
            Pause => "\u{23F8}",
            Play => "\u{23F5}",
            Checked => "\u{2612}",
            Unchecked => "\u{2610}",
            Left => "\u{2190}",
            Right => "\u{2192}",
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
            Sparkle => "*",
            Bullet => "*",
            Hollow => "o",
            Pointer => ">",
            Prompt => ">",
            Separator => ".",
            Thumb => "#",
            Track => "|",
            ToolMark => "*",
            Gutter => "`",
            Down => "v",
            Pause => "=",
            Play => ">",
            Checked => "x",
            Unchecked => "o",
            Left => "<",
            Right => ">",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        }
    }

    /// HarmonyOS's terminal reports no mouse, so the mouse stays its own there
    /// — unless told otherwise; everywhere else it is taken as before.
    #[test]
    fn harmonyos_keeps_the_mouse_unless_told_otherwise() {
        assert!(!mouse_reported_for(true, |_| None));
        assert!(mouse_reported_for(true, |k| (k == "ATOMCODE_MOUSE").then(|| "1".into())));
        assert!(mouse_reported_for(false, |_| None));
        assert!(!mouse_reported_for(false, |k| (k == "ATOMCODE_MOUSE").then(|| "0".into())));
    }

    /// A classic Windows console window is the basic one; a terminal that
    /// announces itself, a JetBrains terminal, a person's stated answer, and
    /// anything not on Windows are not.
    #[test]
    fn only_a_classic_windows_console_is_basic() {
        assert!(basic_glyphs_for(&env_of(&[]), true));
        assert!(!basic_glyphs_for(&env_of(&[]), false), "not Windows");
        assert!(!basic_glyphs_for(&env_of(&[("WT_SESSION", "x")]), true));
        assert!(!basic_glyphs_for(
            &env_of(&[("TERM_PROGRAM", "mintty")]),
            true
        ));
        assert!(!basic_glyphs_for(
            &env_of(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")]),
            true
        ));
        assert!(!basic_glyphs_for(
            &env_of(&[("ATOMCODE_UNICODE", "1")]),
            true
        ));
        assert!(!basic_glyphs_for(&env_of(&[("ATOMCODE_ASCII", "1")]), true));
    }

    /// On a basic console: the box, the bullets and the arrows stay — no
    /// `+--+` — and what the font lacks (`❯ ▸ ✓ ⚑ ⏸`) becomes ASCII, column for
    /// column.
    #[test]
    fn a_basic_console_keeps_what_its_font_draws() {
        assert_eq!(glyph_for(false, true, Glyph::Horizontal), "─");
        assert_eq!(glyph_for(false, true, Glyph::TopLeft), "┌");
        assert_eq!(glyph_for(false, true, Glyph::Bullet), "•");
        assert_eq!(glyph_for(false, true, Glyph::ToolMark), "●");
        assert_eq!(glyph_for(false, true, Glyph::Prompt), ">");
        assert_eq!(glyph_for(false, true, Glyph::Pointer), ">");
        assert_eq!(glyph_for(false, true, Glyph::Ok), "v");
        assert_eq!(glyph_for(false, true, Glyph::Pause), "=");
        assert_eq!(glyph_for(false, true, Glyph::Gutter), "└");
        assert_eq!(
            glyph_for(true, false, Glyph::Prompt),
            "❯",
            "full Unicode untouched"
        );
        assert_eq!(
            glyph_for(false, false, Glyph::Horizontal),
            "-",
            "plain ASCII untouched"
        );

        let text = "⚑ 已更新 · ❯ ▸ ✓ ─┌┐ • ● ← ▀▄█ ⏸";
        let kept = downgrade_keeping(text, false, true);
        assert_eq!(
            kept, "! 已更新 · > > v ─┌┐ • ● ← #▄█ =",
            "`▀` is not in the font"
        );
        assert_eq!(
            crate::width::str_width(&kept),
            crate::width::str_width(text),
            "one column for one"
        );
        assert_eq!(
            downgrade_keeping(text, true, true),
            text,
            "full Unicode untouched"
        );
        assert_eq!(
            downgrade_keeping("⎿ Grep(x)", false, true),
            "└ Grep(x)",
            "the result gutter is the corner glyph_for draws, not a backtick"
        );
        assert_eq!(
            downgrade_keeping("晴 ☀\u{FE0F}", false, true),
            "晴 ☀",
            "the emoji selector the font lacks goes"
        );
        assert_eq!(
            downgrade_keeping(text, false, false),
            downgrade(text, false)
        );
    }

    /// A stated answer about the font is the whole answer.
    #[test]
    fn an_override_leaves_no_basic_middle_ground() {
        let basic = Caps {
            unicode: false,
            basic_glyphs: true,
            ..Caps::default()
        };
        for unicode in [true, false] {
            let over = Overrides {
                unicode: Some(unicode),
                ..Overrides::default()
            }
            .over(basic);
            assert!(!over.basic_glyphs);
            assert_eq!(over.unicode, unicode);
        }
        assert!(
            Overrides::default().over(basic).basic_glyphs,
            "nothing stated, kept"
        );
    }

    /// The keyboard protocol is asked for only where its keys can be read:
    /// not on Windows (`[27u` typed for esc in VS Code), not in JediTerm
    /// (mouse moves typed as keys), and either way when forced.
    #[test]
    fn the_keyboard_protocol_is_asked_for_only_where_it_can_be_read() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let vscode: &[(&str, &str)] = &[("TERM_PROGRAM", "vscode")];
        assert!(
            !keyboard_protocol_for(true, env(vscode)),
            "VS Code on Windows"
        );
        assert!(
            keyboard_protocol_for(false, env(vscode)),
            "VS Code elsewhere"
        );
        assert!(!keyboard_protocol_for(
            false,
            env(&[("TERMINAL_EMULATOR", "JetBrains-JediTerm")])
        ));
        assert!(!keyboard_protocol_for(
            false,
            env(&[("ATOMCODE_JEDITERM", "1")])
        ));
        assert!(
            keyboard_protocol_for(false, env(&[("ATOMCODE_KITTY", "1")])),
            "forced on"
        );
        assert!(
            !keyboard_protocol_for(true, env(&[("ATOMCODE_KITTY", "1")])),
            "not even forced on Windows: nothing there can read it"
        );
        assert!(
            !keyboard_protocol_for(false, env(&[("ATOMCODE_KITTY", "0")])),
            "forced off"
        );
    }
    use crate::width;

    /// A build says what its terminals do, and detection does not get a vote on
    /// the fields it named — nor on the ones it did not.
    ///
    /// The case this is for: a fleet whose `TERM` says `xterm` on emulators that
    /// do 24-bit colour. Before it, the only thing anyone could say was
    /// `ATOMCODE_ASCII`, which answers one of the three questions.
    #[test]
    fn a_build_can_say_what_its_terminals_do() {
        let detected = Caps::plain();
        let nothing = Overrides::default();
        assert!(!nothing.any());
        assert_eq!(
            nothing.over(detected),
            detected,
            "nothing said, nothing done"
        );

        let said = Overrides {
            colors: Some(Colors::True),
            ..Default::default()
        };
        assert!(said.any());
        let got = said.over(detected);
        assert_eq!(got.colors, Colors::True, "what the build said");
        assert!(
            !got.unicode,
            "what it did not say is still what was detected"
        );
        assert!(!got.cell_background);

        // And in the other direction: a runner that claims a colour terminal
        // while rendering into a log file.
        let quiet = Overrides {
            unicode: Some(false),
            colors: Some(Colors::None),
            cell_background: Some(false),
        };
        assert_eq!(quiet.over(Caps::default()), Caps::plain());
    }

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
            Glyph::Sparkle,
            Glyph::Bullet,
            Glyph::Hollow,
            Glyph::Pointer,
            Glyph::Prompt,
            Glyph::Separator,
            Glyph::Thumb,
            Glyph::Track,
            Glyph::ToolMark,
            Glyph::Gutter,
            Glyph::Down,
            Glyph::Pause,
            Glyph::Play,
            Glyph::Checked,
            Glyph::Unchecked,
            Glyph::Left,
            Glyph::Right,
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
            Glyph::Sparkle,
            Glyph::Bullet,
            Glyph::Hollow,
            Glyph::Pointer,
            Glyph::Prompt,
            Glyph::Thumb,
            Glyph::Track,
            Glyph::Down,
            Glyph::Checked,
            Glyph::Unchecked,
            Glyph::Left,
            Glyph::Right,
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

    /// The chrome marks this UI actually draws, and the two answers that are
    /// not one cell for one.
    ///
    /// A mark that reaches a terminal with no glyph for it is a `□` in the
    /// middle of a panel, so each of these has to arrive as ASCII. They are all
    /// narrow (`east_asian_width` = N), which is what lets the swap keep the
    /// row's width — the property `no_swap_in_the_whole_table_changes_a_line_width`
    /// checks over the whole table, and the reason these could be added at all.
    #[test]
    fn the_marks_this_ui_draws_reach_a_terminal_without_unicode_as_ascii() {
        // The background panel's three session states, the notice block's flag,
        // and the diff view's "these hunks are far apart" gap.
        for (rich, plain) in [
            ("\u{2731}", "!"), // ✱ needs input
            ("\u{2691}", "!"), // ⚑ notice
            ("\u{22EE}", ":"), // ⋮ hunk gap
            ("\u{25E6}", "o"), // ◦ working
        ] {
            assert_eq!(
                downgrade(rich, false),
                plain,
                "{rich:?} has no ASCII stand-in — it would draw as tofu"
            );
            assert_eq!(
                crate::width::str_width(&downgrade(rich, false)),
                crate::width::str_width(rich),
                "{rich:?} -> {plain:?} changed the row's width"
            );
        }
    }

    /// A key's glyph is not chrome, and the table must leave it be.
    ///
    /// `⏎` is in the table — mapped to `<`, because there it shares an arm with
    /// the left arrow and a decorative tick is as good as any. A key legend is
    /// the one place that reading is a lie, so `crate::widget::keys` names those
    /// keys in words instead of asking this table. This test holds the line
    /// between the two: the table stays as it is, and the legend stays out of it.
    #[test]
    fn a_key_glyph_is_left_to_the_legend_not_swapped_as_chrome() {
        // What the table does with `⏎`, stated as the reason the legend cannot
        // use it: a wrong-width swap is survivable, a wrong KEY is not.
        assert_eq!(downgrade("\u{23CE}", false), "<");
        // And what a legend therefore does instead.
        assert_eq!(
            crate::widget::keys(&[("\u{23CE}", "ok")], Caps::plain()),
            "enter ok",
            "the legend must name the key, not swap it for the left arrow"
        );
    }

    #[test]
    fn every_spinner_frame_is_one_column() {
        // The property the choice of braille was made for, checked rather than
        // asserted in prose: a frame of two cells would shift everything beside
        // the spinner on every tick, and a frame that renders as nothing would
        // make the line look like a stuck label.
        for frame in SPINNER {
            assert_eq!(
                crate::width::str_width(frame),
                1,
                "{frame:?} is not one cell"
            );
            assert_eq!(downgrade(frame, false), frame, "chrome is never rewritten");
        }
    }

    #[test]
    fn a_terminal_without_unicode_is_handed_a_spinner_it_can_draw() {
        // The braille set is deliberately absent from the downgrade table — it
        // trades one column for one column, and there is no one-cell ASCII
        // stand-in that reads as motion. So the *set* has to follow the
        // terminal, or the shield that exists to stop tofu draws eight of them,
        // on exactly the terminals it was written for (legacy Windows conhost,
        // `LANG=C` in a container).
        let plain = Caps::plain();
        for tick in 0..24 {
            let frame = plain.spinner(tick);
            assert!(
                frame.is_ascii(),
                "tick {tick} drew {frame:?} on a terminal with no unicode"
            );
            assert_eq!(
                crate::width::str_width(frame),
                1,
                "tick {tick} drew {frame:?}, which is not one cell"
            );
        }
        // Anything that is not an outright no keeps the good set. Stated as an
        // explicit capability rather than `Caps::detect()`: detection is the
        // environment's answer, and the test runner sets `TERM=dumb` to keep its
        // own output stable — asserting the runner's answer would be a test of
        // nextest, not of this table.
        let capable = Caps {
            unicode: true,
            ..Caps::default()
        };
        assert_eq!(
            capable.spinner(3),
            SPINNER[3],
            "a terminal that can draw braille did not get it"
        );
    }

    /// A Windows console with nothing set — the session Windows Terminal hosts
    /// when PowerShell was started first and handed over, which arrives without
    /// `WT_SESSION` — is coloured, as the classic screen colours it. Elsewhere
    /// an empty `TERM` still says nothing, and `NO_COLOR` wins everywhere.
    #[test]
    fn a_windows_console_that_names_no_terminal_is_still_coloured() {
        let with = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let bare = with(&[]);
        assert_eq!(colors_for(&bare, true), Colors::Ansi256);
        assert_eq!(colors_for(&bare, false), Colors::None);

        let wt = with(&[("WT_SESSION", "abc")]);
        assert_eq!(colors_for(&wt, true), Colors::Ansi256);

        let refused = with(&[("NO_COLOR", "1")]);
        assert_eq!(colors_for(&refused, true), Colors::None);

        let truecolor = with(&[("COLORTERM", "truecolor")]);
        assert_eq!(colors_for(&truecolor, true), Colors::True);

        let dumb = with(&[("TERM", "dumb")]);
        assert_eq!(colors_for(&dumb, true), Colors::None);
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

    /// A terminal that names itself no longer has to, to draw Unicode.
    ///
    /// The regression this pins: a Windows console that announced neither
    /// `WT_SESSION` nor `TERM_PROGRAM` used to be forced to ASCII `+--+`, which
    /// the classic screen drew as `┌──┐` — strictly worse. The new screen only
    /// runs on Windows once the console speaks ANSI (same guarantee `colors_for`
    /// leans on), so there is no Windows arm here; the decision is a pure
    /// function of the environment, reachable on any machine. The two explicit
    /// overrides still rule in both directions.
    #[test]
    fn a_console_that_names_nothing_still_draws_unicode() {
        let with = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        // The case that used to fall to ASCII: nothing announced at all.
        assert!(unicode_for(&with(&[])), "a bare console must draw Unicode");
        // The overrides, both directions, and the opt-in winning the tie.
        assert!(
            !unicode_for(&with(&[("ATOMCODE_ASCII", "1")])),
            "ASCII forced"
        );
        assert!(
            unicode_for(&with(&[("ATOMCODE_UNICODE", "1")])),
            "Unicode forced"
        );
        assert!(
            unicode_for(&with(&[("ATOMCODE_UNICODE", "1"), ("ATOMCODE_ASCII", "1")])),
            "ATOMCODE_UNICODE wins the tie, the way tuix's did"
        );
        assert!(
            unicode_for(&with(&[("ATOMCODE_ASCII", "0")])),
            "=0 is not a force"
        );
        // The real-bug-report cases are still ASCII.
        assert!(!unicode_for(&with(&[("TERM", "dumb")])), "dumb is ASCII");
        assert!(
            !unicode_for(&with(&[("LANG", "C")])),
            "POSIX locale is ASCII"
        );
        assert!(
            !unicode_for(&with(&[("LC_ALL", "POSIX")])),
            "LC_ALL POSIX is ASCII"
        );
    }
}
