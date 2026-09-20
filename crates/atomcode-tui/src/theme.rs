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
    /// The band across the one row of a panel that is being pointed at.
    ///
    /// A second step from the same ground as `PanelBg`, not the panel's colours
    /// inverted: "here" is a lighter patch of the same surface. Inverting is
    /// what it used to be, and on a dark terminal that is a near-white bar
    /// across a near-black panel — the loudest thing on screen, and the only
    /// mark in this file that was a guess about how a terminal paints instead of
    /// a measurement against it.
    PanelSelBg,
    /// One line of a chart, by its position in the series.
    ///
    /// Data ink, and the only role here whose *meaning is difference*: nothing
    /// is being said about a series by giving it slot 14 rather than slot 11,
    /// except that it is not the other one. That is why it is a role with an
    /// index rather than six roles with names — a name would claim the colour
    /// meant something.
    ///
    /// Indexed into the same slots a scheme already has, so a chart reads in
    /// the person's own colours and follows them into light mode. The classic
    /// front end wrote the 256-colour indices straight into the escape
    /// (`[75, 214, 208, 154, 183, 81]`), which assumed a dark terminal and
    /// disagreed with this front end about every one of them.
    Series(u8),
}

/// How many lines a chart can tell apart before it starts round-tripping.
pub const SERIES: u8 = 6;

