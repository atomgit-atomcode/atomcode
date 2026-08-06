// crates/atomcode-tuix/src/render/theme.rs
use crossterm::style::Color;

use crate::highlight::theme as md_theme;
use crate::terminal::TerminalCaps;

/// Basic 16-color palette — SGR 30-37/90-97 only, no truecolor RGB.
///
/// **Why 16 colors:** truecolor RGB renders the same pixel regardless of
/// terminal theme. On Mac Terminal.app's default "Basic" (light) profile,
/// our old lavender/mint/grays landed on a light background and all but
/// disappeared. The 16-color SGR palette (30-37, 90-97) is interpreted by
/// the terminal's own theme engine — each user's colorscheme remaps the
/// same escape into theme-appropriate RGB, so atomcode adapts to whatever
/// terminal theme the user runs.
///
/// **Compatibility floor:** SGR 30-37/90-97 are part of the 1996 ECMA-48
/// baseline. Every modern terminal (macOS Terminal, iTerm2, Alacritty,
/// Kitty, Wezterm, Windows Terminal, Win10 1511+ cmd.exe with VT mode,
/// tmux, SSH-in-SSH) handles them identically. We specifically avoid
/// `\x1b[2m` (dim) which isn't reliable on Windows conhost < 1809.
pub struct Palette;

impl Palette {
    // Using the bright (9X) variants for signal colours rather than the
    // standard (3X) variants. On dark-theme terminals the standard set
    // (32/33/31/36) renders muddy — "dark green" looks olive-khaki,
    // "dark cyan" looks desaturated. CC uses bright variants for diff
    // +/- and inline code for the same reason; aligning here so colours
    // read consistently across Mac Terminal / iTerm / Alacritty dark
    // themes. Bright variants also still map to sensible colours on
    // light themes (most terminals give them enough contrast with the
    // default background).
    pub const BRAND: Color = Color::Magenta; // bright magenta (95)

    /// Colour for the footer **mode badge** (`⏵ accept edits`, `PLAN`).
    /// Deliberately a soft 256-colour periwinkle (`AnsiValue(104)` ≈ `#8787d7`)
    /// rather than the global `BRAND` magenta: the mode indicator reads as a
    /// distinct "current interaction mode" pill (mirrors Claude Code's purple)
    /// while tool markers / spinner / prompt glyph stay Brand. 256-colour (not
    /// truecolor) keeps parity with the `PanelBg` `AnsiValue` usage and stays
    /// portable across terminals.
    pub const MODE: Color = Color::AnsiValue(104);

    /// Shell-mode (`!`) accent — atomcode's brand **purple** (`#7c3aed` family),
    /// deliberately NOT the reddish global `BRAND` magenta. Terminal chrome has
    /// no 16-colour "purple" (SGR magenta is the red-leaning one), so we use
    /// 256-colour `AnsiValue` like `MODE`. Split light/dark because `AnsiValue`
    /// is fixed (doesn't track the terminal palette) — the periwinkle that pops
    /// on dark washes out on white.
    ///
    /// Dark → periwinkle `AnsiValue(104)` (≈`#8787d7`, same hue as the mode
    /// badge, so `!` shell mode reads as a sibling of `PLAN`/`auto`).
    pub const SHELL_DARK: Color = Color::AnsiValue(104);
    /// Light → deeper violet `AnsiValue(56)` (≈`#5f00d7`, close to the `#7c3aed`
    /// brand) so the border / hint keep contrast on a white background.
    pub const SHELL_LIGHT: Color = Color::AnsiValue(56);

    /// Muted text on **light** backgrounds. SGR 90 ("bright black") maps
    /// to a mid-gray on most light themes — contrast against `#FFFFFF`
    /// lands around 4.5–5:1, comfortably above AA.
    pub const MUTED_LIGHT: Color = Color::DarkGrey; // SGR 90

    /// Muted text on **dark** backgrounds. SGR 37 ("regular white") maps
    /// to a soft light-gray on dark themes — contrast against `#1B1B1B`
    /// to `#303030` lands around 8–10:1.
    ///
    /// Earlier this was `Color::DarkGrey` (SGR 90) for both modes on the
    /// theory that the terminal's palette would adapt. Reality from
    /// Warp / iTerm2 / Mac Terminal screenshots: most dark themes map
    /// SGR 90 to ~`#3F3F3F` (≈ 3:1 against the dark bg) — child rows
    /// under a tool-batch header rendered almost invisible. Splitting
    /// MUTED into light/dark variants and switching via
    /// `is_light_for_render` recovers readable contrast on both.
    pub const MUTED_DARK: Color = Color::Grey; // SGR 37

