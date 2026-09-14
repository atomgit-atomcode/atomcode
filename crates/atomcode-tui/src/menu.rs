//! The composer's context menu: the things you can do to what is being typed.
//!
//! A panel that rises *out of* the prompt rather than a list appended under it.
//! A discovery surface that pushes what it is about off the screen while it is
//! open is worse than no discovery surface, so this one is drawn over the layout
//! and moves nothing — the same rule the slash menu follows. The difference is
//! that this one is anchored to the cell the pointer was on rather than to the
//! field's edge, because it is about a place, not about the whole composer.
//!
//! The state is two numbers and a cursor. Everything else — what the items are,
//! what picking one means — belongs to the row that owns the composer, and is
//! kept out of here on purpose: this module knows how to lay a menu out, not
//! what a person wants in it.

use crate::frame::{Line, Rect, Span, Style};
use crate::moment::Viewport;
use crate::surface::{Key, KeyPress};
use crate::theme::{self, Role};
use crate::width;

/// One thing the menu offers.
///
/// `value` is what the caller gets back when it is picked, and is deliberately
/// opaque here: the menu must not know that `copy` means "put the composer on
/// the clipboard", or adding a fifth item would mean editing the thing that
/// draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub value: String,
    pub label: String,
    /// Shown dimmed after the label. Empty for an item that needs no gloss.
    pub about: String,
}

impl Item {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            about: String::new(),
        }
    }
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = about.into();
        self
    }
}

/// What a key did to the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Still open.
    Stay,
    /// Closed, and the item under the cursor was picked. The value is the
    /// item's own — this is the only way a choice leaves this module.
    Picked(String),
    /// Closed with nothing. Esc, or a click outside.
    Dismissed,
}

/// An open menu: where it is, and what is in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Menu {
    /// The anchor line: the screen row the pointed-at item is drawn on.
    ///
    /// The cell the secondary button went down on when the menu opened, and the
    /// row the pointer is on after that: hovering re-anchors it, because that is
    /// what keeps the panel from sliding out from under a moving pointer. See
    /// [`Menu::hover`].
    pub at: (u16, u16),
    pub items: Vec<Item>,
    cursor: usize,
}

/// Cells of runway to the right of the words, so the panel does not look
/// pinched against its own text.
const TRAILING: usize = 2;

