//! Modals: the one thing on screen that takes the keyboard.
//!
//! An overlay is a `Region::Stack` child, so it composes like anything else —
//! but focus is **arbitration, not composition**: exactly one may hold it, and
//! the host decides which. That is the honest boundary; a region tree can say
//! two things overlap, it cannot say which one a key belongs to.
//!
//! What is left here are the modals that are a flow rather than a list — the
//! onboarding and pairing wizards (`crate::wizard`). A list to pick from, or a
//! file or a diff to read, is the bottom sheet (`crate::sheet`): it rises from
//! the foot of the screen like every other working panel instead of covering
//! the conversation.

use std::sync::{Arc, Mutex};

use crate::caps::{Caps, Glyph};
use crate::frame::{Color, Line, Rect, Span, Style};
use crate::moment::Viewport;
use crate::surface::KeyPress;
use crate::theme::Role;
use crate::width;

/// A flow on screen this frame, drawn: what the bottom panel that shows it
/// (`crate::modules::flow`) needs.
///
/// A snapshot rather than the overlay itself because a module draws from the
/// moment, and the moment is plain data cloned once a frame; the overlay is
/// live state the host keeps talking to (`Wizard::say`, `resolve`). The host
/// draws it into this when it composes a frame (`Host::compose`), at the
/// screen's width, so the panel is exactly as tall as what the flow says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    pub id: String,
    pub title: String,
    pub lines: Vec<Line>,
}

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
pub fn framed(title: &str, body: Vec<Line>, rect: Rect, caps: &Caps) -> Vec<Line> {
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
    let horizontal = caps.g(Glyph::Horizontal);
    let vertical = caps.g(Glyph::Vertical);
    let head = format!("{}{} {title} ", caps.g(Glyph::TopLeft), horizontal);
    let head_w = width::str_width(&head);
    let mut out = vec![Line::from_spans(vec![
        Span::styled(width::take_width(&head, w.saturating_sub(1)), edge),
        Span::styled(
            format!(
                "{}{}",
                horizontal.repeat(w.saturating_sub(head_w + 1)),
                caps.g(Glyph::TopRight)
            ),
            edge,
        ),
    ])
    .truncate(w)];
    let inner = w.saturating_sub(2);
    for line in body.into_iter().take((rect.h as usize).saturating_sub(2)) {
        let mut spans = vec![Span::styled(vertical, edge)];
        let cut = line.truncate(inner);
        let pad = inner.saturating_sub(cut.width());
        spans.extend(cut.spans);
        spans.push(Span::styled(" ".repeat(pad), Style::new()));
        spans.push(Span::styled(vertical, edge));
        out.push(Line::from_spans(spans).truncate(w));
    }
    while out.len() + 1 < rect.h as usize {
        out.push(
            Line::from_spans(vec![
                Span::styled(vertical, edge),
                Span::styled(" ".repeat(inner), Style::new()),
                Span::styled(vertical, edge),
            ])
            .truncate(w),
        );
    }
    out.push(
        Line::styled(
            format!(
                "{}{}{}",
                caps.g(Glyph::BottomLeft),
                horizontal.repeat(w.saturating_sub(2)),
                caps.g(Glyph::BottomRight)
            ),
            edge,
        )
        .truncate(w),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::Key;

    /// The least a modal can be: a name, and Enter picking `b`. What these
    /// criteria judge is the stack of modals, not any one kind of them.
    struct Stub(&'static str);

    impl Overlay for Stub {
        fn id(&self) -> &'static str {
            self.0
        }
        fn title(&self) -> String {
            self.0.to_string()
        }
        fn render(&self, _viewport: &Viewport<'_>) -> Vec<Line> {
            Vec::new()
        }
        fn key(&self, press: KeyPress) -> Step {
            match press.key {
                Key::Enter => Step::Chose("b".into()),
                Key::Esc => Step::Cancelled,
                _ => Step::Stay,
            }
        }
    }

    fn picker() -> Arc<Stub> {
        Arc::new(Stub("pick"))
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
            Arc::new(Stub("first")),
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
            Arc::new(Stub("second")),
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
                let lines = framed("标题", vec![Line::raw("内容")], rect, &Caps::default());
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
