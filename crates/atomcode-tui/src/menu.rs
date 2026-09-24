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
    /// Shown dimmed after the label, in the column every other gloss is in.
    /// Empty for an item that needs no gloss.
    pub about: String,
    /// Shown dimmed at the **end** of the row, after the gloss: what the thing
    /// takes, as a command spells it (`[file]`, `[turn]`). Empty for an item that
    /// takes nothing.
    ///
    /// At the tail rather than in the column with the label, and that is the
    /// whole reason the field exists: an argument list is the one part of a row
    /// with no length anyone controls — `/plugin`'s is a whole usage sentence —
    /// while [`name_column`] is one cell for the whole list. In the name column
    /// a long one pushed its own gloss off to the right and, being the widest row
    /// in the table, the glosses of the rows that fit with it. From the tail it
    /// can run long, or be cut by the panel's right edge, without moving
    /// anything: the two columns people read down are the name and the gloss.
    pub hint: String,
}

impl Item {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            about: String::new(),
            hint: String::new(),
        }
    }
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = about.into();
        self
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
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

/// The furthest the glosses' column may sit, in cells, for a panel `w` wide:
/// half of it, less the `  /` in front of the name.
///
/// The column is as wide as the widest name in the table, up to here. What a
/// command *takes* is not a name and is not measured — it rides at the tail
/// ([`Item::hint`]), where its length moves nothing — so what sets the column
/// is names alone, and a skill's name is routinely thirty cells
/// (`ai-for-science-ai4s-perf-tuning`). A fixed stop short of that left every
/// such row ragged, which on a list of skills was nearly every row. Half the
/// panel still leaves the gloss the other half, and a name wider than that
/// keeps the plain gap on its own row rather than pushing every gloss off the
/// panel.
fn name_stop(w: usize) -> usize {
    (w / 2).saturating_sub(3)
}

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
        // The gap and the words after the label — the gloss, then the hint at
        // the tail, each with its own two cells of separation.
        let after = |s: &str| {
            if s.is_empty() {
                0
            } else {
                2 + width::str_width(s)
            }
        };
        2 + width::str_width(&item.label) + after(&item.about) + after(&item.hint) + TRAILING
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
            spans.extend(after_the_name(item, base, here, panel));
            out.push(pad(Line::from_spans(spans), w, base));
        }
        out
    }
}

/// The slash menu: what a `/` is offering, and which of it is pointed at.
///
/// The same vocabulary as [`Menu`] — one cursor, a row under the pointer, a
/// panel that covers what it is drawn over — and deliberately not the same
/// struct. This one hangs off the composer's top edge rather than off a cell,
/// and it **scrolls**: there are more commands than rows that read well, and a
/// list whose tail is off the screen is a list whose tail cannot be reached.
///
/// What a lit name *means* is kept out of here for the same reason it is in
/// [`Menu`]: completing a command and running one are two different things, and
/// which one a keystroke asks for is the composer's business. This module knows
/// how to lay a list out and which row a cell is on, and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slash {
    items: Vec<Item>,
    cursor: usize,
}

