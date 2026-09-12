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
}

impl Picker {
    pub fn new(id: &'static str, title: impl Into<String>, choices: Vec<Choice>) -> Arc<Self> {
        Arc::new(Self {
            id,
            title: title.into(),
            all: RwLock::new(choices),
            filter: RwLock::new(String::new()),
            cursor: RwLock::new(0),
        })
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
                        "输入以筛选".to_string()
                    } else {
                        filter.clone()
                    },
                    if filter.is_empty() {
                        Style::new().dim()
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
                    if here { base } else { Style::new().dim() },
                ));
            }
            out.push(Line::from_spans(spans).truncate(w));
        }
        if items.is_empty() {
            out.push(Line::styled(
                width::take_width("  没有匹配的", w),
                Style::new().dim(),
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
                None => Step::Stay,
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
