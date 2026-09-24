//! Modals: the one thing on screen that takes the keyboard.
//!
//! An overlay is a `Region::Stack` child, so it composes like anything else —
//! but focus is **arbitration, not composition**: exactly one may hold it, and
//! the host decides which. That is the honest boundary; a region tree can say
//! two things overlap, it cannot say which one a key belongs to.
//!
//! Most modals in a coding UI are the same shape — a filtered list you move a
//! cursor through — so [`Picker`] is that shape once, and the specific ones
//! differ only in what they list and what picking means.

use crate::i18n::{t, Msg};
use std::sync::{Arc, Mutex, RwLock};

use crate::frame::{Color, Line, Rect, Span, Style};
use crate::moment::Viewport;
use crate::surface::{Key, KeyPress, Mods};
use crate::theme::Role;
use crate::width;

/// What a key did to a modal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Still open.
    Stay,
    /// Closed with a choice.
    Chose(String),
    /// Closed with nothing. Always one key away.
    Cancelled,
}

/// A modal.
pub trait Overlay: Send + Sync {
    fn id(&self) -> &'static str;
    /// Shown in the frame's border.
    fn title(&self) -> String;
    /// Draw into the rect the host gave it. Same rules as a view module: pure,
    /// no IO, never wider than the rect.
    fn render(&self, viewport: &Viewport<'_>) -> Vec<Line>;
    /// Take one key.
    fn key(&self, press: KeyPress) -> Step;
    /// How much of the screen it would like, as a fraction in percent.
    fn size(&self) -> (u8, u8) {
        (70, 60)
    }
    /// How many body rows it would fill, when it knows.
    ///
    /// A list does not know — it is as long as what is in it and scrolls. A
    /// card does, and a card with five lines in it should not be drawn in a box
    /// with eleven. Still a request: the host clamps it to the screen, like
    /// every other module's height.
    fn rows(&self) -> Option<u16> {
        None
    }
}

/// One row of a picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// What is returned when it is picked.
    pub value: String,
    /// What is shown.
    pub label: String,
    pub about: String,
    /// A leading mark — `●`/`○` for something that is on or off.
    pub mark: Option<&'static str>,
}

impl Choice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            about: String::new(),
            mark: None,
        }
    }
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = about.into();
        self
    }
    pub fn marked(mut self, on: bool) -> Self {
        self.mark = Some(if on { "●" } else { "○" });
        self
    }
}

/// A filtered list with a cursor — the shape most modals actually are.
pub struct Picker {
    id: &'static str,
    title: String,
    all: RwLock<Vec<Choice>>,
    filter: RwLock<String>,
    cursor: RwLock<usize>,
    /// What `Enter` means when the filter matches nothing: this, with `{}`
    /// replaced by what was typed.
    ///
    /// Without it a filter that matches nothing is a dead end — the keys do
    /// nothing and the only way on is to close the list and type the command
    /// out. That is exactly the case where a person already knows the answer
    /// and the list does not have it: `/cd` into a directory that is neither
    /// bookmarked, nor recent, nor under the one being browsed.
    ///
    /// `None` for a list where the choices are the only answers — picking a
    /// model that is not configured is not a thing a person can mean.
    typed_means: RwLock<Option<String>>,
}

impl Picker {
    pub fn new(id: &'static str, title: impl Into<String>, choices: Vec<Choice>) -> Arc<Self> {
        Arc::new(Self {
            id,
            title: title.into(),
            all: RwLock::new(choices),
            filter: RwLock::new(String::new()),
            cursor: RwLock::new(0),
            typed_means: RwLock::new(None),
        })
    }

    /// Let `Enter` on an unmatched filter mean `template` with `{}` filled in.
    ///
    /// See [`Picker::typed_means`]. Opt-in per list, because for most lists
    /// what was typed is a search and nothing else.
    pub fn accepting_typed(self: Arc<Self>, template: impl Into<String>) -> Arc<Self> {
        *self.typed_means.write().expect("picker poisoned") = Some(template.into());
        self
    }