impl Slash {
    /// A list whose first row is lit.
    ///
    /// The first row and not "no row": the list exists to be chosen from, and a
    /// menu that opens with nothing pointed at it makes the person press a key
    /// before it will say what it is about to do.
    pub fn new(items: Vec<Item>) -> Self {
        Self { items, cursor: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether a key is one the list answers for itself.
    ///
    /// The list is open for the whole time a command is being *typed* — that is
    /// what a discovery surface is — so "the menu is open" cannot be the same
    /// question as "the menu takes this key". Letters and backspace belong to
    /// the composer; these five belong to the list, and they are the same keys
    /// every list in this UI speaks.
    ///
    /// **Enter is the list's**, and the caller decides what taking the row means
    /// for the line underneath: with the menu open the return key acts on the
    /// row that is lit rather than on whatever half-typed prefix happens to be
    /// there. A menu you must complete with tab before the return key will
    /// respect it is a menu that argues with the person who can see the row.
    ///
    /// Shift+enter and alt+enter are deliberately not here: they are a newline
    /// inside what is being typed, and a list that swallowed them would make
    /// `/command` impossible to write on two lines.
    pub fn owns(press: KeyPress) -> bool {
        match press.key {
            // Modifiers are ignored on the arrows, the same as the question
            // panel's: a terminal that reports ctrl+up for a plain up would
            // otherwise leave the list unmovable.
            Key::Up | Key::Down | Key::Esc => true,
            // Tab completes and enter takes, and only the bare forms of each:
            // shift-tab is the mode cycle elsewhere in this UI, and
            // shift/alt-enter is a newline here.
            Key::Tab | Key::Enter => press.mods == crate::surface::Mods::NONE,
            _ => false,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// What is lit, when anything is.
    pub fn selected(&self) -> Option<&Item> {
        self.items.get(self.cursor)
    }

    /// Move the cursor by `delta`, clamped to the list.
    ///
    /// Returns whether it moved, so a caller can tell a key that changed the
    /// highlight from one that had nowhere to go. Clamped and not wrapped, the
    /// same as [`Menu::key`] and for the same reason: past the end of a list
    /// this short there is nowhere to fall off to, and wrapping reads as a jump.
    pub fn move_by(&mut self, delta: i32) -> bool {
        let last = self.items.len().saturating_sub(1) as i32;
        let row = (self.cursor as i32 + delta).clamp(0, last) as usize;
        let moved = row != self.cursor;
        self.cursor = row;
        moved
    }

    /// The first item of the window, given how many rows are on screen.
    ///
    /// The cursor is always inside the window — a highlight that has scrolled
    /// off is a list that has lost the thing it was pointing at — and the
    /// window moves by the least it can, one row at a time. A list that
    /// recentres itself under the cursor jumps under the eye, and the eye is
    /// reading the row next to the one that moved.
    pub fn window(&self, rows: usize) -> usize {
        let rows = rows.max(1);
        if self.items.len() <= rows {
            return 0;
        }
        self.cursor
            .saturating_sub(rows - 1)
            .min(self.items.len() - rows)
    }

    /// The item a screen cell is on, when it is on one.
    ///
    /// Read off the rect the list was **drawn** in and the same window it was
    /// drawn with, so a pointer and the highlight cannot disagree about which
    /// row is which: a second formula for one layout is a whole list answering
    /// to the wrong one. The same rule [`Menu::click`] follows.
    fn row_at(&self, x: u16, y: u16, rect: Rect, rows: usize) -> Option<usize> {
        if !rect.contains(x, y) {
            return None;
        }
        let row = (y - rect.y) as usize;
        if row >= rows {
            return None;
        }
        let index = self.window(rows) + row;
        (index < self.items.len()).then_some(index)
    }

    /// The pointer moved over the list: light the row it is over.
    ///
    /// Returns whether a frame is owed — `false` both when the pointer is not
    /// on the list and when it is on the row already lit. A move inside one row
    /// is not news, and the terminal sends a move for every cell the pointer
    /// crosses, so the cheap answer is the one that matters.
    ///
    /// Unlike [`Menu::hover`] there is nothing to re-anchor: this panel is
    /// pinned to the composer's top edge, so a pointer travelling over it moves
    /// the highlight and not the panel.
    pub fn hover(&mut self, x: u16, y: u16, rect: Rect, rows: usize) -> bool {
        match self.row_at(x, y, rect, rows) {
            Some(row) if row != self.cursor => {
                self.cursor = row;
                true
            }
            _ => false,
        }
    }

    /// A press landed on the list, if it landed on the list at all.
    ///
    /// `None` means the press was outside — the caller decides what that means,
    /// because only the caller knows what else is on screen under the pointer.
    /// The value is the item's own, the same as [`Step::Picked`] carries: what
    /// a name means is not this module's business.
    pub fn click(&mut self, x: u16, y: u16, rect: Rect, rows: usize) -> Option<String> {
        let row = self.row_at(x, y, rect, rows)?;
        self.cursor = row;
        Some(self.items[row].value.clone())
    }

    /// The rows of the window, filling the rect's width.
    ///
    /// The row under the cursor is the same panel one step brighter — not
    /// reverse, for the reason [`Menu::render`] gives — and every row is filled
    /// to the rect with the panel's own colour, so the list is a surface over
    /// the conversation rather than words with the conversation around them.
    ///
    /// The names are widened to one column so the glosses line up — see
    /// [`name_column`] — which is what makes this read as a table of commands
    /// rather than as a ragged list that happens to have two parts per line.
    pub fn render(&self, rect: Rect, rows: usize) -> Vec<Line> {
        let w = rect.w as usize;
        if w == 0 || rows == 0 {
            return Vec::new();
        }
        let panel = theme::bg(Role::PanelBg).under(theme::fg(Role::PanelFg));
        let start = self.window(rows);
        let column = name_column(&self.items, name_stop(w));
        let mut out: Vec<Line> = Vec::with_capacity(rows);
        for i in 0..rows {
            let index = start + i;
            let (line, base) = match self.items.get(index) {
                Some(item) => {
                    let here = index == self.cursor;
                    let base = if here {
                        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
                    } else {
                        panel
                    };
                    let mut spans = vec![
                        Span::styled("  /".to_string(), base),
                        Span::styled(
                            widen(&item.label, column),
                            if here {
                                base
                            } else {
                                theme::fg(Role::Accent).under(panel)
                            },
                        ),
                    ];
                    spans.extend(after_the_name(item, base, here, panel));
                    (Line::from_spans(spans), base)
                }
                // Never reached in practice: the rect is sized to the window,
                // and this is what keeps a short list from showing the screen
                // through its own panel.
                None => (Line::empty(), panel),
            };
            out.push(pad(line, w, base));
        }
        out
    }
}

/// What a row says once its name is drawn: what the thing does, then what it
/// takes.
///
/// One function for both lists, because "what comes after the name" is one rule
/// and two copies of it are two places for the two panels to drift apart: the
/// gloss first — the column every row's eye reads down — and the hint at the
/// tail, after it, for the reason [`Item::hint`] gives. The hint is placed
/// rather than dropped on a narrow panel: what the panel cuts off is the end of
/// a row, and a row whose end is cut keeps both of its columns.
///
/// Dim on an ordinary row and the row's own panel on the row being pointed at,
/// the same as the gloss: a lit row is one patch of a brighter panel, and a
/// differently coloured word inside it would be a second highlight to read.
fn after_the_name(item: &Item, base: Style, here: bool, panel: Style) -> Vec<Span> {
    let dim = if here {
        base
    } else {
        theme::fg(Role::Muted).under(panel)
    };
    let mut spans: Vec<Span> = Vec::new();
    for words in [&item.about, &item.hint] {
        if !words.is_empty() {
            spans.push(Span::styled(format!("  {words}"), dim));
        }
    }
    spans
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

/// The cell the glosses start at: one past the widest name, up to `stop`
/// ([`name_stop`]).
///
/// A list of names and what they do reads as a table only if the second column
/// is a column. Ragged glosses — the difference between `/copy` and `/resume` is
/// four cells — make the eye jump to the start of every line to find the left
/// edge again, which is the work a discovery surface exists to save.
///
/// Measured over the items that **show something after the name** — a gloss or a
/// hint, since the hint starts where the gloss does on a row that has none — so
/// a list with neither comes out exactly as it was drawn before, and over the
/// whole list rather than the rows on screen: the column holds still while the
/// window scrolls under the cursor, and a column that moved with the highlight
/// would be worse than a ragged one.
///
/// Cells, not bytes, and through the same authority the rest of the layout
/// measures with — a name with a CJK argument spec (`cd <目录>`) is wider than it
/// looks, and this is the one place where getting that wrong shifts a column.
fn name_column(items: &[Item], stop: usize) -> usize {
    items
        .iter()
        .filter(|i| !i.about.is_empty() || !i.hint.is_empty())
        .map(|i| width::str_width(&i.label))
        .max()
        .unwrap_or(0)
        .min(stop)
}

/// A name widened to the column with blanks, so the gloss after it starts at the
/// same cell on every row. A name past the column is left alone.
fn widen(label: &str, column: usize) -> String {
    let used = width::str_width(label);
    if used >= column {
        return label.to_string();
    }
    format!("{label}{}", " ".repeat(column - used))
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

    #[test]
    fn the_glosses_start_in_one_column() {
        // The point of the alignment: the second column is a column, so the eye
        // reads the names down one edge and the glosses down another.
        let list = Slash::new(vec![
            Item::new("a", "a").about("短的"),
            Item::new("bb", "bbbb").about("短的"),
            Item::new("c", "cc").about("短的"),
        ]);
        let drawn: Vec<String> = slash_rows(&list, 40, 3)
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(drawn, ["  /a     短的", "  /bbbb  短的", "  /cc    短的"]);
    }

    /// A list of skills: thirty-cell names, every one past the old fixed stop of
    /// sixteen, so every gloss started where its own name ended. On a panel
    /// with room for them they are one column.
    #[test]
    fn long_skill_names_still_line_their_glosses_up() {
        let names = [
            "adapter-check-principle",
            "agent-engineering",
            "agents",
            "ai-for-science-ai4s-perf-tuning",
            "ai4s-main",
            "app",
        ];
        let list = Slash::new(
            names
                .iter()
                .map(|n| Item::new(*n, *n).about("做点什么").hint("[给它的话]"))
                .collect(),
        );
        let drawn = slash_rows(&list, 120, names.len());
        let cells: Vec<usize> = drawn
            .iter()
            .map(|row| width::str_width(&row[..row.find("做点什么").expect(row)]))
            .collect();
        assert!(
            cells.iter().all(|c| *c == cells[0]),
            "{cells:?}\n{drawn:#?}"
        );
        assert_eq!(cells[0], "  /ai-for-science-ai4s-perf-tuning  ".len());
    }

    #[test]
    fn a_name_past_the_stop_keeps_the_plain_gap() {
        // A name nobody here controls — the agent's own command — must not move
        // every other gloss: the stop is where the column is, and a name that
        // runs past it starts its gloss where its own width puts it. The names
        // that fit are still aligned.
        let stop = name_stop(40);
        let long = "x".repeat(stop + 4);
        let list = Slash::new(vec![
            Item::new("a", long.clone()).about("长的"),
            Item::new("b", "bb").about("短的"),
        ]);
        let drawn: Vec<String> = slash_rows(&list, 40, 2)
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(drawn[0], format!("  /{long}  长的"));
        assert_eq!(drawn[1], format!("  /bb{}  短的", " ".repeat(stop - 2)));
    }

    #[test]
    fn what_a_command_takes_rides_at_the_tail_and_moves_no_column() {
        // The bug this field was added for: `/plugin`'s argument list is a whole
        // usage sentence, and in the name column it pushed that row's gloss — and
        // through the column, every other row's gloss — far off to the right.
        // From the tail it lengthens one row and nothing else.
        let long = "[list | install <name> | uninstall <name> | update <name> | marketplace …]";
        let list = Slash::new(vec![
            Item::new("plugin", "plugin").about("插件").hint(long),
            Item::new("undo", "undo").about("撤销").hint("[turn]"),
            Item::new("quit", "quit").about("退出"),
        ]);
        let drawn: Vec<String> = slash_rows(&list, 200, 3)
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        // Every gloss in the column, the shortest name padded into it, and the
        // long hint after its row's gloss rather than before it.
        assert_eq!(
            drawn,
            [
                format!("  /plugin  插件  {long}"),
                "  /undo    撤销  [turn]".to_string(),
                "  /quit    退出".to_string(),
            ]
        );
    }

    // ---- the slash menu --------------------------------------------------

    fn commands(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| Item::new(format!("cmd{i}"), format!("command {i}")))
            .collect()
    }

    fn slash_rows(list: &Slash, w: u16, rows: usize) -> Vec<String> {
        let rect = Rect::new(0, 0, w, rows as u16);
        list.render(rect, rows).iter().map(|l| l.plain()).collect()
    }

    #[test]
    fn a_new_list_has_its_first_row_lit() {
        // The requirement in one line: a menu opens with something pointed at
        // it, so Enter does the obvious thing without an arrow press first.
        let list = Slash::new(commands(3));
        assert_eq!(list.cursor(), 0);
        assert_eq!(list.selected().map(|i| i.value.as_str()), Some("cmd0"));
    }

    #[test]
    fn the_cursor_is_clamped_at_both_ends() {
        let mut list = Slash::new(commands(3));
        assert!(!list.move_by(-1), "there is nothing above the first row");
        assert_eq!(list.cursor(), 0);
        assert!(list.move_by(2));
        assert_eq!(list.cursor(), 2);
        assert!(!list.move_by(1), "and nothing below the last");
        assert_eq!(list.cursor(), 2);
    }

    #[test]
    fn the_window_follows_the_cursor_by_the_least_it_can() {
        // A highlight that scrolls off the panel is a list that has lost the
        // thing it was pointing at; one that recentres jumps under the eye.
        let mut list = Slash::new(commands(10));
        assert_eq!(list.window(3), 0);
        list.move_by(2);
        assert_eq!(list.window(3), 0, "the cursor is still inside the window");
        list.move_by(1);
        assert_eq!(list.window(3), 1, "it moved by exactly one row");
        for _ in 0..6 {
            list.move_by(1);
        }
        assert_eq!(list.cursor(), 9);
        assert_eq!(list.window(3), 7, "and the tail is on screen");
        assert!(list.window(3) + 3 <= 10);
    }

    #[test]
    fn a_list_that_fits_never_scrolls() {
        let mut list = Slash::new(commands(3));
        list.move_by(2);
        assert_eq!(list.window(10), 0);
    }

    #[test]
    fn the_window_is_drawn_so_the_lit_row_is_the_one_the_cursor_is_on() {
        // The claim the window has to earn: what is lit on screen is the row
        // Enter would take. Read off the drawn rows, not off the arithmetic.
        let mut list = Slash::new(commands(6));
        for _ in 0..5 {
            list.move_by(1);
        }
        let rows = slash_rows(&list, 40, 3);
        let lit = rows
            .iter()
            .position(|r| r.contains("command 5"))
            .expect("the lit row is inside the window");
        assert_eq!(
            list.window(3) + lit,
            list.cursor(),
            "the row drawn as lit is not the row the cursor is on: {rows:?}"
        );
    }

    #[test]
    fn the_pointer_lights_the_row_it_is_over_and_only_that_row() {
        let mut list = Slash::new(commands(6));
        let rect = Rect::new(5, 10, 40, 3);
        assert!(
            list.hover(6, 11, rect, 3),
            "row 1 is a row the pointer moved onto"
        );
        assert_eq!(list.cursor(), 1);
        assert!(
            !list.hover(6, 11, rect, 3),
            "a move inside the row is not news"
        );
        assert!(
            !list.hover(0, 0, rect, 3),
            "and a pointer off the panel is not either"
        );
        assert_eq!(list.cursor(), 1, "the highlight was cleared from off-panel");
    }

    #[test]
    fn a_press_takes_the_row_the_pointer_was_over() {
        // The whole point of the highlight: the row a click lands on is the row
        // that was lit, and it is the row's own value that comes back.
        let mut list = Slash::new(commands(6));
        let rect = Rect::new(5, 10, 40, 3);
        assert_eq!(list.click(6, 12, rect, 3).as_deref(), Some("cmd2"));
        assert_eq!(list.cursor(), 2, "and the press moved the highlight to it");
    }

    #[test]
    fn a_press_off_the_panel_is_not_the_list_s_business() {
        let mut list = Slash::new(commands(6));
        let rect = Rect::new(5, 10, 40, 3);
        assert_eq!(list.click(0, 0, rect, 3), None);
        assert_eq!(list.cursor(), 0, "and it did not move the highlight");
    }

    #[test]
    fn a_click_is_read_off_the_same_window_that_was_drawn() {
        // Scrolled list: the third row of the panel is *not* the third command.
        // A second formula here would make every row below the fold answer to
        // the wrong command.
        let mut list = Slash::new(commands(6));
        for _ in 0..5 {
            list.move_by(1);
        }
        let rect = Rect::new(5, 10, 40, 3);
        let start = list.window(3);
        assert!(
            start > 0,
            "the list has to be scrolled for this to mean anything"
        );
        assert_eq!(
            list.click(6, 10, rect, 3).as_deref(),
            Some(format!("cmd{start}").as_str()),
            "the top row of a scrolled panel is the window's first item"
        );
    }

    #[test]
    fn the_lit_row_is_a_brighter_panel_and_the_rest_are_not() {
        let list = Slash::new(commands(3));
        let rect = Rect::new(0, 0, 40, 3);
        let lines = list.render(rect, 3);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(line.width(), rect.w as usize, "row {i} is not filled");
            assert!(
                line.spans.iter().all(|s| !s.style.reverse),
                "row {i} is drawn inverted"
            );
        }
        let plain = Some(crate::frame::Color::role(Role::PanelBg));
        let pointed = Some(crate::frame::Color::role(Role::PanelSelBg));
        assert_eq!(
            lines[0].spans[0].style.bg, pointed,
            "the first row is not lit"
        );
        assert_eq!(lines[1].spans[0].style.bg, plain, "row 1 is lit too");
        assert!(
            lines
                .iter()
                .all(|l| l.spans.iter().all(|s| s.style.bg.is_some())),
            "a row shows the screen through its own panel"
        );
    }
}