impl Menu {
    pub fn new(at: (u16, u16), items: Vec<Item>) -> Option<Self> {
        (!items.is_empty()).then_some(Self {
            at,
            items,
            cursor: 0,
        })
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The rect the menu occupies on a screen this wide and tall.
    ///
    /// Sat so the item under the pointer is under the pointer: the top is
    /// `at.y - cursor`, and the whole thing is slid back on screen rather than
    /// clipped, because a menu half off the edge is a menu whose last item
    /// cannot be reached.
    pub fn rect(&self, screen_w: u16, screen_h: u16) -> Rect {
        let h = (self.items.len() as u16).min(screen_h);
        if h == 0 {
            return Rect::new(0, 0, 0, 0);
        }
        let want = self
            .items
            .iter()
            .map(|i| self.line_width(i))
            .max()
            .unwrap_or(0) as u16;
        let w = want.min(screen_w);
        let y = self
            .at
            .1
            .saturating_sub(self.cursor as u16)
            .min(screen_h.saturating_sub(h));
        let x = self.at.0.min(screen_w.saturating_sub(w));
        Rect::new(x, y, w, h)
    }

    /// How wide one item's row is, in cells.
    fn line_width(&self, item: &Item) -> usize {
        let about = if item.about.is_empty() {
            0
        } else {
            // The gap and the gloss after the label.
            2 + width::str_width(&item.about)
        };
        2 + width::str_width(&item.label) + about + TRAILING
    }

    /// Take a key. Up/down move, enter picks, esc closes — the same vocabulary
    /// every list in this UI speaks.
    pub fn key(&mut self, press: KeyPress) -> Step {
        match (press.key, press.mods) {
            (Key::Up, _) => {
                // Clamped, not wrapped: wrapping past the top of a three-item
                // menu reads as a jump, and there is nowhere to fall off to.
                self.cursor = self.cursor.saturating_sub(1);
                Step::Stay
            }
            (Key::Down, _) => {
                self.cursor = (self.cursor + 1).min(self.items.len().saturating_sub(1));
                Step::Stay
            }
            (Key::Enter, _) => Step::Picked(self.items[self.cursor].value.clone()),
            (Key::Esc, _) => Step::Dismissed,
            _ => Step::Stay,
        }
    }

    /// A click landed somewhere on the menu, if it landed on the menu at all.
    ///
    /// `None` means the click was outside: the caller decides whether that
    /// closes it, because only the caller knows whether the click was meant for
    /// something else on screen.
    ///
    /// Which row was hit is read off the rect the menu is *drawn* in, not off
    /// the cell it was opened at. The two differ whenever the menu had to slide
    /// to fit — near the bottom edge, which is exactly where a composer's menu
    /// opens — and a second formula here is a whole menu of rows that answer to
    /// the wrong one. Whatever [`Menu::rect`] says is the layout, so it is the
    /// only walk this can take.
    pub fn click(&mut self, x: u16, y: u16, screen_w: u16, screen_h: u16) -> Option<Step> {
        let rect = self.rect(screen_w, screen_h);
        if !rect.contains(x, y) {
            return None;
        }
        let row = (y - rect.y) as usize;
        if row >= self.items.len() {
            return None;
        }
        self.cursor = row;
        Some(Step::Picked(self.items[row].value.clone()))
    }

    /// The pointer moved over the menu: point at the row it is over.
    ///
    /// Returns whether a frame is owed — `false` both when the pointer is not on
    /// the menu at all and when it is on the row already pointed at. A move
    /// inside one row is not news, and the terminal sends a move for every cell
    /// the pointer crosses, so the cheap answer is the one that matters here.
    ///
    /// A pointer beside the menu is not pointing at a row, it is pointing at
    /// whatever the menu covers — so the panel keeps the highlight it had rather
    /// than clearing it. The highlight is where a choice would land, and it
    /// should not blink out from under a pointer that has stepped off the edge.
    pub fn hover(&mut self, x: u16, y: u16, screen_w: u16, screen_h: u16) -> bool {
        let rect = self.rect(screen_w, screen_h);
        if !rect.contains(x, y) {
            return false;
        }
        let row = (y - rect.y) as usize;
        if row >= self.items.len() || row == self.cursor {
            return false;
        }
        self.cursor = row;
        // Re-anchor to the row the pointer is on, without which the whole panel
        // would slide: `rect` is read off `at` and `cursor` together
        // (`y = at.y - cursor`), so moving the cursor alone moves the menu by
        // exactly that many rows and the row under the pointer changes again on
        // the next frame. Holding `at.y - cursor` still is what makes the panel
        // something the pointer travels *over* rather than something it pushes.
        self.at.1 = y;
        true
    }

    /// The menu's rows, top to bottom, filling the rect.
    ///
    /// Every row is filled to the rect with the panel's own background, so what
    /// it covers is covered rather than showing through: a menu of bare words
    /// floating over the conversation is the same list with none of the
    /// authority.
    pub fn render(&self, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let panel = theme::bg(Role::PanelBg).under(theme::fg(Role::PanelFg));
        let mut out: Vec<Line> = Vec::with_capacity(vp.rect.h as usize);
        for (i, item) in self.items.iter().enumerate().take(vp.rect.h as usize) {
            let here = i == self.cursor;
            // The pointed-at row is the same panel one step brighter. Reverse is
            // what it used to be, and reverse is not a colour: the terminal
            // decides what it means, and on a dark terminal it is a near-white
            // bar across a near-black panel — the loudest thing on the screen,
            // for a row that is only being pointed at.
            let base = if here {
                theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
            } else {
                panel
            };
            let mut spans = vec![Span::styled("  ", base)];
            spans.push(Span::styled(item.label.clone(), base));
            if !item.about.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", item.about),
                    if here {
                        base
                    } else {
                        theme::fg(Role::Muted).under(panel)
                    },
                ));
            }
            out.push(pad(Line::from_spans(spans), w, base));
        }
        out
    }
}