    /// Replace the list, keeping the filter. For a picker whose contents change
    /// while it is open — toggling a row, say.
    pub fn refill(&self, choices: Vec<Choice>) {
        *self.all.write().expect("picker poisoned") = choices;
        let n = self.visible().len();
        let mut c = self.cursor.write().expect("picker poisoned");
        *c = (*c).min(n.saturating_sub(1));
    }

    pub fn visible(&self) -> Vec<Choice> {
        let f = self.filter.read().expect("picker poisoned").to_lowercase();
        self.all
            .read()
            .expect("picker poisoned")
            .iter()
            .filter(|c| {
                f.is_empty()
                    || c.label.to_lowercase().contains(&f)
                    || c.about.to_lowercase().contains(&f)
            })
            .cloned()
            .collect()
    }

    pub fn selected(&self) -> Option<Choice> {
        let at = *self.cursor.read().expect("picker poisoned");
        self.visible().into_iter().nth(at)
    }

    fn move_by(&self, by: i32) {
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let mut c = self.cursor.write().expect("picker poisoned");
        // Wraps, because a list you can fall off the end of is a list you have
        // to look at to use.
        *c = ((*c as i32 + by).rem_euclid(n as i32)) as usize;
    }
}

/// A file, read-only, scrollable, one key from gone.
///
/// The gap it fills: to look at a file during a session you either ask the
/// model to read it — which costs a turn and puts the whole file in the
/// conversation for good — or you leave for another window. This is neither:
/// nothing is sent, nothing is logged, and closing it leaves no trace.
///
/// It holds the text rather than the path. Reading is IO, and an overlay draws
/// under the same rule a view module does — pure, no IO — so the read happens
/// once, in the command that opens this.
pub struct Reading {
    what: String,
    lines: Vec<String>,
    at: RwLock<usize>,
    /// A diff carries its own line numbers and its own signs, so it is drawn
    /// differently from a file. One type rather than two, because scrolling,
    /// closing and "picks nothing" are the same for both and would otherwise be
    /// written twice.
    diff: bool,
    /// What `esc` goes back to, for a reader that was opened out of a list.
    ///
    /// `None` — a `/view` of a path somebody typed — closes outright: there is
    /// no list behind it to return to. Set, it is a command the way a
    /// [`Picker`]'s pick is one, so going back is the same dispatch that got
    /// here and not a second way of building the list.
    back_to: RwLock<Option<String>>,
}

impl Reading {
    /// `what` is what the frame says it is showing — a path, usually.
    pub fn new(what: impl Into<String>, text: &str) -> Arc<Self> {
        Arc::new(Self {
            what: what.into(),
            lines: text.lines().map(str::to_string).collect(),
            at: RwLock::new(0),
            diff: false,
            back_to: RwLock::new(None),
        })
    }

    /// The same reader over a unified diff: no line numbers of its own, and the
    /// signs coloured. An uncoloured diff is a wall of text with punctuation in
    /// it — the colour is what makes it readable, and it is the one place a
    /// role means exactly what it says (added, removed).
    pub fn diff(what: impl Into<String>, text: &str) -> Arc<Self> {
        Arc::new(Self {
            what: what.into(),
            lines: text.lines().map(str::to_string).collect(),
            at: RwLock::new(0),
            diff: true,
            back_to: RwLock::new(None),
        })
    }

    /// Let `esc` and `left` go back to `command` instead of closing.
    ///
    /// For a reader reached by picking a row: the reason to open one file's
    /// diff is almost always to then open the next one's, and without this that
    /// costs retyping the command that built the list — which is also the
    /// moment the list is rebuilt, so the cursor is back at the top.
    ///
    /// Opt-in for the same reason [`Picker::accepting_typed`] is: most readers
    /// have nothing behind them, and one that pretended to would dispatch a
    /// command nobody asked for on the way out.
    pub fn returning_to(self: Arc<Self>, command: impl Into<String>) -> Arc<Self> {
        *self.back_to.write().expect("reading poisoned") = Some(command.into());
        self
    }

    /// Where the window starts, for a criterion.
    pub fn top(&self) -> usize {
        *self.at.read().expect("reading poisoned")
    }

    fn scroll(&self, by: isize) {
        let mut at = self.at.write().expect("reading poisoned");
        let last = self.lines.len().saturating_sub(1);
        *at = at.saturating_add_signed(by).min(last);
    }
}

