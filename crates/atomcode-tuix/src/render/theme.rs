// crates/atomcode-tuix/src/render/theme.rs
use crossterm::style::Color;

use crate::terminal::TerminalCaps;

/// 24-bit RGB palette. Callers check TerminalCaps::colors before applying.
pub struct Palette;

impl Palette {
    // Palette: no blue, no yellow. Soft lavender brand, mint accent,
    // neutral grays for body text and borders, coral for warning.
    pub const BRAND: Color = Color::Rgb { r: 205, g: 175, b: 215 };
    pub const DIM_GRAY: Color = Color::Rgb { r: 105, g: 105, b: 115 };
    pub const ACCENT: Color = Color::Rgb { r: 165, g: 210, b: 180 };
    pub const ACCENT_DIM: Color = Color::Rgb { r: 90, g: 110, b: 100 };
    pub const SECONDARY: Color = Color::Rgb { r: 170, g: 170, b: 180 };
    pub const BORDER: Color = Color::Rgb { r: 130, g: 130, b: 140 };
    pub const WARNING: Color = Color::Rgb { r: 220, g: 140, b: 140 };
    pub const ERROR: Color = Color::Rgb { r: 220, g: 95, b: 95 };
    pub const GREEN: Color = Color::Rgb { r: 140, g: 200, b: 140 };
    pub const RED: Color = Color::Rgb { r: 220, g: 95, b: 95 };
    /// Soft teal used for inline code (was yellow).
    pub const CODE: Color = Color::Rgb { r: 175, g: 205, b: 190 };
    /// Pure bright white — used for tool names so they stand out.
    pub const WHITE: Color = Color::Rgb { r: 245, g: 245, b: 250 };
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
        Role::Accent => Palette::ACCENT,
        Role::AccentDim => Palette::ACCENT_DIM,
        Role::Secondary => Palette::SECONDARY,
        Role::Border => Palette::BORDER,
        Role::Warning => Palette::WARNING,
        Role::Error => Palette::ERROR,
        Role::Success => Palette::GREEN,
        Role::DiffAdd => Palette::GREEN,
        Role::DiffRemove => Palette::RED,
        Role::ToolName => Palette::WHITE,
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
}