    /// Back-compat alias — same value as `MUTED_LIGHT` so old call sites
    /// that pre-date the dark-mode split keep compiling. New code should
    /// call [`muted_for_current_theme`] instead so the shade tracks the
    /// active palette.
    pub const MUTED: Color = Self::MUTED_LIGHT;

    pub const ACCENT: Color = Color::Cyan; // bright cyan (96)
    pub const BORDER: Color = Color::Cyan; // bright cyan (96) — 蓝绿色边框，和 Accent/prompt glyph 视觉呼应，对比度高于 DarkGrey 不易被背景吞掉
    /// Warning on **light** backgrounds. Bright yellow (SGR 93 / `Color::Yellow`)
    /// washes out to near-invisible on white; `DarkYellow` (SGR 33, an olive/gold)
    /// keeps ~AA contrast. Mirrors the MUTED light/dark split.
    pub const WARNING_LIGHT: Color = Color::DarkYellow; // SGR 33
    /// Warning on **dark** backgrounds — bright yellow pops.
    pub const WARNING_DARK: Color = Color::Yellow; // SGR 93
    /// Back-compat alias (same as the old unconditional value). New code should
    /// call [`warning_for_current_theme`] so the shade tracks the active palette.
    pub const WARNING: Color = Self::WARNING_DARK;
    pub const ERROR: Color = Color::Red; // bright red (91)
    pub const DIFF_ADD: Color = Color::Green; // bright green (92)
    pub const DIFF_REMOVE: Color = Color::Red; // bright red (91) — paired with Error
    /// Diff add on **light** backgrounds. `DarkGreen` (SGR 32) is more readable on white.
    pub const DIFF_ADD_LIGHT: Color = Color::DarkGreen; // SGR 32
    /// Diff add on **dark** backgrounds — keep bright green for contrast.
    pub const DIFF_ADD_DARK: Color = Color::Green; // SGR 92
    /// Diff remove on **light** backgrounds. `DarkRed` (SGR 31) is more readable on white.
    pub const DIFF_REMOVE_LIGHT: Color = Color::DarkRed; // SGR 31
    /// Diff remove on **dark** backgrounds — keep bright red for contrast.
    pub const DIFF_REMOVE_DARK: Color = Color::Red; // SGR 91
    pub const CODE: Color = Color::Cyan; // bright cyan (96)

    /// Foreground paired with [`Role::PanelBg`] on a light palette.
    /// User-message panels paint an explicit background, so inheriting the
    /// terminal's default foreground is unsafe when automatic theme detection
    /// is unavailable (notably Windows): a light terminal may otherwise put
    /// its dark default text on our dark fallback panel.
    pub const PANEL_FG_LIGHT: Color = Color::Black;
    /// Foreground paired with [`Role::PanelBg`] on a dark palette. SGR 37 is
    /// interpreted by the terminal palette as its readable light foreground.
    pub const PANEL_FG_DARK: Color = Color::Grey;

    /// Colour for the **Plan mode badge** (⏸ plan). Orange (`AnsiValue(208)` ≈
    /// `#ff8700`) — deliberately distinct from the periwinkle MODE color used by
    /// AcceptEdits, so the two non-default approval modes are visually separable.
    /// 256-colour (not truecolor) keeps parity with the MODE/SHELL usage and stays
    /// portable across terminals.
    pub const PLAN: Color = Color::AnsiValue(208);
}

/// Resolve the muted shade for the active palette.
///
/// Light theme → `MUTED_LIGHT` (SGR 90, dark gray on white).
/// Dark theme  → `MUTED_DARK`  (SGR 37, light gray on dark).
///
/// Routed through this fn rather than a `const` so role lookups
/// pick up live theme switches (auto-detect at startup + future
/// `/theme` slash command) without restart.
pub fn muted_for_current_theme() -> Color {
    if md_theme::is_light_for_render() {
        Palette::MUTED_LIGHT
    } else {
        Palette::MUTED_DARK
    }
}

/// Resolve the warning shade for the active palette — the `!` advisory line.
///
/// Light theme → `WARNING_LIGHT` (SGR 33 dark yellow, readable on white).
/// Dark theme  → `WARNING_DARK`  (SGR 93 bright yellow, pops on dark).
///
/// Bright yellow (the old unconditional value) is near-invisible on light
/// backgrounds; this split restores contrast, matching `muted_for_current_theme`.
pub fn warning_for_current_theme() -> Color {
    if md_theme::is_light_for_render() {
        Palette::WARNING_LIGHT
    } else {
        Palette::WARNING_DARK
    }
}