/// How one line of a unified diff is drawn.
///
/// `+++`/`---` are headers rather than content, so they are muted with the
/// hunk markers instead of being coloured as a whole added or removed file.
fn diff_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
        return Style::new().fg(Color::role(Role::Muted));
    }
    match line.as_bytes().first() {
        Some(b'+') => Style::new().fg(Color::role(Role::Success)),
        Some(b'-') => Style::new().fg(Color::role(Role::Error)),
        _ => Style::new(),
    }
}

impl Overlay for Reading {
    fn id(&self) -> &'static str {
        "view"
    }

    fn title(&self) -> String {
        self.what.clone()
    }

    fn render(&self, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        if self.lines.is_empty() {
            return vec![Line::styled(
                t(Msg::OverlayEmptyFile).into_owned(),
                Style::new().fg(Color::role(Role::Muted)),
            )];
        }
        let room = vp.rect.h as usize;
        let at = (*self.at.read().expect("reading poisoned")).min(self.lines.len() - 1);
        // Numbered, because the reason to open a file mid-session is usually to
        // say a line number out loud. Dim, so the text reads as the text. A
        // diff is not numbered: it carries `@@` markers of its own, and a
        // second set of numbers beside them would be two answers to "which
        // line".
        let width = self.lines.len().to_string().len();
        self.lines
            .iter()
            .enumerate()
            .skip(at)
            .take(room)
            .map(|(i, text)| {
                let shown = crate::text::for_screen(text).into_owned();
                if self.diff {
                    return Line::styled(shown, diff_style(text)).truncate(w);
                }
                Line::from_spans(vec![
                    Span::styled(
                        format!("{:>width$}  ", i + 1, width = width),
                        Style::new().fg(Color::role(Role::Muted)),
                    ),
                    Span::raw(shown),
                ])
                .truncate(w)
            })
            .collect()
    }

    fn key(&self, press: KeyPress) -> Step {
        match press.key {
            Key::Up => {
                self.scroll(-1);
                Step::Stay
            }
            Key::Down => {
                self.scroll(1);
                Step::Stay
            }
            Key::PageUp => {
                self.scroll(-20);
                Step::Stay
            }
            Key::PageDown => {
                self.scroll(20);
                Step::Stay
            }
            // Back to the list, when there is one behind this.
            //
            // `Left` as well as `esc`, because this is one level down from a
            // list and that is the direction people reach for — and because a
            // reader has nothing else to do with a sideways key.
            Key::Esc | Key::Left => match self.back_to.read().expect("reading poisoned").clone() {
                Some(command) => Step::Chose(command),
                None => Step::Cancelled,
            },
            // Closed with nothing otherwise: looking at a file picks nothing
            // and runs nothing. An overlay that answered with a value here
            // would dispatch that value as a command.
            _ => Step::Cancelled,
        }
    }

    fn size(&self) -> (u8, u8) {
        (80, 80)
    }
}

