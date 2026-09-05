//! Colour by role, not by number.
//!
//! Ported from `atomcode-tuix`'s `render/theme.rs`, values unchanged. Two front
//! ends that disagreed about what "muted" looks like would be two products, and
//! the palette there is not arbitrary — each colour carries a note about a real
//! terminal where the obvious choice washed out.
//!
//! Two things are load-bearing:
//!
//! **The colours are named ANSI slots (0–15), not RGB.** `38;5;14` is "whatever
//! this terminal calls bright cyan", so the UI sits inside the user's own theme
//! instead of fighting it. A hard-coded `#7dcfff` looks right on the machine it
//! was picked on and wrong everywhere else.
//!
//! **Some roles deliberately have no colour.** `Secondary` and `ToolName` return
//! `None`, meaning "emit no SGR" — the terminal's default foreground. That is a
//! choice, not an omission: body text that insists on a colour is body text that
//! clashes with half the world's colour schemes.
//!
//! Light and dark are not detected, they are configured (`ui.theme`), and the
//! value arrives through [`crate::caps::Caps`] like every other terminal fact —
//! never read from the environment inside `render`.

use crate::frame::{Color, Style};

/// Which palette the terminal's background calls for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// What a piece of text *is*, which is what decides its colour.
///
/// A module asks for `Role::Error`, never for red. That is what lets the
/// palette change in one place, and what `gates/tui-layers.sh` checks for: a
/// raw colour index above this layer is the palette having been bypassed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The product's own mark.
    Brand,
    /// Interactive emphasis: selections, the prompt, headings.
    Accent,
    /// Chrome around things: borders, rules.
    Border,
    /// Metadata subordinate to what it describes.
    Muted,
    /// Body text — the terminal's own foreground, on purpose.
    Secondary,
    Warning,
    Error,
    Success,
    DiffAdd,
    DiffRemove,
    /// A tool's name. Uncoloured: it sits in a line that already has a marker.
    ToolName,
    /// A mode badge.
    Mode,
    /// Foreground and background of a raised panel.
    PanelFg,
    PanelBg,
}

/// The palette, with tuix's reasoning attached where it matters.
mod palette {
    use crate::frame::Color::{self, Ansi};
    /// Bright magenta.
    pub const BRAND: Color = Ansi(13);
    /// Bright cyan — high enough contrast that a border is not eaten by the
    /// background, unlike dark grey.
    pub const ACCENT: Color = Ansi(14);
    pub const BORDER: Color = Ansi(14);
    /// SGR 90 on light backgrounds…
    pub const MUTED_LIGHT: Color = Ansi(8);
    /// …and SGR 37 on dark, where bright black is nearly invisible.
    pub const MUTED_DARK: Color = Ansi(7);
    /// Bright yellow washes out to nothing on white; olive reads.
    pub const WARNING_LIGHT: Color = Ansi(3);
    pub const WARNING_DARK: Color = Ansi(11);
    pub const ERROR: Color = Ansi(9);
    pub const DIFF_ADD_DARK: Color = Ansi(10);
    pub const DIFF_ADD_LIGHT: Color = Ansi(2);
    pub const DIFF_REMOVE_DARK: Color = Ansi(9);
    pub const DIFF_REMOVE_LIGHT: Color = Ansi(1);
    pub const MODE: Color = Ansi(104);
    pub const PANEL_BG: Color = Ansi(236);
    pub const PANEL_FG_LIGHT: Color = Ansi(0);
    /// The panel background is #303030; a dim grey on it is unreadable.
    pub const PANEL_FG_DARK: Color = Ansi(15);
}

/// The colour for a role, or `None` for "leave the terminal's own".
pub fn colour(role: Role, theme: Theme) -> Option<Color> {
    let light = theme == Theme::Light;
    Some(match role {
        Role::Brand => palette::BRAND,
        Role::Accent => palette::ACCENT,
        Role::Border => palette::BORDER,
        Role::Muted => {
            if light {
                palette::MUTED_LIGHT
            } else {
                palette::MUTED_DARK
            }
        }
        Role::Secondary | Role::ToolName => return None,
        Role::Warning => {
            if light {
                palette::WARNING_LIGHT
            } else {
                palette::WARNING_DARK
            }
        }
        Role::Error => palette::ERROR,
        Role::Success | Role::DiffAdd => {
            if light {
                palette::DIFF_ADD_LIGHT
            } else {
                palette::DIFF_ADD_DARK
            }
        }
        Role::DiffRemove => {
            if light {
                palette::DIFF_REMOVE_LIGHT
            } else {
                palette::DIFF_REMOVE_DARK
            }
        }
        Role::Mode => palette::MODE,
        Role::PanelFg => {
            if light {
                palette::PANEL_FG_LIGHT
            } else {
                palette::PANEL_FG_DARK
            }
        }
        Role::PanelBg => palette::PANEL_BG,
    })
}

/// A style carrying this role's foreground.
pub fn fg(role: Role, theme: Theme) -> Style {
    match colour(role, theme) {
        Some(c) => Style::new().fg(c),
        None => Style::new(),
    }
}

/// A style carrying this role as a background.
pub fn bg(role: Role, theme: Theme) -> Style {
    match colour(role, theme) {
        Some(c) => Style::new().bg(c),
        None => Style::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Role; 14] = [
        Role::Brand,
        Role::Accent,
        Role::Border,
        Role::Muted,
        Role::Secondary,
        Role::Warning,
        Role::Error,
        Role::Success,
        Role::DiffAdd,
        Role::DiffRemove,
        Role::ToolName,
        Role::Mode,
        Role::PanelFg,
        Role::PanelBg,
    ];

    #[test]
    fn every_role_answers_in_both_themes() {
        // A role that panicked or was forgotten in one theme would show up as
        // "the light theme is broken", reported by whoever uses it least.
        for role in ALL {
            for theme in [Theme::Dark, Theme::Light] {
                let _ = colour(role, theme);
                let _ = fg(role, theme);
            }
        }
    }

    #[test]
    fn body_text_keeps_the_terminals_own_foreground() {
        // Deliberate: prose that insists on a colour clashes with half the
        // colour schemes in the world.
        for theme in [Theme::Dark, Theme::Light] {
            assert_eq!(colour(Role::Secondary, theme), None);
            assert_eq!(colour(Role::ToolName, theme), None);
            assert_eq!(fg(Role::Secondary, theme), Style::new());
        }
    }

    #[test]
    fn the_roles_that_differ_by_theme_actually_differ() {
        // Otherwise the light palette is decoration: present, and doing nothing.
        for role in [Role::Muted, Role::Warning, Role::DiffAdd, Role::DiffRemove] {
            assert_ne!(
                colour(role, Theme::Dark),
                colour(role, Theme::Light),
                "{role:?} is the same in both themes — then why is it listed?"
            );
        }
    }

    #[test]
    fn signal_colours_are_named_slots_not_fixed_pixels() {
        // 0..=15 are the terminal's own palette, so the UI sits inside the
        // user's colour scheme instead of fighting it.
        for role in [Role::Brand, Role::Accent, Role::Border, Role::Error] {
            match colour(role, Theme::Dark) {
                Some(crate::frame::Color::Ansi(n)) => {
                    assert!(n < 16, "{role:?} uses index {n}, outside the themed range")
                }
                other => panic!("{role:?} is {other:?}"),
            }
        }
    }
}
