//! Colour by role — resolved against the colours the terminal actually renders.
//!
//! # Why this is not a table of colour numbers
//!
//! It was, twice, and it was unreadable both times: bright cyan headings on a
//! white terminal, then dark blue ones on a black terminal. Hand-picking an
//! ANSI slot per role per theme cannot work, for two reasons that no amount of
//! care removes:
//!
//! * **A slot is not a colour.** `38;5;14` means "whatever *this* terminal calls
//!   bright cyan", which is the point — the UI sits inside the user's own
//!   scheme rather than fighting it — but it also means the author cannot know
//!   whether it will read. A scheme where slot 14 is a muted teal on a charcoal
//!   background is a legitimate scheme, and a table cannot see it.
//! * **Light and dark is one bit for a continuous question.** Two terminals can
//!   both be "dark" and disagree by 40% of the luminance range, and any single
//!   detection signal (OSC 11, `COLORFGBG`, a config line) can be missing,
//!   stale, or wrong — at which point a table is confidently inverted.
//!
//! So nothing here picks a colour. A role names *candidates in order of
//! preference*, and the resolver keeps the first one that is measurably legible
//! against the measured background. Nothing qualifying is itself an answer: the
//! colour is then synthesised, by pushing the candidate away from the
//! background until it reads. The one-bit question disappears — there is no
//! light branch and no dark branch, only a contrast ratio.
//!
//! # Resolution happens exactly once
//!
//! A module writes `Role::Accent`, never a colour, and never a `Theme`. The
//! role travels inside the `Style` as [`Color::Role`] and is resolved in
//! [`crate::ansi::sgr`], the single place that holds the measured
//! [`crate::caps::Caps`]. That is what makes this enforceable rather than
//! conventional: a module *cannot* resolve against the wrong palette, because
//! it is never given one. (It used to be given one, and `content.rs` promptly
//! hard-coded `Theme::Dark` — which is how a whole transcript stayed dark on a
//! light screen.)
//!
//! **Some roles deliberately have no colour.** `Secondary` and `ToolName`
//! resolve to `None`, meaning "emit no SGR" — the terminal's own foreground,
//! which is legible by construction. Body text that insists on a colour is body
//! text that clashes with half the colour schemes in the world.

use crate::caps::{Caps, Colors};
use crate::frame::{Color, Style};

/// A colour as the terminal renders it.
pub type Rgb = (u8, u8, u8);

/// Which way the background leans. Derived from a measurement, never guessed,
/// and used only where a direction genuinely is the question — which way a
/// raised panel should move.
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