/// Every role, so a check can walk them instead of keeping a list in step.
pub const ROLES: [Role; 21] = [
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
    Role::PanelSelBg,
    Role::Series(0),
    Role::Series(1),
    Role::Series(2),
    Role::Series(3),
    Role::Series(4),
    Role::Series(5),
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
    /// What the terminal draws ordinary text in, if it said. Body text uses the
    /// terminal's own foreground and therefore needs no number; [`Role::Muted`]
    /// does, because "quieter than that" is a measurement against it rather
    /// than a slot a scheme author guessed at.
    fg: Option<Rgb>,
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
            fg: None,
            slots: [None; 16],
        }
    }

    pub fn with_background(mut self, rgb: Rgb) -> Self {
        self.bg = rgb;
        self.bg_measured = true;
        self
    }

    pub fn with_foreground(mut self, rgb: Rgb) -> Self {
        self.fg = Some(rgb);
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

    /// The terminal's own text colour, when it answered for it.
    pub fn foreground(&self) -> Option<Rgb> {
        self.fg
    }

    /// Whether the terminal itself said what this slot renders as.
    ///
    /// The distinction is the whole of [`Role::Muted`]'s bug report. A slot the
    /// terminal did not answer is xterm's value for a *different* terminal's
    /// scheme — plausible-looking, and the reason a status line measured at
    /// 4.7:1 arrived on screen nearer 3:1: the arithmetic was right about a
    /// colour nobody was rendering.
    pub fn answered(&self, n: u8) -> bool {
        self.slots.get(n as usize).is_some_and(Option::is_some)
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

    /// What the terminal renders for any index, above the scheme's own sixteen
    /// included.
    ///
    /// Slots 0..15 are the person's scheme and only the terminal can say what
    /// they are; 16 and up are the standard cube and ramp, which every
    /// 256-colour terminal renders identically. That asymmetry is the whole
    /// reason a computed surface can be placed at all on a terminal that never
    /// answered — see [`cube`].
    pub fn rendered(&self, n: u8) -> Rgb {
        if n < 16 {
            self.slot(n)
        } else {
            cube(n)
        }
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
                if winner.is_none_or(|(s, _)| step < s) {
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
/// The floors were raised in response to exactly the complaint "the palette is
/// correct and still hard to read": the resolver is designed to *stop at* the
/// floor — [`quietest`] picks the dimmest ink that qualifies, [`lift`] halts the
/// moment the ratio clears — so a floor set at the legal minimum produces a
/// palette that sits on the legal minimum everywhere, all at once. WCAG AA
/// (4.5) is a threshold below which text is *faulty*, not a target at which
/// text is comfortable; terminals render small monospace glyphs through video
/// compression and antialiasing that a printed page never meets, so the target
/// here is the AAA band (7.0) for anything carrying meaning — an error, a diff
/// half, the line that says which model is running.
///
/// - **7.0** — meaning-carrying ink: errors, diffs, warnings, success, the
///   brand and accent marks.
/// - **6.0** — [`Role::Muted`]. It must stay below the prose (its hierarchy
///   test asserts that against the loudest ink), and 7.0 on black leaves
///   almost no room between it and plain text; 6.0 is still a solid jump from
///   the 4.5 that read as washed out.
/// - **4.5** — [`Role::Mode`]. A badge sits on a filled ground it does not
///   control; AA there, bold weight does the rest.
/// - **3.0** — [`Role::Border`] alone. Lines are chrome, not prose: a border
///   that shouted would be a border competing with the words, and its
///   legibility is carried by shape (a rule, a box edge), not by colour.
fn floor(role: Role) -> f32 {
    match role {
        Role::Border => 3.0,
        // Chart ink: it has to read, but it is not prose and it is not asked to
        // carry a sentence. The same floor a mode badge gets.
        Role::Mode | Role::Series(_) => 4.5,
        Role::Muted => 6.0,
        _ => 7.0,
    }
}

/// How far [`Role::Muted`] is pulled from the terminal's own text colour toward
/// its background — the whole definition of the role, when the terminal has
/// told us both ends.
///
/// Two measured values and one ratio, instead of a slot number that means
/// "whatever this terminal calls bright black". On a scheme whose foreground
/// reads against its background, a third of the way is a grey that visibly
/// recedes and still lands around 6:1 on a dark ground and 7:1 on paper — the
/// range a person reads as "secondary text" without leaning in. `lift` raises it
/// further if the terminal's own foreground is itself dim, so the floor holds
/// whatever the scheme is.
const MUTED_REACH: f32 = 0.35;

/// Ink for metadata: the terminal's own text colour, moved toward its
/// background, held at the floor. `None` when nothing measured anchors the
/// colour at all.
///
/// The anchor is the terminal's own foreground when it answered for it. When it
/// did not — OSC 10 is not implemented everywhere, and answering for the
/// background is not a promise to answer for this — the *direction* is still a
/// measurement, and xterm's default foreground for that direction is the right
/// thing to start from. That is a milder assumption than the one this module
/// refuses elsewhere: it does not guess which way the terminal leans, it only
/// fills in the far end of a direction that was measured.
fn muted_ink(p: &Palette, need: f32) -> Option<Rgb> {
    let anchor = p.foreground().or_else(|| {
        p.background_measured().then(|| match p.theme() {
            Theme::Dark => XTERM[7],
            Theme::Light => XTERM[0],
        })
    })?;
    Some(lift(
        mix(anchor, p.background(), MUTED_REACH),
        p.background(),
        need,
    ))
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
        Role::Secondary | Role::ToolName | Role::PanelFg | Role::PanelBg | Role::PanelSelBg => &[],
        // Six hues a scheme is near certain to have set apart from each other,
        // bright first and the dim twin behind it. Round-tripped rather than
        // extended past six: a seventh line a reader cannot name is worse than
        // a seventh line that shares a colour with the first.
        Role::Series(n) => match n % SERIES {
            0 => &[14, 6],
            1 => &[11, 3],
            2 => &[13, 5],
            3 => &[10, 2],
            4 => &[12, 4],
            _ => &[9, 1],
        },
    }
}

/// A raised surface: the background moved a little away from itself, so a panel
/// reads as sitting on top rather than as a hole.
fn panel_bg(p: &Palette) -> Rgb {
    panel_ground(p, PANEL_REACH)
}

/// How far a raised surface is moved off the screen's own background.
const PANEL_REACH: f32 = 0.12;
/// How far the band on the pointed-at row is moved: the same direction again, so
/// it is the same surface one step brighter rather than a second colour. Small
/// on purpose — this marks a row, it does not shout about it, and the whole
/// complaint about the thing it replaces was that it shouted.
const PANEL_SEL_REACH: f32 = 0.24;

/// A panel surface `reach` of the way off the background, in the direction the
/// theme leans. Two of these are one surface and the row being pointed at on it.
fn panel_ground(p: &Palette, reach: f32) -> Rgb {
    let toward = match p.theme() {
        Theme::Dark => (255, 255, 255),
        Theme::Light => (0, 0, 0),
    };
    mix(p.background(), toward, reach)
}

/// The colour for a role, or `None` for "leave the terminal's own".
///
/// The single resolution point. Everything above states a role.
pub fn resolve(role: Role, caps: Caps) -> Option<Color> {
    let p = &caps.palette;
    match role {
        Role::Secondary | Role::ToolName => None,
        Role::PanelBg => Some(exact(panel_bg(p), caps.colors, p)),
        Role::PanelSelBg => Some(exact(panel_ground(p, PANEL_SEL_REACH), caps.colors, p)),
        Role::PanelFg => {
            // Read against the panel, not against the screen behind it.
            let on = panel_bg(p);
            let ink = if contrast((255, 255, 255), on) >= contrast((0, 0, 0), on) {
                (255, 255, 255)
            } else {
                (0, 0, 0)
            };
            Some(exact(ink, caps.colors, p))
        }
        Role::Muted => {
            let need = floor(role);
            // The scheme's own dim slot — but only a slot the terminal said it
            // renders as. An unanswered slot is xterm's value, i.e. a guess
            // about someone else's scheme, and for this role it is a *reliably*
            // wrong guess: slot 8's whole job in most schemes is to be the
            // dimmest grey there is. Believing it is how a status line that
            // measured 4.7:1 arrived on screen nearer 3:1. See
            // [`Palette::answered`].
            //
            // And it has to be the *quietest* slot that reads, not the first
            // that reads: "quietest" cannot be a fixed order, because slot 8 is
            // the dim one on a dark ground and one of the loud ones on a light
            // ground. Contrast knows which way round the scheme is; a list of
            // slot numbers does not.
            if let Some(n) = quietest(role, caps, true) {
                return Some(Color::Ansi(n));
            }
            // Otherwise the two measured ends: the terminal's own text colour,
            // moved toward its own background until the ink recedes. Truecolor
            // keeps the ratio that was just computed; an indexed terminal gets
            // the nearest slot, which is the honest answer when slots are the
            // only vocabulary it has.
            if let Some(ink) = muted_ink(p, need) {
                return Some(exact(ink, caps.colors, p));
            }
            // Nobody answered: synthesise from the standard candidate and stop
            // just above the floor. Taking an assumed slot here instead is what
            // made metadata black — the loudest ink there is — on a white
            // ground.
            synthesise(role, caps)
        }
        _ => from_slots(role, caps),
    }
}

/// The candidate slot for `role` that reads with the *least* contrast — the
/// quietest ink that still clears the floor.
///
/// `only_answered` restricts it to slots the terminal reported the colour of.
fn quietest(role: Role, caps: Caps, only_answered: bool) -> Option<u8> {
    let p = &caps.palette;
    let bg = p.background();
    let need = floor(role);
    candidates(role)
        .iter()
        .copied()
        .filter(|&n| (!only_answered || p.answered(n)) && contrast(p.slot(n), bg) >= need)
        .min_by(|&a, &b| {
            contrast(p.slot(a), bg)
                .partial_cmp(&contrast(p.slot(b), bg))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Keep the role's hue and move the lightness: the colour's own anchor is the
/// first candidate, which in a measured scheme is what the role *is*.
///
/// Used when nothing the scheme offers reads — including the case where nothing
/// was measured at all.
fn synthesise(role: Role, caps: Caps) -> Option<Color> {
    let p = &caps.palette;
    let truecolor = caps.colors == Colors::True;
    let need = floor(role);
    let bg = p.background();
    let slots = candidates(role);
    let mut base = slots.first().map(|&n| p.slot(n)).unwrap_or(bg);
    // A scheme whose slot for this role is essentially grey gives the
    // synthesiser no hue to preserve, and every role would come out the same
    // grey — an error indistinguishable from a heading. Fall back to the
    // standard hue for the slot, which at least keeps red red.
    if chroma(base) < 12 {
        if let Some(&n) = slots.first() {
            base = XTERM[n as usize];
        }
    }
    if truecolor {
        return Some(Color::rgb(lift(base, bg, need)));
    }
    // No truecolor to fall back on, so take the least bad slot rather than the
    // most preferred one.
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

/// The slot table's answer: the first candidate the user's own scheme defines
/// that measurably reads, or a synthesis from the best candidate there is.
///
/// This is what keeps the UI inside the colours the person chose, and it is the
/// right answer whenever the slots are real — either because the terminal
/// reported them, or because nothing at all was measured and the assumptions
/// then agree with each other.
fn from_slots(role: Role, caps: Caps) -> Option<Color> {
    let p = &caps.palette;
    let bg = p.background();
    let need = floor(role);
    // First choice: a slot the user's own scheme already defines, that
    // measurably reads. This is the case that keeps the UI inside their
    // colours instead of imposing ours.
    if let Some(&n) = candidates(role)
        .iter()
        .find(|&&n| contrast(p.slot(n), bg) >= need)
    {
        return Some(Color::Ansi(n));
    }
    synthesise(role, caps)
}

/// The colour xterm's index table renders for index `n`, 16 and up.
///
/// 16..231 is the 6×6×6 cube, 232..255 the 24-step grey ramp. Both are part of
/// the *standard*, not of anybody's scheme: unlike slots 0..15 — which a person
/// redefines in their profile, so assuming one is assuming someone else's
/// colours — every 256-colour terminal draws these the same. That is what makes
/// them usable on a terminal that answered nothing at all, which is the state
/// every Windows terminal is in (the palette query needs a tty descriptor;
/// see `surface::query_terminal`).
fn cube(n: u8) -> Rgb {
    match n {
        0..=15 => XTERM[n as usize],
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            let i = n as usize - 16;
            (LEVELS[i / 36 % 6], LEVELS[i / 6 % 6], LEVELS[i % 6])
        }
        // 8, 18, ... 238 — xterm's ramp starts at eight, not zero, so the first
        // step is a grey that is *not* the black it would otherwise equal.
        232..=255 => {
            let g = 8 + 10 * (n - 232);
            (g, g, g)
        }
    }
}

/// An exact colour when the terminal has one, and the nearest index when it does
/// not — a panel is a background, and a wrong background is worse than a plain
/// one.
///
/// The search runs over all 256 indices on a 256-colour terminal, not just the
/// scheme's sixteen. Sixteen was the bug: the surfaces this resolves are the
/// background moved a *little* (12% and 24% of the way to white), and on a black
/// ground the nearest of the sixteen to `#1f1f1f` is slot 0 — black — so the
/// raised panel came out identical to the screen and the pointed-at row
/// identical to the panel. The cube has an index for every hexstep and the ramp
/// steps by ten, so both land on something that is actually a step away. Ties go
/// to the lower index, so a slot the person's own scheme defines still wins
/// whenever it is exactly as close as a cube entry.
///
/// Sixteen colours have no such vocabulary — there, any answer that is not the
/// ground is also not a colour the person chose — so an [`Colors::Ansi16`]
/// terminal keeps the old behaviour and its panels stay flat. Inventing a slot
/// there is the guess this module refuses everywhere else.
fn exact(rgb: Rgb, colors: Colors, p: &Palette) -> Color {
    if colors == Colors::True {
        return Color::rgb(rgb);
    }
    let last = if colors == Colors::Ansi256 { 255 } else { 15 };
    let best = (0u8..=last)
        .min_by(|&a, &b| {
            distance(p.rendered(a), rgb)
                .partial_cmp(&distance(p.rendered(b), rgb))
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

/// An exact colour, or the nearest one this terminal can actually show.
///
/// **The only door an arbitrary RGB enters a frame by.** A bitmap of a photograph
/// or a rendered chart states colours nobody chose, so — unlike a role — they
/// cannot come from the palette; but they must still be *resolved against* it.
/// Two reasons, and the second is the one that has bitten:
///
/// 1. A 24-bit sequence on a terminal that only has 256 indices is a colour the
///    terminal has to guess at, and the guesses differ.
/// 2. `Color::Rgb` means "an exact colour, from a measured triple … only the
///    palette resolver produces these" (`frame.rs`). Code that builds one
///    directly has left the palette behind, and the layering gate exists to
///    notice exactly that.
pub fn exact_colour(rgb: Rgb, caps: Caps) -> Color {
    exact(rgb, caps.colors, &caps.palette)
}

/// A picture's own 256-index colour, as this terminal can draw it.
///
/// The second door, beside [`exact_colour`], for the other way a picture states
/// colour: baked pixel art says "index 202", not "rgb(255,95,0)". `atomcode-tuix`
///'s mascot is exactly that (`mascot_color`), and on the terminals tuix supports
/// this returns the same index, so the same bytes go out.
///
/// Narrower than the cube, the index is **resolved against the palette** rather
/// than passed through. That is the one place this is stricter than tuix, which
/// gates the art on "has colour at all" and would send `38;5;202` to a sixteen
/// colour terminal — a sequence such a terminal may map to anything. Resolving
/// costs nothing on the way in and keeps the promise the layering gate makes.
pub fn indexed_colour(index: u8, caps: Caps) -> Color {
    match caps.colors {
        // The picture asks for this exact index, and this terminal has the cube.
        Colors::True | Colors::Ansi256 => Color::Ansi(index),
        // No cube: the nearest index this terminal really has.
        _ => exact(cube(index), caps.colors, &caps.palette),
    }
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
            format!("slot {n:<3} {}", hex(caps.palette.rendered(n))),
            caps.palette.rendered(n),
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
            // `PanelBg` *is* the ground, so a contrast floor is not a question
            // it can be asked: reporting it against the screen it sits on is
            // how the one line about a raised surface read as a failure.
            if role == Role::PanelBg {
                return format!("  {:<11} {:<22} a raised surface", "PanelBg", what);
            }
            // Not a contrast question either: this role exists to be a *different*
            // patch of the same surface, and its worth is the step it makes.
            if role == Role::PanelSelBg {
                let step = rgb_of(resolve(Role::PanelBg, caps)).map(|(_, c)| c);
                let how = match step {
                    Some(step) => format!(" one step above {}", hex(step)),
                    None => String::new(),
                };
                return format!(
                    "  {:<11} {:<22} the pointed-at row on a panel{how}",
                    "PanelSelBg", what
                );
            }
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
            Color::Ansi(n) => Some(caps.palette.rendered(n)),
            Color::Role(_) => unreachable!("resolution does not produce a role"),
            Color::Picture(_) => unreachable!("resolution does not produce a picture colour"),
        }
    }

    #[test]
    fn metadata_stays_quieter_than_the_prose_it_annotates() {
        // The over-correction this guards against: `Muted` missing a floor by a
        // hair and falling through to near-white, which is legible and wrong —
        // the whole point of the role is that it recedes.
        //
        // "Quieter" means quieter than the strongest ink on that ground, which
        // is the prose. Not quieter than the accent: a role's contrast ratio is
        // a property of its hue as much as of its brightness, and cyan on white
        // is a low-contrast accent however it is drawn. Comparing the two made
        // this test read as a hierarchy rule while it was measuring a hue.
        for bg in [
            (0x1e, 0x1e, 0x1e),
            (0, 0, 0),
            (255, 255, 255),
            (0xfd, 0xf6, 0xe3),
        ] {
            let caps = caps_on(bg);
            let muted = seen(Role::Muted, caps).unwrap();
            let ratio = contrast(muted, bg);
            assert!(ratio >= 4.5, "unreadable at {ratio:.2} on {bg:?}");
            let loudest = contrast((255, 255, 255), bg).max(contrast((0, 0, 0), bg));
            assert!(
                ratio < loudest,
                "muted ({ratio:.2}) is as loud as plain text ({loudest:.2}) on {bg:?}"
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
                        if matches!(role, Role::PanelFg | Role::PanelBg | Role::PanelSelBg) {
                            continue; // surfaces and the ink on them, below
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
            let pointed = seen(Role::PanelSelBg, caps).unwrap();
            let ink = seen(Role::PanelFg, caps).unwrap();
            assert!(
                contrast(ink, panel) >= 4.5,
                "panel {ink:?} on {panel:?} over {bg:?} is {:.2}",
                contrast(ink, panel)
            );
            assert!(
                contrast(ink, pointed) >= 4.5,
                "the row being pointed at {ink:?} on {pointed:?} over {bg:?} is {:.2}",
                contrast(ink, pointed)
            );
            assert_ne!(panel, bg, "a raised panel has to be visible as one");
            assert_ne!(
                pointed, panel,
                "the pointed-at row has to be a step off the panel it is on"
            );
            assert!(
                contrast(pointed, bg) > contrast(panel, bg),
                "and the step has to be *away* from the ground: {pointed:?} over {bg:?}"
            );
        }
    }

    #[test]
    fn a_raised_panel_is_visible_without_truecolor_too() {
        // The Windows case, and every other terminal that will not report
        // itself: nothing measured and no truecolor, so `exact` has only the
        // index table to answer from. It searched the first sixteen and stopped
        // — and the panel it computed for a black ground, `#1f1f1f`, twelve
        // percent of the way to white, is nearest to slot 0, which *is* the
        // ground. The raised surface and the pointed-at row both sank into the
        // screen behind them, on the terminals least able to report it back.
        for theme in [Theme::Dark, Theme::Light] {
            let caps = Caps {
                palette: Palette::assumed(theme),
                colors: Colors::Ansi256,
                ..Caps::default()
            };
            let bg = caps.palette.background();
            let panel = seen(Role::PanelBg, caps).unwrap();
            let pointed = seen(Role::PanelSelBg, caps).unwrap();
            assert_ne!(panel, bg, "the panel is the ground it sits on ({theme:?})");
            assert_ne!(
                pointed, panel,
                "the pointed-at row is its panel ({theme:?})"
            );
            assert!(
                contrast(pointed, bg) > contrast(panel, bg),
                "the step has to be *away* from the ground: {panel:?} / {pointed:?} over {bg:?}"
            );
        }
    }

    #[test]
    fn a_pictures_own_index_survives_where_the_cube_exists_and_is_resolved_where_it_does_not() {
        // The mascot's orange, index 202, as tuix bakes it. On a terminal with the
        // cube the picture's index is passed through, so the bytes match tuix
        // exactly; on a narrower one it is resolved, because `38;5;202` to a
        // sixteen-colour terminal is a colour nobody chose.
        let cube = Caps {
            colors: Colors::Ansi256,
            ..Caps::default()
        };
        assert_eq!(indexed_colour(202, cube), Color::Ansi(202));
        assert_eq!(indexed_colour(166, cube), Color::Ansi(166));
        assert_eq!(indexed_colour(232, cube), Color::Ansi(232));

        let sixteen = Caps {
            colors: Colors::Ansi16,
            ..Caps::default()
        };
        match indexed_colour(202, sixteen) {
            Color::Ansi(n) => assert!(n <= 15, "slot {n} is not one of the sixteen"),
            other => panic!("expected a real slot, got {other:?}"),
        }

        // Truecolour keeps the index too: `Ansi` is a sequence every truecolour
        // terminal still understands, and it is what the art asked for.
        let true_ = Caps {
            colors: Colors::True,
            ..Caps::default()
        };
        assert_eq!(indexed_colour(202, true_), Color::Ansi(202));
    }

    #[test]
    fn a_sixteen_colour_terminal_is_not_sent_to_the_cube() {
        // The boundary of the fix above, and the reason `exact` takes a
        // `Colors` rather than a `bool`: the cube and the ramp are the one part
        // of the index table every 256-colour terminal draws the same, and a
        // terminal that has only sixteen of them does not draw them *at all* —
        // it maps the index back into its own sixteen, so a computed surface
        // would land on whatever approximation the terminal chose rather than
        // on the step we asked for. Sixteen colours therefore keep the flat
        // panel: no colour is better than a colour we cannot predict.
        let caps = Caps {
            palette: Palette::assumed(Theme::Dark),
            colors: Colors::Ansi16,
            ..Caps::default()
        };
        for role in [Role::PanelBg, Role::PanelSelBg] {
            let Some(Color::Ansi(n)) = resolve(role, caps) else {
                panic!("{role:?} did not resolve to an index");
            };
            assert!(
                n < 16,
                "{role:?} reached for index {n} on a 16-colour terminal"
            );
        }
    }

    #[test]
    fn the_users_own_scheme_is_preferred_over_a_colour_of_ours() {
        // Synthesis is the fallback, not the default: when the scheme's slot
        // measurably reads, an ordinary terminal lands on one of its own named
        // slots, so the UI sits inside the scheme the person chose.
        //
        // `Muted` is deliberately absent from this list — see the tests below.
        // So is `Error`: xterm's assumed red reads 5.25:1 on black, which
        // cannot clear the 7.0 a meaning-carrying colour now owes, and an
        // *assumed* slot is not a scheme the person chose anyway — overriding
        // it is what
        // `a_scheme_whose_slots_do_not_read_is_overridden_rather_than_obeyed`
        // already prescribes; the raised floor merely moved to where that
        // override starts to bite.
        for bg in [(0, 0, 0), (255, 255, 255)] {
            let caps = caps_on(bg);
            for role in [Role::Accent, Role::Border] {
                assert!(
                    matches!(resolve(role, caps), Some(Color::Ansi(_))),
                    "{role:?} on {bg:?} reached for a colour of its own"
                );
            }
        }
    }

    #[test]
    fn a_stock_dark_profile_carries_meaning_at_the_new_floors() {
        // The report this raise answers: nothing measured, xterm's own numbers
        // — a terminal nobody customised. Every meaning-carrying role must
        // land at its floor against black, not at the 4.5 that measured as
        // passing and still read as washed out.
        let caps = caps_on((0, 0, 0));
        for role in ROLES {
            if matches!(
                role,
                Role::Secondary | Role::ToolName | Role::PanelBg | Role::PanelSelBg
            ) {
                continue; // no ink of their own, or not a contrast question
            }
            let Some(c) = seen(role, caps) else { continue };
            let against = match role {
                // Panel ink reads against the panel, as on any ground.
                Role::PanelFg => seen(Role::PanelBg, caps).unwrap(),
                _ => (0, 0, 0),
            };
            let ratio = contrast(c, against);
            let need = floor(role);
            assert!(
                ratio >= need,
                "{role:?} on black is only {ratio:.2}:1 (floor {need:.1})"
            );
        }
    }

    /// `Caps` as Warp hands them over — the terminal that produced the bug
    /// report. It answers what its background is and nothing about its sixteen
    /// slots, so `--probe-terminal` printed `slots answered: 0/16` and
    /// `Muted ... slot 8 #7f7f7f contrast 4.68`. That colour was xterm's, not
    /// Warp's: the arithmetic was right about a colour nobody was rendering,
    /// and the status line arrived dimmer than the number said.
    fn as_warp_reports_itself() -> Caps {
        Caps {
            palette: Palette::assumed(Theme::Dark)
                .with_background((0x12, 0x12, 0x12))
                .with_foreground((0xe5, 0xe5, 0xe5)),
            colors: Colors::True,
            ..Caps::default()
        }
    }

    #[test]
    fn metadata_is_made_from_the_two_ends_the_terminal_did_answer_for() {
        let caps = as_warp_reports_itself();
        let bg = (0x12, 0x12, 0x12);
        let ink = (0xe5, 0xe5, 0xe5);
        let muted = seen(Role::Muted, caps).unwrap();
        let ratio = contrast(muted, bg);
        assert!(ratio >= 4.5, "metadata at {ratio:.2}:1 is the bug report");
        assert!(
            ratio >= 5.0,
            "at {ratio:.2}:1 it clears the floor and still reads as washed out"
        );
        assert!(
            ratio < contrast(ink, bg),
            "and it is quieter than the prose it annotates ({muted:?} vs {ink:?})"
        );
    }

    #[test]
    fn metadata_still_reads_when_only_the_background_was_answered() {
        // OSC 10 is not implemented everywhere. The *direction* is still a
        // measurement, so metadata must not fall back to an assumed slot 8 —
        // the whole failure being repaired.
        let caps = Caps {
            palette: Palette::assumed(Theme::Dark).with_background((0x12, 0x12, 0x12)),
            colors: Colors::True,
            ..Caps::default()
        };
        assert!(!matches!(resolve(Role::Muted, caps), Some(Color::Ansi(8))));
        let ratio = contrast(seen(Role::Muted, caps).unwrap(), (0x12, 0x12, 0x12));
        assert!(ratio >= 4.5, "only {ratio:.2}:1");
        // …and on paper, the other direction.
        let light = Caps {
            palette: Palette::assumed(Theme::Dark).with_background((0xff, 0xff, 0xff)),
            colors: Colors::True,
            ..Caps::default()
        };
        let ratio = contrast(seen(Role::Muted, light).unwrap(), (0xff, 0xff, 0xff));
        assert!(ratio >= 4.5, "only {ratio:.2}:1 on paper");
    }

    #[test]
    fn a_scheme_that_says_what_its_dim_colour_is_gets_to_use_it() {
        // The other half: when the terminal does answer for the slot and it
        // measurably reads, the person's own dim colour wins over our
        // arithmetic — the same rule as every other role.
        let p = Palette::assumed(Theme::Dark)
            .with_background((0x12, 0x12, 0x12))
            .with_slot(8, (0x9a, 0x9a, 0x9a));
        let caps = Caps {
            palette: p,
            colors: Colors::True,
            ..Caps::default()
        };
        assert_eq!(resolve(Role::Muted, caps), Some(Color::Ansi(8)));
    }

    #[test]
    fn the_quietest_slot_that_reads_wins_on_either_ground() {
        // "Quietest" cannot be a fixed order of slot numbers: slot 8 is the dim
        // one on black and one of the loud ones on white. Reading the candidate
        // list in order would put black — the loudest ink there is — on a white
        // terminal as "muted", which is the shape of the bug this module is a
        // reaction to.
        for bg in [(0, 0, 0), (255, 255, 255)] {
            let mut p = Palette::assumed(Theme::Dark).with_background(bg);
            // Both answered, and only one of them reads on each ground.
            for (n, rgb) in [(8u8, (0x8a, 0x8a, 0x8a)), (7, (0xe5, 0xe5, 0xe5))] {
                p = p.with_slot(n, rgb);
            }
            let caps = Caps {
                palette: p,
                colors: Colors::True,
                ..Caps::default()
            };
            let muted = seen(Role::Muted, caps).unwrap();
            let ratio = contrast(muted, bg);
            let loudest = contrast((255, 255, 255), bg).max(contrast((0, 0, 0), bg));
            assert!(ratio >= 4.5, "{muted:?} on {bg:?} is only {ratio:.2}:1");
            assert!(
                ratio < loudest,
                "{muted:?} on {bg:?} is {ratio:.2}:1 — as loud as plain text"
            );
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
