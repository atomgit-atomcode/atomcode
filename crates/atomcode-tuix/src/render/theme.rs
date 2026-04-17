// crates/atomcode-tuix/src/render/theme.rs
use crossterm::style::Color;

use crate::terminal::TerminalCaps;

/// 24-bit RGB palette. Callers check TerminalCaps::colors before applying.
pub struct Palette;

impl Palette {
    pub const BRAND: Color = Color::Rgb { r: 140, g: 175, b: 230 };
    pub const DIM_GRAY: Color = Color::Rgb { r: 85, g: 88, b: 100 };
    pub const BLUE: Color = Color::Rgb { r: 100, g: 160, b: 240 };
    pub const ACCENT_DIM: Color = Color::Rgb { r: 40, g: 55, b: 80 };
    pub const SECONDARY: Color = Color::Rgb { r: 140, g: 142, b: 155 };
    /// Bright-but-muted border colour for boxes (welcome, spinner, input).
    /// Visible on dark terminals without screaming.
    pub const BORDER: Color = Color::Rgb { r: 110, g: 140, b: 200 };
    pub const WARNING: Color = Color::Rgb { r: 195, g: 155, b: 45 };
    pub const ERROR: Color = Color::Rgb { r: 210, g: 75, b: 75 };
    pub const GREEN: Color = Color::Rgb { r: 80, g: 200, b: 120 };
    pub const RED: Color = Color::Rgb { r: 210, g: 75, b: 75 };
}

/// Semantic colour role → concrete Color, honouring NO_COLOR etc.
/// Returns None when colours are disabled.
pub fn role(caps: TerminalCaps, role: Role) -> Option<Color> {
    if !caps.colors {
        return None;
    }
    Some(match role {
        Role::Brand => Palette::BRAND,
        Role::Muted => Palette::DIM_GRAY,
        Role::Accent => Palette::BLUE,
        Role::AccentDim => Palette::ACCENT_DIM,
        Role::Secondary => Palette::SECONDARY,
        Role::Border => Palette::BORDER,
        Role::Warning => Palette::WARNING,
        Role::Error => Palette::ERROR,
        Role::Success => Palette::GREEN,
        Role::DiffAdd => Palette::GREEN,
        Role::DiffRemove => Palette::RED,
    })
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
}
