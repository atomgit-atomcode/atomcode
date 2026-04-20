// crates/atomcode-tuix/src/render/theme.rs
use crossterm::style::Color;

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
    pub const BRAND: Color = Color::Magenta;      // bright magenta (95)
    pub const MUTED: Color = Color::DarkGrey;     // bright black / mid-gray (90)
    pub const ACCENT: Color = Color::Cyan;        // bright cyan (96)
    pub const BORDER: Color = Color::Cyan;        // bright cyan (96) — 蓝绿色边框，和 Accent/prompt glyph 视觉呼应，对比度高于 DarkGrey 不易被背景吞掉
    pub const WARNING: Color = Color::Yellow;     // bright yellow (93)
    pub const ERROR: Color = Color::Red;          // bright red (91)
    pub const DIFF_ADD: Color = Color::Green;     // bright green (92)
    pub const DIFF_REMOVE: Color = Color::Red;    // bright red (91) — paired with Error
    pub const CODE: Color = Color::Cyan;          // bright cyan (96)
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
        Role::Muted => Some(Palette::MUTED),
        Role::Accent => Some(Palette::ACCENT),
        Role::AccentDim => Some(Palette::MUTED),
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
    fn secondary_and_toolname_return_none() {
        // These roles deliberately fall through to the terminal's default
        // foreground — they should return None even when colours are on.
        assert!(role(caps(true), Role::Secondary).is_none());
        assert!(role(caps(true), Role::ToolName).is_none());
    }
}