/// A line widened to the rect with the panel's own style.
///
/// The same rule as the slash menu's: a floating part covers what it is drawn
/// over only where it puts a cell down, and a span ends where its text ends.
/// Filling the rest of the row is what makes it a surface rather than words
/// with the conversation visible around them.
fn pad(line: Line, w: usize, style: Style) -> Line {
    let used = line.width();
    if used >= w {
        return line.truncate(w);
    }
    let mut spans = line.spans;
    spans.push(Span::styled(" ".repeat(w - used), style));
    Line::from_spans(spans).truncate(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moment::Moment;

    fn items() -> Vec<Item> {
        vec![
            Item::new("copy", "复制全文"),
            Item::new("paste", "粘贴"),
            Item::new("clear", "清空"),
            Item::new("send", "发送"),
        ]
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn drawn(menu: &Menu, w: u16, h: u16) -> Vec<String> {
        let m = Moment::default();
        let rect = menu.rect(w, h);
        let vp = Viewport::new(rect, &m);
        menu.render(&vp).iter().map(|l| l.plain()).collect()
    }

    #[test]
    fn an_empty_menu_is_not_opened_at_all() {
        assert!(Menu::new((3, 3), Vec::new()).is_none());
    }

    #[test]
    fn the_item_the_pointer_was_on_is_the_item_under_the_pointer() {
        // The whole reason the rect is bottom-anchored to the cursor: a menu
        // that opens with its first item under the pointer makes the pointer
        // look like it is choosing something it is not.
        let menu = Menu::new((10, 10), items()).unwrap();
        let rect = menu.rect(80, 24);
        assert_eq!(
            rect.y, 10,
            "the first item sits on the cell that was clicked"
        );
        assert_eq!(
            menu.rect(80, 24).x,
            10,
            "and the menu starts at the cell that was clicked"
        );
        assert_eq!(rect.h, 4);
    }

    #[test]
    fn the_menu_is_slid_back_on_screen_rather_than_clipped() {
        // Near the right edge and the bottom: the whole menu has to move, not
        // lose its last item off the screen.
        let menu = Menu::new((79, 23), items()).unwrap();
        let rect = menu.rect(80, 24);
        assert!(rect.right() <= 80, "runs off the right edge: {rect:?}");
        assert!(rect.bottom() <= 24, "runs off the bottom edge: {rect:?}");
        assert_eq!(rect.h, 4, "the menu kept all of its items");
    }

    #[test]
    fn moving_down_and_picking_returns_the_item_that_was_under_the_cursor() {
        let mut menu = Menu::new((0, 0), items()).unwrap();
        assert_eq!(menu.key(press(Key::Down)), Step::Stay);
        assert_eq!(menu.key(press(Key::Down)), Step::Stay);
        assert_eq!(menu.cursor(), 2);
        assert_eq!(menu.key(press(Key::Enter)), Step::Picked("clear".into()));
    }

    #[test]
    fn the_cursor_does_not_fall_off_either_end() {
        let mut menu = Menu::new((0, 0), items()).unwrap();
        menu.key(press(Key::Up));
        assert_eq!(menu.cursor(), 0, "up from the top stays at the top");
        for _ in 0..10 {
            menu.key(press(Key::Down));
        }
        assert_eq!(menu.cursor(), 3, "down past the end stops at the end");
    }

    #[test]
    fn escape_closes_it_with_nothing_chosen() {
        let mut menu = Menu::new((0, 0), items()).unwrap();
        assert_eq!(menu.key(press(Key::Esc)), Step::Dismissed);
    }

    #[test]
    fn a_click_on_a_row_picks_that_row() {
        let mut menu = Menu::new((0, 0), items()).unwrap();
        let rect = menu.rect(80, 24);
        assert_eq!(
            menu.click(rect.x, rect.y + 2, 80, 24),
            Some(Step::Picked("clear".into()))
        );
    }

    #[test]
    fn a_click_outside_the_menu_is_not_its_business() {
        let mut menu = Menu::new((10, 10), items()).unwrap();
        assert_eq!(menu.click(0, 0, 80, 24), None);
    }

    #[test]
    fn a_click_picks_the_words_that_were_drawn_where_it_landed() {
        // The case the geometry has to survive: opened at the bottom of the
        // screen, the menu slides up, and the row the words were drawn on is
        // the row a click on those words has to choose. A click that reads the
        // menu's position off the pointer instead of off the rect picks a row
        // nobody aimed at — at this size, every row of it.
        let items = items();
        let menu = Menu::new((10, 23), items.clone()).unwrap();
        let rect = menu.rect(80, 24);
        assert!(
            rect.y < 23,
            "this menu had to slide to fit, or the case is not under test"
        );
        for (i, want) in ["copy", "paste", "clear", "send"].iter().enumerate() {
            let mut menu = menu.clone();
            assert_eq!(
                menu.click(rect.x, rect.y + i as u16, 80, 24),
                Some(Step::Picked((*want).into())),
                "row {i} of the menu is not the row that was clicked on"
            );
        }
    }

    #[test]
    fn the_row_under_the_pointer_is_the_row_that_is_drawn_brighter() {
        // What a hover is for: the menu says which row a click would take
        // *before* the click. Read off the drawn rows rather than off the
        // cursor, so a menu that tracks the pointer but paints nothing fails
        // here.
        let mut menu = Menu::new((10, 10), items()).unwrap();
        assert!(menu.hover(12, 12, 80, 24), "a move onto row 2 is news");
        let m = Moment::default();
        let lines = menu.render(&Viewport::new(menu.rect(80, 24), &m));
        let pointed = Some(crate::frame::Color::role(Role::PanelSelBg));
        for (i, line) in lines.iter().enumerate() {
            let bright = line.spans[0].style.bg == pointed;
            assert_eq!(
                bright,
                i == 2,
                "row {i} is{} the row the pointer is on",
                if bright { "" } else { " not" }
            );
        }
    }

    #[test]
    fn hovering_walks_the_rows_without_moving_the_menu() {
        // The trap this walks around: `rect` is read off `at` and `cursor`
        // together (`y = at.y - cursor`), so a hover that moves the cursor
        // without re-anchoring moves the whole panel by the same amount — and
        // the pointer ends up over a row it did not choose, with the row it did
        // choose drawn a line or two higher. Held mid-screen on purpose: at the
        // bottom edge `rect` is clamped to fit, and the clamp would hide the
        // slide and let a broken hover look correct.
        let mut menu = Menu::new((10, 10), items()).unwrap();
        let rect = menu.rect(80, 24);
        assert_eq!(rect.y, 10, "this menu sits where it was opened");
        for (i, want) in ["copy", "paste", "clear", "send"].iter().enumerate() {
            let y = rect.y + i as u16;
            menu.hover(rect.x, y, 80, 24);
            assert_eq!(
                menu.rect(80, 24),
                rect,
                "the menu moved out from under the pointer at row {i}"
            );
            assert_eq!(
                menu.click(rect.x, y, 80, 24),
                Some(Step::Picked((*want).into())),
                "the pointer is over row {i} but that is not what is there"
            );
        }
    }

    #[test]
    fn hovering_a_menu_that_had_to_slide_still_reads_the_row_it_is_over() {
        // And the same walk for a menu that slid up to fit the bottom edge,
        // where the anchor and the rect are two different rows from the start.
        let mut menu = Menu::new((10, 23), items()).unwrap();
        let rect = menu.rect(80, 24);
        assert!(
            rect.y < 23,
            "this menu has to slide, or the case is not under test"
        );
        for (i, want) in ["copy", "paste", "clear", "send"].iter().enumerate() {
            let y = rect.y + i as u16;
            menu.hover(rect.x, y, 80, 24);
            assert_eq!(
                menu.rect(80, 24),
                rect,
                "the menu moved out from under the pointer at row {i}"
            );
            assert_eq!(
                menu.click(rect.x, y, 80, 24),
                Some(Step::Picked((*want).into())),
                "row {i} is no longer the row the pointer is over"
            );
        }
    }

    #[test]
    fn a_move_inside_the_row_it_is_already_on_is_not_news() {
        // Every cell the pointer crosses arrives here. Answering "no" is what
        // keeps a hover from costing a frame of its own.
        let mut menu = Menu::new((10, 10), items()).unwrap();
        assert!(
            menu.hover(11, 11, 80, 24),
            "the first move onto row 1 is news"
        );
        assert!(
            !menu.hover(12, 11, 80, 24),
            "a move within the same row is not news, and it cost a frame"
        );
        assert_eq!(menu.cursor(), 1);
    }

    #[test]
    fn a_pointer_beside_the_menu_leaves_the_highlight_where_it_was() {
        // The highlight is where a choice would land. A pointer that has stepped
        // off the panel is not choosing "nothing" — clearing the highlight would
        // make Enter's target blink out under a pointer that merely drifted.
        let mut menu = Menu::new((10, 10), items()).unwrap();
        menu.hover(11, 11, 80, 24);
        let before = menu.rect(80, 24);
        assert!(
            !menu.hover(0, 0, 80, 24),
            "a move outside the menu is not news"
        );
        assert_eq!(menu.cursor(), 1, "the highlight was cleared from off-menu");
        assert_eq!(menu.rect(80, 24), before, "and the menu moved");
    }

    #[test]
    fn enter_takes_the_row_the_pointer_was_over() {
        // What the highlight is *for*: the row that would be taken. A menu that
        // painted the hovered row but left the pick on another one is the exact
        // bug this feature is about.
        let mut menu = Menu::new((10, 10), items()).unwrap();
        menu.hover(11, 13, 80, 24);
        assert_eq!(menu.cursor(), 3);
        assert_eq!(menu.key(press(Key::Enter)), Step::Picked("send".into()));
    }

    #[test]
    fn every_row_is_filled_to_the_rect_with_the_panel_behind_it() {
        // The point of it being a panel: what it covers must not show through
        // on the right of the last word. The pointed-at row is filled the same
        // way — with the one step brighter patch of the same surface, which is
        // still a background, and still not the screen showing through.
        let menu = Menu::new((0, 0), items()).unwrap();
        let m = Moment::default();
        let rect = menu.rect(80, 24);
        let lines = menu.render(&Viewport::new(rect, &m));
        let plain = Some(crate::frame::Color::role(Role::PanelBg));
        let pointed = Some(crate::frame::Color::role(Role::PanelSelBg));
        for (i, line) in lines.iter().enumerate() {
            let want = if i == menu.cursor() { pointed } else { plain };
            assert_eq!(line.width(), rect.w as usize, "row {i} is not filled");
            assert!(
                line.spans.iter().all(|s| s.style.bg == want),
                "row {i} has a cell with no background"
            );
        }
    }

    #[test]
    fn the_pointed_at_row_is_a_brighter_panel_not_an_inverted_one() {
        // What this replaces: `reverse`, which the terminal decides the meaning
        // of — on a dark terminal, a near-white bar across a near-black panel.
        // A row being pointed at is a row, not an alarm.
        let menu = Menu::new((0, 0), items()).unwrap();
        let m = Moment::default();
        let rect = menu.rect(80, 24);
        let lines = menu.render(&Viewport::new(rect, &m));
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.spans.iter().all(|s| !s.style.reverse),
                "row {i} is drawn inverted"
            );
        }
        assert_ne!(
            lines[menu.cursor()].spans[0].style.bg,
            lines[menu.cursor() + 1].spans[0].style.bg,
            "the pointed-at row is the same patch as the rows around it"
        );
        // And the two patches come from the same resolution, so the step is a
        // step and not two independent guesses at a colour.
        let text = theme::resolve(Role::PanelFg, crate::caps::Caps::default());
        assert!(
            lines[menu.cursor()]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(crate::frame::Color::Role(Role::PanelFg))),
            "the pointed-at row keeps the panel's ink: {text:?}"
        );
    }

    #[test]
    fn the_label_and_its_gloss_are_both_drawn() {
        let menu = Menu::new((0, 0), items()).unwrap();
        let rows = drawn(&menu, 80, 24);
        assert_eq!(rows.first().unwrap().trim(), "复制全文");
        // And the about text, where one is given.
        let with_about =
            Menu::new((0, 0), vec![Item::new("x", "复制全文").about("放剪贴板")]).unwrap();
        let rows = drawn(&with_about, 80, 24);
        assert!(
            rows[0].contains("复制全文") && rows[0].contains("放剪贴板"),
            "{rows:?}"
        );
    }
}