/// Every role, so a check can walk them instead of keeping a list in step.
pub const ROLES: [Role; 14] = [
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

/// xterm's sixteen, the fallback for a terminal that will not say what its own
/// are. Being wrong here costs contrast, not correctness: the resolver checks
/// whatever it is given, so a bad assumption is caught by the same arithmetic
/// that catches a bad scheme.
const XTERM: [Rgb; 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x00, 0x00),
    (0x00, 0xcd, 0x00),
    (0xcd, 0xcd, 0x00),
    (0x00, 0x00, 0xee),
    (0xcd, 0x00, 0xcd),
    (0x00, 0xcd, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x7f, 0x7f, 0x7f),
    (0xff, 0x00, 0x00),
    (0x00, 0xff, 0x00),
    (0xff, 0xff, 0x00),
    (0x5c, 0x5c, 0xff),
    (0xff, 0x00, 0xff),
    (0x00, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

/// What the terminal actually renders: its background, and its sixteen slots.
///
/// Every field is a measurement with a fallback, and [`Palette::measured`] says
/// how much of it is real — which is what `--probe-terminal` prints, because a
/// palette that silently fell back to assumptions is the failure worth seeing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    bg: Rgb,
    bg_measured: bool,
    slots: [Option<Rgb>; 16],
}

impl Default for Palette {
    fn default() -> Self {
        Self::assumed(Theme::Dark)
    }
}

impl Palette {
    /// Nothing measured: xterm's slots on a black or white ground.
    pub const fn assumed(theme: Theme) -> Self {
        Self {
            bg: match theme {
                Theme::Dark => (0, 0, 0),
                Theme::Light => (0xff, 0xff, 0xff),
            },
            bg_measured: false,
            slots: [None; 16],
        }
    }

    pub fn with_background(mut self, rgb: Rgb) -> Self {
        self.bg = rgb;
        self.bg_measured = true;
        self
    }

    pub fn with_slot(mut self, n: u8, rgb: Rgb) -> Self {
        if let Some(slot) = self.slots.get_mut(n as usize) {
            *slot = Some(rgb);
        }
        self
    }

    pub fn background(&self) -> Rgb {
        self.bg
    }

    /// Whether the background came from the terminal rather than an assumption.
    pub fn background_measured(&self) -> bool {
        self.bg_measured
    }

    /// How many of the sixteen the terminal reported.
    pub fn measured(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// What the terminal renders for a slot, or xterm's value for it.
    pub fn slot(&self, n: u8) -> Rgb {
        let i = (n as usize).min(15);
        self.slots[i].unwrap_or(XTERM[i])
    }

    /// Which way the background leans.
    pub fn theme(&self) -> Theme {
        if luminance(self.bg) > 0.18 {
            Theme::Light
        } else {
            Theme::Dark
        }
    }
}

// ---- the arithmetic -----------------------------------------------------

/// WCAG relative luminance, 0.0 (black) to 1.0 (white).
pub fn luminance((r, g, b): Rgb) -> f32 {
    fn linear(c: u8) -> f32 {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// WCAG contrast ratio, 1.0 (identical) to 21.0 (black on white).
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

fn mix((ar, ag, ab): Rgb, (br, bg_, bb): Rgb, t: f32) -> Rgb {
    let f = |a: u8, b: u8| {
        (a as f32 + (b as f32 - a as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (f(ar, br), f(ag, bg_), f(ab, bb))
}

/// Push `base` away from `bg` until it reads, keeping as much of it as possible.
///
/// Both directions are tried and the *smaller* move wins, so a colour stays
/// recognisably itself: on a charcoal background a warning brightens rather
/// than turning white, and on paper it darkens rather than turning black.
/// When neither direction reaches `need` — a mid-grey background, where nothing
/// can — the best available is returned rather than an error, because a
/// slightly-too-low contrast is a far better answer than no colour at all.
fn lift(base: Rgb, bg: Rgb, need: f32) -> Rgb {
    if contrast(base, bg) >= need {
        return base;
    }
    const STEPS: u8 = 24;
    let mut best = base;
    let mut best_ratio = contrast(base, bg);
    let mut winner: Option<(u8, Rgb)> = None;
    for toward in [(255, 255, 255), (0, 0, 0)] {
        for step in 1..=STEPS {
            let c = mix(base, toward, step as f32 / STEPS as f32);
            let ratio = contrast(c, bg);
            if ratio > best_ratio {
                best_ratio = ratio;
                best = c;
            }
            if ratio >= need {
                // The smaller move keeps more of the original hue.
                if winner.map_or(true, |(s, _)| step < s) {
                    winner = Some((step, c));
                }
                break;
            }
        }
    }
    winner.map(|(_, c)| c).unwrap_or(best)
}

/// The contrast a role must clear against the background.
///
/// 4.5 is WCAG AA for body text and applies to anything carrying meaning — an
/// error that cannot be read is worse than no error. Chrome sits at 3.0, AA for
/// large text: a border that shouted would be a border competing with the words.
///
/// `Muted` is the one role that wants *less* contrast, and its floor is what
/// stops the resolver from over-serving it: at 3.0 a dim grey (`#666` on `#1e1e1e`
/// is 2.97) just misses, the search falls through to near-white, and metadata
/// ends up louder than the prose it annotates. The candidates are ordered
/// quietest-first for the same reason.
fn floor(role: Role) -> f32 {
    match role {
        Role::Muted => 2.5,
        Role::Border | Role::Mode => 3.0,
        _ => 4.5,
    }
}

/// The slots a role will accept, best first. Meaning, not brightness — which
/// one survives is decided by measurement.
fn candidates(role: Role) -> &'static [u8] {
    match role {
        Role::Brand => &[13, 5],
        Role::Accent => &[14, 6, 12, 4],
        Role::Border => &[6, 14, 12, 4],
        Role::Muted => &[8, 7, 15, 0],
        Role::Warning => &[11, 3],
        Role::Error | Role::DiffRemove => &[9, 1],
        Role::Success | Role::DiffAdd => &[10, 2],
        Role::Mode => &[12, 4, 13, 5],
        Role::Secondary | Role::ToolName | Role::PanelFg | Role::PanelBg => &[],
    }
}

/// A raised surface: the background moved a little away from itself, so a panel
/// reads as sitting on top rather than as a hole.
fn panel_bg(p: &Palette) -> Rgb {
    let toward = match p.theme() {
        Theme::Dark => (255, 255, 255),
        Theme::Light => (0, 0, 0),
    };
    mix(p.background(), toward, 0.12)
}

/// The colour for a role, or `None` for "leave the terminal's own".
///
/// The single resolution point. Everything above states a role.
pub fn resolve(role: Role, caps: Caps) -> Option<Color> {
    let p = &caps.palette;
    let truecolor = caps.colors == Colors::True;
    match role {
        Role::Secondary | Role::ToolName => None,
        Role::PanelBg => Some(exact(panel_bg(p), truecolor, p)),
        Role::PanelFg => {
            // Read against the panel, not against the screen behind it.
            let on = panel_bg(p);
            let ink = if contrast((255, 255, 255), on) >= contrast((0, 0, 0), on) {
                (255, 255, 255)
            } else {
                (0, 0, 0)
            };
            Some(exact(ink, truecolor, p))
        }
        _ => {
            let need = floor(role);
            let bg = p.background();
            let slots = candidates(role);
            // First choice: a slot the user's own scheme already defines, that
            // measurably reads. This is the case that keeps the UI inside their
            // colours instead of imposing ours.
            if let Some(&n) = slots.iter().find(|&&n| contrast(p.slot(n), bg) >= need) {
                return Some(Color::Ansi(n));
            }
            let mut base = slots.first().map(|&n| p.slot(n)).unwrap_or(bg);
            // A scheme whose slot for this role is essentially grey gives the
            // synthesiser no hue to preserve, and every role would come out the
            // same grey — an error indistinguishable from a heading. Fall back
            // to the standard hue for the slot, which at least keeps red red.
            if chroma(base) < 12 {
                if let Some(&n) = slots.first() {
                    base = XTERM[n as usize];
                }
            }
            if truecolor {
                // Nothing in the scheme reads: keep the hue, move the lightness.
                return Some(Color::rgb(lift(base, bg, need)));
            }
            // No truecolor to fall back on, so take the least bad slot rather
            // than the most preferred one.
            let best = slots
                .iter()
                .copied()
                .max_by(|&a, &b| {
                    contrast(p.slot(a), bg)
                        .partial_cmp(&contrast(p.slot(b), bg))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(7);
            Some(Color::Ansi(best))
        }
    }
}

/// An exact colour when the terminal has one, and the nearest slot when it does
/// not — a panel is a background, and a wrong background is worse than a plain
/// one.
fn exact(rgb: Rgb, truecolor: bool, p: &Palette) -> Color {
    if truecolor {
        return Color::rgb(rgb);
    }
    let best = (0u8..16)
        .min_by(|&a, &b| {
            distance(p.slot(a), rgb)
                .partial_cmp(&distance(p.slot(b), rgb))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(7);
    Color::Ansi(best)
}

/// How much colour there is in a colour, as opposed to lightness.
fn chroma((r, g, b): Rgb) -> u8 {
    r.max(g).max(b) - r.min(g).min(b)
}

fn distance((ar, ag, ab): Rgb, (br, bg_, bb): Rgb) -> f32 {
    let d = |a: u8, b: u8| (a as f32 - b as f32).powi(2);
    d(ar, br) + d(ag, bg_) + d(ab, bb)
}

/// One line per role: what it resolved to on this terminal, and how far it
/// stands out from what it sits on.
///
/// Here rather than at the caller because taking a resolved colour apart means
/// naming `Color::Ansi` and `Color::Rgb`, and outside this file that is exactly
/// what the layering gate forbids — a diagnostic is not a reason to bypass the
/// palette. So the palette explains itself.
pub fn explain(caps: Caps) -> Vec<String> {
    let hex = |(r, g, b): Rgb| format!("#{r:02x}{g:02x}{b:02x}");
    let rgb_of = |c: Option<Color>| match c {
        Some(Color::Ansi(n)) => Some((
            format!("slot {n:<3} {}", hex(caps.palette.slot(n))),
            caps.palette.slot(n),
        )),
        Some(Color::Rgb(r, g, b)) => Some((format!("exact    {}", hex((r, g, b))), (r, g, b))),
        _ => None,
    };
    let panel = rgb_of(resolve(Role::PanelBg, caps)).map(|(_, c)| c);
    ROLES
        .iter()
        .map(|&role| {
            let Some((what, c)) = rgb_of(resolve(role, caps)) else {
                return format!(
                    "  {:<11} the terminal's own foreground",
                    format!("{role:?}")
                );
            };
            // A panel is read against the panel, not against the screen behind.
            let against = match role {
                Role::PanelFg => panel.unwrap_or_else(|| caps.palette.background()),
                _ => caps.palette.background(),
            };
            format!(
                "  {:<11} {:<22} contrast {:>5.2} (floor {:.1}) against {}",
                format!("{role:?}"),
                what,
                contrast(c, against),
                floor(role),
                hex(against)
            )
        })
        .collect()
}

/// A style carrying this role's foreground.
///
/// The role, not a colour: it is resolved once, at paint, against the palette
/// that was actually measured. No caller passes a theme, because no caller has
/// any business holding one.
pub fn fg(role: Role) -> Style {
    Style::new().fg(Color::Role(role))
}

/// A style carrying this role as a background.
pub fn bg(role: Role) -> Style {
    Style::new().bg(Color::Role(role))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_on(bg: Rgb) -> Caps {
        Caps {
            palette: Palette::assumed(Theme::Dark).with_background(bg),
            colors: Colors::True,
            ..Caps::default()
        }
    }

    /// The resolved colour as RGB, so a test can measure what a person sees.
    fn seen(role: Role, caps: Caps) -> Option<Rgb> {
        match resolve(role, caps)? {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            Color::Ansi(n) => Some(caps.palette.slot(n)),
            Color::Role(_) => unreachable!("resolution does not produce a role"),
        }
    }

    #[test]
    fn metadata_stays_quieter_than_the_prose_it_annotates() {
        // The over-correction this guards against: `Muted` missing a 3.0 floor
        // by 0.03 and falling through to near-white, which is legible and
        // wrong — the whole point of the role is that it recedes.
        for bg in [
            (0x1e, 0x1e, 0x1e),
            (0, 0, 0),
            (255, 255, 255),
            (0xfd, 0xf6, 0xe3),
        ] {
            let caps = caps_on(bg);
            let muted = seen(Role::Muted, caps).unwrap();
            let ratio = contrast(muted, bg);
            assert!(ratio >= 2.5, "unreadable at {ratio:.2} on {bg:?}");
            let loud = seen(Role::Accent, caps).unwrap();
            assert!(
                ratio < contrast(loud, bg),
                "muted ({ratio:.2}) is louder than the accent ({:.2}) on {bg:?}",
                contrast(loud, bg)
            );
        }
    }

    #[test]
    fn every_role_reads_against_any_background() {
        // The property the whole scheme exists for, checked over the colour
        // cube rather than over the two backgrounds someone happened to try.
        // Both shipped palettes failed this: the first was invisible on white,
        // the second on charcoal.
        let mut worst: Vec<String> = Vec::new();
        for r in (0..=255u8).step_by(51) {
            for g in (0..=255u8).step_by(51) {
                for b in (0..=255u8).step_by(51) {
                    let caps = caps_on((r, g, b));
                    for role in ROLES {
                        if matches!(role, Role::PanelFg | Role::PanelBg) {
                            continue; // measured against the panel, below
                        }
                        let Some(c) = seen(role, caps) else { continue };
                        let ratio = contrast(c, (r, g, b));
                        // A mid-grey background cannot reach 4.5 by any colour;
                        // the guarantee there is "the best there is".
                        let ceiling = contrast((255, 255, 255), (r, g, b))
                            .max(contrast((0, 0, 0), (r, g, b)));
                        let need = floor(role).min(ceiling - 0.01);
                        if ratio < need {
                            worst.push(format!(
                                "{role:?} on #{r:02x}{g:02x}{b:02x}: {ratio:.2} < {need:.2}"
                            ));
                        }
                    }
                }
            }
        }
        assert!(worst.is_empty(), "unreadable:\n{}", worst.join("\n"));
    }

    #[test]
    fn a_panel_reads_against_the_panel_not_the_screen_behind_it() {
        // The bug this replaces: `PanelFg` was black and `PanelBg` was a fixed
        // charcoal, so a light terminal got black text on near-black.
        for bg in [
            (0, 0, 0),
            (255, 255, 255),
            (0x1c, 0x1c, 0x1c),
            (0xfd, 0xf6, 0xe3),
        ] {
            let caps = caps_on(bg);
            let panel = seen(Role::PanelBg, caps).unwrap();
            let ink = seen(Role::PanelFg, caps).unwrap();
            assert!(
                contrast(ink, panel) >= 4.5,
                "panel {ink:?} on {panel:?} over {bg:?} is {:.2}",
                contrast(ink, panel)
            );
            assert_ne!(panel, bg, "a raised panel has to be visible as one");
        }
    }

    #[test]
    fn the_users_own_scheme_is_preferred_over_a_colour_of_ours() {
        // Synthesis is the fallback, not the default: on an ordinary terminal
        // every role should land on a named slot, so the UI sits inside the
        // scheme the user chose.
        for bg in [(0, 0, 0), (255, 255, 255)] {
            let caps = caps_on(bg);
            for role in [Role::Accent, Role::Border, Role::Error, Role::Muted] {
                assert!(
                    matches!(resolve(role, caps), Some(Color::Ansi(_))),
                    "{role:?} on {bg:?} reached for a colour of its own"
                );
            }
        }
    }

    #[test]
    fn a_scheme_whose_slots_do_not_read_is_overridden_rather_than_obeyed() {
        // The case a table of slot numbers cannot see: a legitimate low-contrast
        // scheme. Every candidate for `Accent` is near the background, so no
        // slot qualifies and the colour has to be made.
        let flat = (0x30, 0x30, 0x30);
        let mut p = Palette::assumed(Theme::Dark).with_background(flat);
        for n in candidates(Role::Accent) {
            p = p.with_slot(*n, (0x36, 0x38, 0x3a));
        }
        let caps = Caps {
            palette: p,
            colors: Colors::True,
            ..Caps::default()
        };
        let c = seen(Role::Accent, caps).unwrap();
        assert!(
            contrast(c, flat) >= 4.5,
            "{c:?} on {flat:?} is {:.2}",
            contrast(c, flat)
        );
    }

    #[test]
    fn a_greyscale_scheme_does_not_collapse_every_role_into_one_grey() {
        // The degenerate input: a scheme where the slot for each role is the
        // same near-grey. Synthesising from it would make an error and a
        // heading the same colour, which is legible and useless.
        let bg = (0x30, 0x30, 0x30);
        let mut p = Palette::assumed(Theme::Dark).with_background(bg);
        for n in 0..16u8 {
            p = p.with_slot(n, (0x36, 0x38, 0x3a));
        }
        let caps = Caps {
            palette: p,
            colors: Colors::True,
            ..Caps::default()
        };
        let hues: Vec<Rgb> = [Role::Accent, Role::Warning, Role::Error, Role::Success]
            .iter()
            .map(|&r| seen(r, caps).unwrap())
            .collect();
        for (i, a) in hues.iter().enumerate() {
            assert!(contrast(*a, bg) >= 4.5, "{a:?} is unreadable");
            for b in &hues[i + 1..] {
                assert_ne!(a, b, "two roles came out the same colour: {hues:?}");
            }
        }
    }

    #[test]
    fn body_text_keeps_the_terminals_own_foreground() {
        // Deliberate: prose that insists on a colour clashes with half the
        // colour schemes in the world, and the terminal's own foreground is
        // legible by construction.
        for bg in [(0, 0, 0), (255, 255, 255)] {
            assert_eq!(resolve(Role::Secondary, caps_on(bg)), None);
            assert_eq!(resolve(Role::ToolName, caps_on(bg)), None);
        }
        assert_eq!(
            fg(Role::Secondary),
            Style::new().fg(Color::Role(Role::Secondary))
        );
    }

    #[test]
    fn a_role_is_carried_not_resolved_by_whoever_asked_for_it() {
        // The structural half of the fix. A module that could resolve could
        // resolve against the wrong palette — which is exactly what happened
        // when `content.rs` was handed a `Theme` and hard-coded `Dark`.
        assert_eq!(fg(Role::Accent).fg, Some(Color::Role(Role::Accent)));
        assert_eq!(bg(Role::PanelBg).bg, Some(Color::Role(Role::PanelBg)));
    }

    #[test]
    fn without_truecolor_the_least_bad_slot_wins_not_the_most_preferred() {
        // A 16-colour terminal has nothing to synthesise with, so the ordering
        // has to change rather than the answer being wrong.
        let caps = Caps {
            palette: Palette::assumed(Theme::Light),
            colors: Colors::Ansi16,
            ..Caps::default()
        };
        match resolve(Role::Warning, caps) {
            // Not 11 (bright yellow), which is invisible on white.
            Some(Color::Ansi(n)) => assert_eq!(n, 3, "picked slot {n}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_background_decides_the_direction_and_nothing_else_does() {
        assert_eq!(Palette::assumed(Theme::Dark).theme(), Theme::Dark);
        assert_eq!(Palette::assumed(Theme::Light).theme(), Theme::Light);
        // Solarized, both ways round — the pair a luminance threshold has to
        // separate, and the pair a naive average gets wrong.
        let p = |rgb| Palette::assumed(Theme::Dark).with_background(rgb).theme();
        assert_eq!(p((0x00, 0x2b, 0x36)), Theme::Dark);
        assert_eq!(p((0xfd, 0xf6, 0xe3)), Theme::Light);
        assert_eq!(p((0x1c, 0x1c, 0x1c)), Theme::Dark);
    }

    #[test]
    fn contrast_is_the_wcag_ratio_and_not_something_that_looks_like_it() {
        // An independent oracle: the two ends of the scale are defined values.
        assert!((contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 0.01);
        assert!((contrast((0, 0, 0), (0, 0, 0)) - 1.0).abs() < 0.001);
        assert!(contrast((255, 255, 255), (255, 255, 255)) - 1.0 < 0.001);
    }
}