/// Resolve the shell-mode (`!`) accent for the active palette — atomcode's
/// brand purple, kept readable on both backgrounds.
///
/// Light theme → `SHELL_LIGHT` (deeper violet, contrast on white).
/// Dark theme  → `SHELL_DARK`  (periwinkle, pops on dark).
pub fn shell_for_current_theme() -> Color {
    if md_theme::is_light_for_render() {
        Palette::SHELL_LIGHT
    } else {
        Palette::SHELL_DARK
    }
}

/// Resolve the diff add shade for the active palette.
///
/// Light theme → `DIFF_ADD_LIGHT` (SGR 32 dark green, readable on white).
/// Dark theme  → `DIFF_ADD_DARK`  (SGR 92 bright green, pops on dark).
///
/// Bright green on light backgrounds lacks contrast; this split matches the
/// light/dark strategy used by `warning_for_current_theme` and `muted_for_current_theme`.
pub fn diff_add_for_current_theme() -> Color {
    if md_theme::is_light_for_render() {
        Palette::DIFF_ADD_LIGHT
    } else {
        Palette::DIFF_ADD_DARK
    }
}

/// Resolve the diff remove shade for the active palette.
///
/// Light theme → `DIFF_REMOVE_LIGHT` (SGR 31 dark red, readable on white).
/// Dark theme  → `DIFF_REMOVE_DARK`  (SGR 91 bright red, pops on dark).
///
/// Bright red on light backgrounds lacks contrast; this split matches the
/// light/dark strategy used by `warning_for_current_theme` and `muted_for_current_theme`.
pub fn diff_remove_for_current_theme() -> Color {
    if md_theme::is_light_for_render() {
        Palette::DIFF_REMOVE_LIGHT
    } else {
        Palette::DIFF_REMOVE_DARK
    }
}