impl Overlay for Picker {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn render(&self, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let filter = self.filter.read().expect("picker poisoned").clone();
        out.push(
            Line::from_spans(vec![
                Span::styled("  ", Style::new()),
                Span::styled(
                    if filter.is_empty() {
                        t(Msg::OverlayFilterHint).into_owned()
                    } else {
                        filter.clone()
                    },
                    if filter.is_empty() {
                        Style::new().fg(Color::role(Role::Muted))
                    } else {
                        Style::new().fg(Color::role(Role::Warning))
                    },
                ),
            ])
            .truncate(w),
        );

        let items = self.visible();
        let at = *self.cursor.read().expect("picker poisoned");
        let room = (vp.rect.h as usize).saturating_sub(2);
        // Keep the cursor in view without scrolling more than it has to.
        let start = at.saturating_sub(room.saturating_sub(1));
        for (i, item) in items.iter().enumerate().skip(start).take(room) {
            let here = i == at;
            let base = if here {
                Style::new().reverse()
            } else {
                Style::new()
            };
            let mut spans = vec![Span::styled(if here { " ▸ " } else { "   " }, base)];
            if let Some(mark) = item.mark {
                spans.push(Span::styled(format!("{mark} "), base));
            }
            spans.push(Span::styled(item.label.clone(), base));
            if !item.about.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", item.about),
                    if here {
                        base
                    } else {
                        Style::new().fg(Color::role(Role::Muted))
                    },
                ));
            }
            out.push(Line::from_spans(spans).truncate(w));
        }
        if items.is_empty() {
            out.push(Line::styled(
                width::take_width(&t(Msg::OverlayNoMatch), w),
                Style::new().fg(Color::role(Role::Muted)),
            ));
        }
        out
    }

    fn key(&self, press: KeyPress) -> Step {
        match (press.key, press.mods) {
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => Step::Cancelled,
            (Key::Up, _) | (Key::Char('p'), Mods::CTRL) => {
                self.move_by(-1);
                Step::Stay
            }
            (Key::Down, _) | (Key::Char('n'), Mods::CTRL) => {
                self.move_by(1);
                Step::Stay
            }
            (Key::PageUp, _) => {
                self.move_by(-10);
                Step::Stay
            }
            (Key::PageDown, _) => {
                self.move_by(10);
                Step::Stay
            }
            (Key::Enter, _) => match self.selected() {
                Some(c) => Step::Chose(c.value),
                // Nothing matched. If this list said what typing means, that
                // is what it means; otherwise the key stays dead rather than
                // inventing a pick.
                None => {
                    let typed = self.filter.read().expect("picker poisoned").clone();
                    let means = self.typed_means.read().expect("picker poisoned").clone();
                    match means.filter(|_| !typed.trim().is_empty()) {
                        Some(template) => Step::Chose(template.replace("{}", typed.trim())),
                        None => Step::Stay,
                    }
                }
            },
            (Key::Backspace, _) => {
                self.filter.write().expect("picker poisoned").pop();
                *self.cursor.write().expect("picker poisoned") = 0;
                Step::Stay
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
                self.filter.write().expect("picker poisoned").push(c);
                *self.cursor.write().expect("picker poisoned") = 0;
                Step::Stay
            }
            _ => Step::Stay,
        }
    }
}

/// What to do when a modal closes.
type WhenDone = Box<dyn FnOnce(Option<String>) + Send>;

/// The modal on screen, and who is waiting for its answer.
struct Active {
    overlay: Arc<dyn Overlay>,
    done: Option<WhenDone>,
}

/// At most one modal.
#[derive(Default)]
pub struct Overlays {
    active: Mutex<Option<Active>>,
}

impl Overlays {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open one. Replaces whatever was open, cancelling it — a second modal on
    /// top of a first is a stack nobody can reason about.
    pub fn open(&self, overlay: Arc<dyn Overlay>, done: WhenDone) {
        let previous = self
            .active
            .lock()
            .expect("overlays poisoned")
            .replace(Active {
                overlay,
                done: Some(done),
            });
        if let Some(Active { done: Some(cb), .. }) = previous {
            cb(None);
        }
    }

    pub fn current(&self) -> Option<Arc<dyn Overlay>> {
        self.active
            .lock()
            .expect("overlays poisoned")
            .as_ref()
            .map(|a| a.overlay.clone())
    }

    pub fn is_open(&self) -> bool {
        self.active.lock().expect("overlays poisoned").is_some()
    }

    /// Give a key to the modal. `true` when it closed.
    pub fn key(&self, press: KeyPress) -> bool {
        let step = match self.current() {
            Some(o) => o.key(press),
            None => return false,
        };
        match step {
            Step::Stay => false,
            Step::Chose(value) => {
                self.close(Some(value));
                true
            }
            Step::Cancelled => {
                self.close(None);
                true
            }
        }
    }

    /// Close the modal with `id`, if it is still the one that is open.
    ///
    /// For a modal the *host* ends rather than the person: a step that was
    /// waiting on work that has now landed, a prompt for something that no
    /// longer needs asking. `false` when it was not open any more — which is
    /// the case this exists to get right, because work finishing late must not
    /// close whatever the person opened in the meantime.
    pub fn finish(&self, id: &str, value: Option<String>) -> bool {
        let is_it = self
            .active
            .lock()
            .expect("overlays poisoned")
            .as_ref()
            .is_some_and(|a| a.overlay.id() == id);
        if is_it {
            self.close(value);
        }
        is_it
    }

