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
    pub const MUTED_DARK: Color = Color::White; // SGR 37

    /// Back-compat alias — same value as `MUTED_LIGHT` so old call sites
    /// that pre-date the dark-mode split keep compiling. New code should
    /// call [`muted_for_current_theme`] instead so the shade tracks the
    /// active palette.
    pub const MUTED: Color = Self::MUTED_LIGHT;

    pub const ACCENT: Color = Color::Cyan; // bright cyan (96)
    pub const BORDER: Color = Color::Cyan; // bright cyan (96) — 蓝绿色边框，和 Accent/prompt glyph 视觉呼应，对比度高于 DarkGrey 不易被背景吞掉
    pub const WARNING: Color = Color::Yellow; // bright yellow (93)
    pub const ERROR: Color = Color::Red; // bright red (91)
    pub const DIFF_ADD: Color = Color::Green; // bright green (92)
    pub const DIFF_REMOVE: Color = Color::Red; // bright red (91) — paired with Error
    pub const CODE: Color = Color::Cyan; // bright cyan (96)
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
        Role::Muted => Some(muted_for_current_theme()),
        Role::Accent => Some(Palette::ACCENT),
        Role::AccentDim => Some(muted_for_current_theme()),
        // Secondary = default terminal foreground. Using None means
        // "don't emit a colour SGR"; text shows in whatever colour the
        // terminal's theme chose for regular output.
        Role::Secondary => None,
        Role::Border => Some(Palette::BORDER),
        Role::Warning => Some(Palette::WARNING),
        Role::Error => Some(Palette::ERROR),
        Role::Success => Some(Palette::DIFF_ADD),
        Role::DiffAdd => Some(Palette::DIFF_ADD),
        Role::DiffRemove => Some(Palette::DIFF_REMOVE),
        // Tool names: emphasise with bold only; the caller adds `\x1b[1m`.
        // No colour means the name picks up the terminal's default fg,
        // which guarantees readability on any theme.
        Role::ToolName => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Role {
    Brand,
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
    fn secondary_and_toolname_return_none() {
        // These roles deliberately fall through to the terminal's default
        // foreground — they should return None even when colours are on.
        assert!(role(caps(true), Role::Secondary).is_none());
        assert!(role(caps(true), Role::ToolName).is_none());
    }
}