/// Semantic colour role → concrete Color, honouring NO_COLOR etc.
/// Returns None when colours are disabled OR when the role intentionally
/// uses the terminal's default foreground (so strong/tool-name text just
/// gets SGR bold without a fixed colour).
pub fn role(caps: TerminalCaps, role: Role) -> Option<Color> {
    if !caps.colors {
        return None;
    }
    match role {
        Role::Brand => Some(Palette::BRAND),
        Role::Mode => Some(Palette::MODE),
        Role::Plan => Some(Palette::PLAN),
        Role::Shell => Some(shell_for_current_theme()),
        Role::Muted => Some(muted_for_current_theme()),
        Role::Accent => Some(Palette::ACCENT),
        Role::AccentDim => Some(muted_for_current_theme()),
        // Secondary = default terminal foreground. Using None means
        // "don't emit a colour SGR"; text shows in whatever colour the
        // terminal's theme chose for regular output.
        Role::Secondary => None,
        Role::Border => Some(Palette::BORDER),
        Role::Warning => Some(warning_for_current_theme()),
        Role::Error => Some(Palette::ERROR),
        Role::Success => Some(Palette::DIFF_ADD),
        Role::DiffAdd => Some(diff_add_for_current_theme()),
        Role::DiffRemove => Some(diff_remove_for_current_theme()),
        Role::ToolName => None,
        Role::PanelFg => {
            if md_theme::is_light_for_render() {
                Some(Palette::PANEL_FG_LIGHT)
            } else {
                Some(Palette::PANEL_FG_DARK)
            }
        }
        Role::PanelBg => {
            if md_theme::is_light_for_render() {
                Some(Color::AnsiValue(254)) // Light gray for light theme
            } else {
                Some(Color::AnsiValue(236)) // Sleek dark gray for dark theme
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Role {
    Brand,
    Mode,
    Plan,
    Shell,
    Muted,
    Accent,
    AccentDim,
    Secondary,
    Border,
    Warning,
    Error,
    Success,
    DiffAdd,
    DiffRemove,
    ToolName,
    PanelFg,
    PanelBg,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{EnvView, TerminalCaps};

    fn caps(colors: bool) -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: !colors,
            term: Some("xterm".to_string()),
            colorterm: Some("truecolor".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn role_returns_none_when_colors_disabled() {
        assert!(role(caps(false), Role::Brand).is_none());
    }

    #[test]
    fn role_returns_palette_when_colors_enabled() {
        assert_eq!(role(caps(true), Role::Brand), Some(Palette::BRAND));
    }

    #[test]
    fn muted_switches_with_theme() {
        let _theme = md_theme::test_lock();
        // Take the theme lock so we don't race other theme-switching
        // tests in the highlight module — `MODE` is a process-wide
        // AtomicU8 and parallel test runs would interleave reads.
        use crate::highlight::theme as md_theme;
        md_theme::set_theme_mode(false); // dark
        assert_eq!(
            role(caps(true), Role::Muted),
            Some(Palette::MUTED_DARK),
            "dark theme must use SGR 37 (regular white) for muted — \
             SGR 90 reads invisible on Warp / iTerm2 / Mac Terminal dark"
        );
        assert_eq!(
            role(caps(true), Role::AccentDim),
            Some(Palette::MUTED_DARK),
            "AccentDim must track the same muted shade as Role::Muted"
        );

        md_theme::set_theme_mode(true); // light
        assert_eq!(
            role(caps(true), Role::Muted),
            Some(Palette::MUTED_LIGHT),
            "light theme must use SGR 90 (bright black) — `white` would \
             be invisible against the white background"
        );
        assert_eq!(
            role(caps(true), Role::AccentDim),
            Some(Palette::MUTED_LIGHT)
        );

        // Restore default (dark) so subsequent tests see the legacy state.
        md_theme::set_theme_mode(false);
    }

    #[test]
    fn back_compat_muted_alias_is_light_variant() {
        // `Palette::MUTED` predates the dark-mode split. Pin that it
        // continues to mean the light-mode shade so any caller that
        // still references the bare constant doesn't silently break.
        assert_eq!(Palette::MUTED, Palette::MUTED_LIGHT);
    }

    #[test]
    fn warning_switches_with_theme() {
        let _theme = md_theme::test_lock();
        // The `!` advisory colour must track the palette: bright yellow (SGR 93)
        // is near-invisible on white, so light theme uses dark yellow (SGR 33).
        use crate::highlight::theme as md_theme;
        md_theme::set_theme_mode(false); // dark
        assert_eq!(role(caps(true), Role::Warning), Some(Palette::WARNING_DARK));
        md_theme::set_theme_mode(true); // light
        assert_eq!(
            role(caps(true), Role::Warning),
            Some(Palette::WARNING_LIGHT),
            "light theme must use SGR 33 (dark yellow) — bright yellow reads invisible on white"
        );
        // Distinct shades, else the split is pointless.
        assert_ne!(Palette::WARNING_LIGHT, Palette::WARNING_DARK);
        md_theme::set_theme_mode(false); // restore default
    }

    #[test]
    fn shell_mode_uses_brand_purple_and_switches_with_theme() {
        let _theme = md_theme::test_lock();
        // The `!` shell-mode accent is atomcode's brand PURPLE (#7c3aed family),
        // NOT the reddish `Brand` magenta. Because `AnsiValue` is fixed (doesn't
        // track the terminal palette), a light-theme-safe deeper purple is used
        // on light backgrounds and the periwinkle pops on dark.
        use crate::highlight::theme as md_theme;
        md_theme::set_theme_mode(false); // dark
        assert_eq!(role(caps(true), Role::Shell), Some(Palette::SHELL_DARK));
        md_theme::set_theme_mode(true); // light
        assert_eq!(
            role(caps(true), Role::Shell),
            Some(Palette::SHELL_LIGHT),
            "light theme must use the deeper purple — periwinkle washes out on white"
        );
        // Distinct shades, else the split is pointless; and never the red magenta.
        assert_ne!(Palette::SHELL_LIGHT, Palette::SHELL_DARK);
        assert_ne!(
            Palette::SHELL_DARK,
            Palette::BRAND,
            "shell must not be the red brand magenta"
        );
        md_theme::set_theme_mode(false); // restore default
    }

    #[test]
    fn secondary_and_toolname_return_none() {
        // These roles deliberately fall through to the terminal's default
        // foreground — they should return None even when colours are on.
        assert!(role(caps(true), Role::Secondary).is_none());
        assert!(role(caps(true), Role::ToolName).is_none());
    }

    #[test]
    fn panel_foreground_and_background_are_paired_for_both_themes() {
        let _theme = md_theme::test_lock();

        md_theme::set_theme_mode(false);
        assert_eq!(
            role(caps(true), Role::PanelFg),
            Some(Palette::PANEL_FG_DARK)
        );
        assert_eq!(role(caps(true), Role::PanelBg), Some(Color::AnsiValue(236)));

        md_theme::set_theme_mode(true);
        assert_eq!(
            role(caps(true), Role::PanelFg),
            Some(Palette::PANEL_FG_LIGHT)
        );
        assert_eq!(role(caps(true), Role::PanelBg), Some(Color::AnsiValue(254)));

        md_theme::set_theme_mode(false);
    }

    #[test]
    fn diff_colors_soften_on_light_theme() {
        let _theme = md_theme::test_lock();
        md_theme::set_theme_mode(true); // light
        assert_eq!(diff_add_for_current_theme(), Color::DarkGreen);
        assert_eq!(diff_remove_for_current_theme(), Color::DarkRed);
        md_theme::set_theme_mode(false); // dark
        assert_eq!(diff_add_for_current_theme(), Color::Green);
        assert_eq!(diff_remove_for_current_theme(), Color::Red);
    }
}