    fn close(&self, result: Option<String>) {
        let taken = self.active.lock().expect("overlays poisoned").take();
        if let Some(Active { done: Some(cb), .. }) = taken {
            cb(result);
        }
    }

    /// Close everything, cancelling. For shutdown.
    pub fn close_all(&self) {
        self.close(None);
    }
}

/// Where a modal goes: centred, as wide as it asked, and as tall as its
/// content when it knows — otherwise as tall as it asked.
pub fn modal_rect(screen: Rect, size: (u8, u8), rows: Option<u16>) -> Rect {
    let rect = frame_rect(screen, size);
    let Some(rows) = rows else {
        return rect;
    };
    // Two for the border. Never taller than the screen, and never so short
    // that the frame has nothing between its edges.
    let h = rows
        .saturating_add(2)
        .clamp(3, screen.h.max(3))
        .min(screen.h);
    Rect::new(rect.x, (screen.h.saturating_sub(h)) / 2, rect.w, h)
}

/// A framed box in the middle of the screen.
pub fn frame_rect(screen: Rect, size: (u8, u8)) -> Rect {
    let w = ((screen.w as u32 * size.0.min(100) as u32) / 100).max(10) as u16;
    let h = ((screen.h as u32 * size.1.min(100) as u32) / 100).max(3) as u16;
    let w = w.min(screen.w);
    let h = h.min(screen.h);
    Rect::new((screen.w - w) / 2, (screen.h - h) / 2, w, h)
}

/// Draw the border and title around a modal's own lines.
pub fn framed(title: &str, body: Vec<Line>, rect: Rect) -> Vec<Line> {
    let w = rect.w as usize;
    if w < 4 || rect.h < 3 {
        // Too small to frame. Return the body clipped to the rect rather than
        // as-is: an unframed body wider than its rect is exactly the overflow
        // the containment check exists to catch.
        return body
            .into_iter()
            .take(rect.h as usize)
            .map(|l| l.truncate(w))
            .collect();
    }
    let edge = Style::new().fg(Color::role(Role::Border));
    let head = format!("┌─ {title} ");
    let head_w = width::str_width(&head);
    let mut out = vec![Line::from_spans(vec![
        Span::styled(width::take_width(&head, w.saturating_sub(1)), edge),
        Span::styled(
            format!("{}┐", "─".repeat(w.saturating_sub(head_w + 1))),
            edge,
        ),
    ])
    .truncate(w)];
    let inner = w.saturating_sub(2);
    for line in body.into_iter().take((rect.h as usize).saturating_sub(2)) {
        let mut spans = vec![Span::styled("│", edge)];
        let cut = line.truncate(inner);
        let pad = inner.saturating_sub(cut.width());
        spans.extend(cut.spans);
        spans.push(Span::styled(" ".repeat(pad), Style::new()));
        spans.push(Span::styled("│", edge));
        out.push(Line::from_spans(spans).truncate(w));
    }
    while out.len() + 1 < rect.h as usize {
        out.push(
            Line::from_spans(vec![
                Span::styled("│", edge),
                Span::styled(" ".repeat(inner), Style::new()),
                Span::styled("│", edge),
            ])
            .truncate(w),
        );
    }
    out.push(Line::styled(format!("└{}┘", "─".repeat(w.saturating_sub(2))), edge).truncate(w));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moment::Moment;

    fn picker() -> Arc<Picker> {
        Picker::new(
            "test",
            "挑一个",
            vec![
                Choice::new("a", "alpha").about("first"),
                Choice::new("b", "beta").about("second"),
                Choice::new("c", "gamma").about("third"),
            ],
        )
    }

    /// A list that says what typing means is not a dead end when nothing
    /// matches.
    ///
    /// The case: `/cd` into a directory that is neither bookmarked, nor
    /// recent, nor under the one being browsed. Every row filters away, and
    /// before this the keys simply stopped working — the only way on was to
    /// close the list and type the command out, which is the one thing a
    /// person who already knows the path should not have to do.
    #[test]
    fn a_list_can_say_what_typing_something_it_does_not_have_means() {
        let p = picker().accepting_typed("/cd {}");
        for ch in "/srv/deploy".chars() {
            p.key(KeyPress::ch(ch));
        }
        assert!(p.visible().is_empty(), "nothing in the list matches it");
        assert_eq!(
            p.key(KeyPress::plain(Key::Enter)),
            Step::Chose("/cd /srv/deploy".into()),
            "what was typed is what it means"
        );

        // A match still wins: the list answering is not overridden by the
        // text that found it.
        let p = picker().accepting_typed("/cd {}");
        p.key(KeyPress::ch('a'));
        assert_eq!(p.key(KeyPress::plain(Key::Enter)), Step::Chose("a".into()));

        // And an empty filter means nothing — `Enter` on a list scrolled to
        // nowhere must not dispatch a command with a blank in it.
        let p = picker().accepting_typed("/cd {}");
        p.key(KeyPress::ch('z'));
        p.key(KeyPress::plain(Key::Backspace));
        assert!(matches!(p.key(KeyPress::plain(Key::Enter)), Step::Chose(v) if v == "a"));
    }

    /// A list that did not say so stays dead, which is the point of it being
    /// opt-in: picking a model nobody configured is not a thing a person can
    /// mean, and a command built from a typo is worse than a key that waits.
    #[test]
    fn a_list_that_said_nothing_still_refuses_what_it_does_not_have() {
        let p = picker();
        for ch in "nowhere".chars() {
            p.key(KeyPress::ch(ch));
        }
        assert!(p.visible().is_empty());
        assert_eq!(p.key(KeyPress::plain(Key::Enter)), Step::Stay);
    }

    /// Work that lands late closes the modal it was about, and no other.
    ///
    /// The case this is here for: a wizard step waiting on a login, the person
    /// gives up and opens something else, and only then does the login land.
    /// Closing by "whatever is open" would close the wrong thing and hand its
    /// waiter an answer meant for someone else.
    #[test]
    fn a_modal_is_finished_by_name_so_late_work_cannot_close_the_next_one() {
        let overlays = Overlays::new();
        let heard: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));

        let to = heard.clone();
        overlays.open(
            Picker::new("first", "一", vec![Choice::new("a", "a")]),
            Box::new(move |v| to.lock().expect("heard poisoned").push(v)),
        );
        assert!(
            overlays.finish("first", Some("done".into())),
            "the one that is open closes"
        );
        assert_eq!(
            *heard.lock().expect("heard poisoned"),
            vec![Some("done".to_string())]
        );
        assert!(!overlays.is_open());

        let to = heard.clone();
        overlays.open(
            Picker::new("second", "二", vec![Choice::new("b", "b")]),
            Box::new(move |v| to.lock().expect("heard poisoned").push(v)),
        );
        assert!(
            !overlays.finish("first", Some("late".into())),
            "the modal it was about is gone"
        );
        assert!(
            overlays.is_open(),
            "and the one that replaced it is untouched"
        );
        assert_eq!(
            heard.lock().expect("heard poisoned").len(),
            1,
            "nobody was handed an answer meant for the modal that closed"
        );
    }

    /// A file can be looked at without spending a turn on it, and looking
    /// picks nothing.
    ///
    /// The second half matters: an overlay that closed with a value would have
    /// that value dispatched as a command. Reading a file runs nothing.
    #[test]
    fn a_file_is_read_without_spending_a_turn_and_picks_nothing() {
        let text = (1..=50)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = Reading::new("src/main.rs", &text);
        let m = Moment::default();
        let drawn = |r: &Reading| {
            r.render(&Viewport::new(Rect::sized(30, 5), &m))
                .iter()
                .map(|l| l.plain())
                .collect::<Vec<_>>()
        };

        let top = drawn(&r);
        assert_eq!(top.len(), 5, "as many rows as it was given: {top:?}");
        assert!(top[0].contains("line 1"), "{top:?}");
        // Numbered, because the reason to open a file mid-session is usually to
        // say a line number out loud. Right-aligned to the file's own width.
        assert!(top[0].starts_with(" 1  "), "{top:?}");
        assert!(top[4].starts_with(" 5  "), "{top:?}");

        assert_eq!(r.key(KeyPress::plain(Key::Down)), Step::Stay);
        assert_eq!(r.top(), 1);
        assert_eq!(r.key(KeyPress::plain(Key::PageDown)), Step::Stay);
        assert_eq!(r.top(), 21);
        // It does not run off the end.
        for _ in 0..5 {
            r.key(KeyPress::plain(Key::PageDown));
        }
        assert_eq!(r.top(), 49, "the last line, not past it");
        for _ in 0..5 {
            r.key(KeyPress::plain(Key::PageUp));
        }
        assert_eq!(r.top(), 0, "nor before the first");

        // Anything else closes it, and always with nothing. `Esc` is in the
        // list because this reader has nothing behind it; one that does is
        // `a_reader_reached_from_a_list_goes_back_to_it`.
        for press in [
            KeyPress::plain(Key::Enter),
            KeyPress::plain(Key::Esc),
            KeyPress::ch('a'),
        ] {
            assert_eq!(
                Reading::new("f", "x").key(press),
                Step::Cancelled,
                "{press:?}"
            );
        }

        // An empty file says so rather than drawing nothing at all.
        let empty = Reading::new("empty.txt", "");
        assert!(drawn(&empty)[0].contains("空文件"), "{:?}", drawn(&empty));
    }

    /// A reader opened out of a list goes back to the list, not away.
    ///
    /// The reason to open one file's diff is almost always to then open the
    /// next one's, and `esc` used to close the whole thing — so seeing a
    /// second file meant retyping the command, which also rebuilt the list
    /// with the cursor back at the top. `left` as well as `esc`, because this
    /// is one level down from a list and that is the direction people reach
    /// for.
    ///
    /// The `None` half is the other half of the claim: a `/view` of a path
    /// somebody typed has no list behind it, and one that dispatched a command
    /// on the way out would run something nobody asked for.
    #[test]
    fn a_reader_reached_from_a_list_goes_back_to_it() {
        for press in [KeyPress::plain(Key::Esc), KeyPress::plain(Key::Left)] {
            assert_eq!(
                Reading::diff("src/a.rs", "@@ -1 +1 @@\n-a\n+b\n")
                    .returning_to("/diff")
                    .key(press),
                Step::Chose("/diff".into()),
                "{press:?}"
            );
            assert_eq!(
                Reading::new("notes.md", "x").key(press),
                Step::Cancelled,
                "nothing behind it: {press:?}"
            );
        }
        // Everything else still closes, list behind it or not: the reader has
        // one job and leaving is the only thing it answers.
        assert_eq!(
            Reading::diff("src/a.rs", "x")
                .returning_to("/diff")
                .key(KeyPress::ch('q')),
            Step::Cancelled
        );
    }

    /// A diff is drawn by its signs rather than by line numbers.
    ///
    /// Uncoloured it is a wall of text with punctuation in it; the colour is
    /// what makes it readable. `+++`/`---` are headers, not a whole added or
    /// removed file, so they wear the hunk marker's colour rather than green
    /// and red.
    #[test]
    fn a_diff_is_read_by_its_signs_and_not_by_line_numbers() {
        let text = "--- a/src/parser.rs\n+++ b/src/parser.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n ok\n";
        let r = Reading::diff("src/parser.rs", text);
        let m = Moment::default();
        let lines = r.render(&Viewport::new(Rect::sized(40, 6), &m));
        let plain: Vec<String> = lines.iter().map(|l| l.plain()).collect();
        assert!(
            plain.iter().all(|l| !l.starts_with(" 1 ")),
            "no numbers of its own: {plain:?}"
        );
        let colour = |n: usize| lines[n].spans.first().and_then(|s| s.style.fg);
        let role = |r: Role| Some(Color::role(r));
        assert_eq!(colour(0), role(Role::Muted), "--- is a header");
        assert_eq!(colour(1), role(Role::Muted), "+++ is a header");
        assert_eq!(colour(2), role(Role::Muted), "@@ is a marker");
        assert_eq!(colour(3), role(Role::Error), "a removed line");
        assert_eq!(colour(4), role(Role::Success), "an added line");
        assert_eq!(colour(5), None, "context is neither");
    }

    #[test]
    fn typing_filters_and_enter_returns_the_value_not_the_label() {
        let p = picker();
        assert_eq!(p.key(KeyPress::ch('b')), Step::Stay);
        assert_eq!(p.visible().len(), 1);
        assert_eq!(p.key(KeyPress::plain(Key::Enter)), Step::Chose("b".into()));
    }

    #[test]
    fn the_cursor_wraps_rather_than_sticking_at_the_end() {
        let p = picker();
        p.key(KeyPress::plain(Key::Up));
        assert_eq!(p.selected().unwrap().value, "c", "up from the top wraps");
        p.key(KeyPress::plain(Key::Down));
        assert_eq!(p.selected().unwrap().value, "a");
    }

    #[test]
    fn escape_is_always_one_key_away() {
        let p = picker();
        assert_eq!(p.key(KeyPress::plain(Key::Esc)), Step::Cancelled);
        assert_eq!(p.key(KeyPress::ctrl('c')), Step::Cancelled);
    }

    #[test]
    fn filtering_to_nothing_says_so_rather_than_going_blank() {
        let p = picker();
        for c in "zzz".chars() {
            p.key(KeyPress::ch(c));
        }
        let m = Moment::default();
        let vp = Viewport::new(Rect::sized(30, 8), &m);
        let text = p
            .render(&vp)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("没有匹配的"), "{text}");
        assert_eq!(
            p.key(KeyPress::plain(Key::Enter)),
            Step::Stay,
            "and enter does nothing"
        );
    }

    #[test]
    fn a_picker_never_draws_wider_than_its_rect() {
        let p = Picker::new(
            "wide",
            "t",
            (0..40)
                .map(|i| {
                    Choice::new(format!("v{i}"), format!("一个很长的中文标签 {i}"))
                        .about("说明也很长很长很长")
                })
                .collect(),
        );
        let m = Moment::default();
        for w in 0..60u16 {
            for h in 0..20u16 {
                let vp = Viewport::new(Rect::sized(w, h), &m);
                for line in p.render(&vp) {
                    assert!(line.width() <= w as usize, "{w}×{h}: {}", line.width());
                }
            }
        }
    }

    #[test]
    fn opening_a_second_modal_cancels_the_first_rather_than_stacking() {
        let overlays = Overlays::new();
        let first = Arc::new(Mutex::new(None::<Option<String>>));
        let seen = first.clone();
        overlays.open(picker(), Box::new(move |r| *seen.lock().unwrap() = Some(r)));
        overlays.open(picker(), Box::new(|_| {}));
        assert_eq!(
            *first.lock().unwrap(),
            Some(None),
            "the first was cancelled, not left dangling"
        );
        assert!(overlays.is_open());
    }

    #[test]
    fn a_choice_reaches_the_caller_and_closes_the_modal() {
        let overlays = Overlays::new();
        let got = Arc::new(Mutex::new(None::<Option<String>>));
        let sink = got.clone();
        overlays.open(picker(), Box::new(move |r| *sink.lock().unwrap() = Some(r)));
        assert!(!overlays.key(KeyPress::plain(Key::Down)), "still open");
        assert!(overlays.key(KeyPress::plain(Key::Enter)), "closed");
        assert_eq!(*got.lock().unwrap(), Some(Some("b".into())));
        assert!(!overlays.is_open());
    }

    #[test]
    fn the_frame_fits_its_rect_at_any_size() {
        for w in 0..40u16 {
            for h in 0..12u16 {
                let rect = Rect::sized(w, h);
                let lines = framed("标题", vec![Line::raw("内容")], rect);
                assert!(lines.len() <= (h as usize).max(1));
                for line in lines {
                    assert!(line.width() <= w as usize, "{w}×{h}: {}", line.width());
                }
            }
        }
    }

    #[test]
    fn a_centred_box_stays_inside_the_screen() {
        for w in 1..80u16 {
            for h in 1..30u16 {
                let screen = Rect::sized(w, h);
                let r = frame_rect(screen, (70, 60));
                assert!(r.right() <= w && r.bottom() <= h, "{w}×{h} gave {r:?}");
            }
        }
    }
}
