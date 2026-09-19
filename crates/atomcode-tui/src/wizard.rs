//! A wizard: several questions in a row, in one modal.
//!
//! Why this is a *shape* and not a screen of its own: the first thing a new
//! machine needs is a provider, and getting one is several questions — but
//! **which** questions is not the screen's business. The steps arrive as data
//! from whoever assembles the front end (`docs/plans/2026-09-19-remaining-gaps.md`,
//! 决策 5), so nothing in this file knows the word "onboarding", what a language
//! is for, or that a code on screen is something to scan. It draws what it was
//! given, says which step it is on, and hands back the answers.
//!
//! That division is what lets the same shape run `/setup` later, and what lets
//! a downstream fork change the questions without touching Rust — the lesson
//! from `docs/plans/2026-09-18-tui-openness-inventory.md`: what a fork wants to
//! change has to be data, or it is 165 strings to re-scan.
//!
//! **Waiting** is the one thing a wizard does that a [`crate::overlay::Picker`]
//! does not. A step may be blocked on work only the host can finish — polling a
//! login, writing files — so the host keeps the `Arc`, calls [`Wizard::say`]
//! while the work is still going and [`Wizard::resolve`] when it lands, the same
//! way a picker whose contents change is refilled. Both are `&self`, and both
//! need a repaint afterwards (`Wake::Fact`), because nothing about them came
//! from a keystroke.

use std::sync::{Arc, RwLock};

use crate::caps::Glyph;
use crate::frame::{Color, Line, Span, Style};
use crate::moment::Viewport;
use crate::overlay::{Choice, Overlay, Step};
use crate::raster::Raster;
use crate::surface::{Key, KeyPress, Mods};
use crate::theme::Role;
use crate::width;

/// What one step asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// Something to read. Enter goes on.
    Note,
    /// One of these.
    Choose(Vec<Choice>),
    /// Something to type.
    ///
    /// Not for secrets — a password is [`crate::secret::SecretPrompt`], and the
    /// reason is in that file: what is typed here is an ordinary answer and is
    /// drawn as it is typed.
    Type { placeholder: String },
    /// The host is doing something; the person is waiting for it.
    ///
    /// `skippable` is the difference between "you may go on without this" and
    /// "this has to finish" — and it is the only reason a key moves a waiting
    /// step at all.
    Wait { skippable: bool },
}

/// One step, as the host defined it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepDef {
    /// What the answer is filed under, and what the host is told it is on.
    pub id: String,
    /// The one line at the top. Also what the rail names.
    pub title: String,
    /// The body, already laid out in lines by whoever knows what it says.
    ///
    /// Lines rather than a paragraph on purpose: whoever writes the step is
    /// the one who knows how long a line of it should be.
    pub body: Vec<String>,
    /// A picture under the body, if the step has one.
    ///
    /// A [`Raster`] rather than more lines, because a picture is a cell grid
    /// with colours of its own (`docs/adr/0027`) — a QR code drawn in the
    /// theme's colours is a QR code that does not scan. Drawn only where the
    /// terminal paints cell backgrounds; see the render.
    pub picture: Option<Raster>,
    pub kind: StepKind,
}

impl StepDef {
    pub fn new(id: impl Into<String>, title: impl Into<String>, kind: StepKind) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            body: Vec::new(),
            picture: None,
            kind,
        }
    }

    pub fn saying(mut self, body: Vec<String>) -> Self {
        self.body = body;
        self
    }

    /// Put a picture under the body — [`crate::qr::code`] makes one.
    pub fn showing(mut self, picture: Raster) -> Self {
        self.picture = Some(picture);
        self
    }
}

/// What the host is told when a step opens.
type OnStep = Box<dyn Fn(&str) + Send + Sync>;

/// Steps, where it is, and what has been answered.
pub struct Wizard {
    id: &'static str,
    title: String,
    steps: RwLock<Vec<StepDef>>,
    at: RwLock<usize>,
    /// `None` for a step that was skipped — which a host must be able to tell
    /// apart from one answered with nothing.
    answers: RwLock<Vec<(String, Option<String>)>>,
    typed: RwLock<String>,
    cursor: RwLock<usize>,
    on_step: OnStep,
}

impl Wizard {
    /// What the modal closes with when every step is done.
    ///
    /// A wizard's result is its [`answers`](Wizard::answers), which are several
    /// and are read from the `Arc` the host kept; the closing value says only
    /// that it reached the end. `None` from the host's callback is the person
    /// giving up — those are the two outcomes, and they must not be confused.
    pub const DONE: &'static str = "done";

    /// `on_step` is called with the id of every step as it opens, starting with
    /// the first, before this returns. It is how the host learns there is work
    /// to start — the wizard itself starts nothing.
    pub fn new(
        id: &'static str,
        title: impl Into<String>,
        steps: Vec<StepDef>,
        on_step: OnStep,
    ) -> Arc<Self> {
        let first = steps.first().map(|s| s.id.clone());
        let me = Arc::new(Self {
            id,
            title: title.into(),
            steps: RwLock::new(steps),
            at: RwLock::new(0),
            answers: RwLock::new(Vec::new()),
            typed: RwLock::new(String::new()),
            cursor: RwLock::new(0),
            on_step,
        });
        if let Some(first) = first {
            (me.on_step)(&first);
        }
        me
    }

    pub fn at(&self) -> usize {
        *self.at.read().expect("wizard poisoned")
    }

    pub fn total(&self) -> usize {
        self.steps.read().expect("wizard poisoned").len()
    }

    /// Every step that has been answered, in order. `None` is a skip.
    pub fn answers(&self) -> Vec<(String, Option<String>)> {
        self.answers.read().expect("wizard poisoned").clone()
    }

    fn step(&self) -> Option<StepDef> {
        let at = self.at();
        self.steps.read().expect("wizard poisoned").get(at).cloned()
    }

    /// Change what the current step says, while it is open.
    ///
    /// For a wait that has news — a code that expired, a login that got as far
    /// as the browser. The step's kind does not change: what it is waiting for
    /// is still what it is waiting for.
    pub fn say(&self, body: Vec<String>) {
        let at = self.at();
        if let Some(step) = self.steps.write().expect("wizard poisoned").get_mut(at) {
            step.body = body;
        }
    }

    /// The host finished what the current step was waiting for.
    ///
    /// Answers with `Some(answer)` and moves on. `true` when that was the last
    /// step — the wizard is done and the host should close it
    /// ([`crate::overlay::Overlays::finish`]); nothing here can close a modal,
    /// because nothing here holds the modals.
    ///
    /// Ignored, with `false`, unless the current step is a [`StepKind::Wait`]:
    /// a resolve that arrives late — the person already moved on, or gave up —
    /// must not answer a question it was not about.
    pub fn resolve(&self, answer: impl Into<String>) -> bool {
        if !matches!(self.step().map(|s| s.kind), Some(StepKind::Wait { .. })) {
            return false;
        }
        matches!(self.advance(Some(answer.into())), Step::Chose(_))
    }

    /// Record an answer for the current step and open the next one.
    fn advance(&self, answer: Option<String>) -> Step {
        let Some(step) = self.step() else {
            return Step::Cancelled;
        };
        {
            let mut answers = self.answers.write().expect("wizard poisoned");
            answers.retain(|(id, _)| id != &step.id);
            answers.push((step.id.clone(), answer));
        }
        let last = self.total().saturating_sub(1);
        if self.at() >= last {
            return Step::Chose(Self::DONE.to_string());
        }
        let next = {
            let mut at = self.at.write().expect("wizard poisoned");
            *at += 1;
            *at
        };
        self.reset_entry();
        let id = self
            .steps
            .read()
            .expect("wizard poisoned")
            .get(next)
            .map(|s| s.id.clone());
        // Outside every lock: the host is free to call straight back in —
        // `say` on a step it already knows about is the ordinary case.
        if let Some(id) = id {
            (self.on_step)(&id);
        }
        Step::Stay
    }

    /// Back one step, forgetting what that step answered.
    ///
    /// Forgetting is the point: a step you are looking at again is a question
    /// that is open again, and an answer left behind would be one the person
    /// never gave the second time.
    fn back(&self) {
        let at = self.at();
        if at == 0 {
            return;
        }
        let previous = {
            let mut at = self.at.write().expect("wizard poisoned");
            *at -= 1;
            *at
        };
        self.reset_entry();
        let id = self
            .steps
            .read()
            .expect("wizard poisoned")
            .get(previous)
            .map(|s| s.id.clone());
        if let Some(id) = id {
            self.answers
                .write()
                .expect("wizard poisoned")
                .retain(|(had, _)| had != &id);
            (self.on_step)(&id);
        }
    }

    fn reset_entry(&self) {
        self.typed.write().expect("wizard poisoned").clear();
        *self.cursor.write().expect("wizard poisoned") = 0;
    }

    fn move_by(&self, by: i32, len: usize) {
        if len == 0 {
            return;
        }
        let mut c = self.cursor.write().expect("wizard poisoned");
        *c = ((*c as i32 + by).rem_euclid(len as i32)) as usize;
    }

    /// The rail across the top: what is done, where it is, what is left.
    fn rail(&self, vp: &Viewport<'_>) -> Line {
        let caps = &vp.moment.caps;
        let at = self.at();
        let mut spans = vec![Span::raw("  ".to_string())];
        for i in 0..self.total() {
            let (glyph, role) = match i.cmp(&at) {
                std::cmp::Ordering::Less => (Glyph::Ok, Role::Success),
                std::cmp::Ordering::Equal => (Glyph::Pointer, Role::Accent),
                std::cmp::Ordering::Greater => (Glyph::Bullet, Role::Muted),
            };
            spans.push(Span::styled(
                format!("{} ", caps.g(glyph)),
                Style::new().fg(Color::role(role)),
            ));
        }
        spans.push(Span::styled(
            format!(
                " {} {} / {}",
                caps.g(Glyph::Separator),
                at + 1,
                self.total()
            ),
            Style::new().fg(Color::role(Role::Muted)),
        ));
        Line::from_spans(spans)
    }

    /// The last line: which keys do what, for the step that is open.
    fn hint(&self, kind: &StepKind, at: usize) -> String {
        let back = if at > 0 { " · ← 上一步" } else { "" };
        match kind {
            StepKind::Note => format!("enter 继续{back} · esc 放弃"),
            StepKind::Choose(_) => format!("↑↓ 选 · enter 确定{back} · esc 放弃"),
            StepKind::Type { .. } => format!("enter 确定{back} · esc 放弃"),
            StepKind::Wait { skippable: true } => "enter 跳过 · esc 放弃".to_string(),
            StepKind::Wait { skippable: false } => "esc 放弃".to_string(),
        }
    }
}

impl Overlay for Wizard {
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
        let Some(step) = self.step() else {
            return Vec::new();
        };
        let caps = &vp.moment.caps;
        let mut out = vec![
            self.rail(vp).truncate(w),
            Line::styled(
                width::take_width(&format!("  {}", step.title), w),
                Style::new().fg(Color::role(Role::Accent)),
            ),
            Line::raw(String::new()),
        ];
        for line in &step.body {
            out.push(
                Line::styled(format!("  {}", crate::text::for_screen(line)), Style::new())
                    .truncate(w),
            );
        }
        if let Some(picture) = step.picture.as_ref() {
            // Half of a two-module cell is its background colour, so a terminal
            // that drops backgrounds would draw half a QR code — worse than
            // none, because half of one still looks like a thing to scan. The
            // body says what the picture says; that is what is left.
            if vp.moment.caps.cell_background {
                out.push(Line::raw(String::new()));
                let room = crate::frame::Rect::sized(vp.rect.w, picture.rows);
                out.extend(picture.lines_in(room, vp.moment.caps));
            }
        }
        match &step.kind {
            StepKind::Note => {}
            StepKind::Choose(choices) => {
                out.push(Line::raw(String::new()));
                let at = *self.cursor.read().expect("wizard poisoned");
                for (i, choice) in choices.iter().enumerate() {
                    let here = i == at;
                    let mark = if here { caps.g(Glyph::Pointer) } else { " " };
                    let style = if here {
                        Style::new().fg(Color::role(Role::Accent))
                    } else {
                        Style::new()
                    };
                    let mut spans = vec![
                        Span::styled(format!("  {mark} "), style),
                        Span::styled(crate::text::for_screen(&choice.label).into_owned(), style),
                    ];
                    if !choice.about.is_empty() {
                        spans.push(Span::styled(
                            format!("  {}", crate::text::for_screen(&choice.about)),
                            Style::new().fg(Color::role(Role::Muted)),
                        ));
                    }
                    out.push(Line::from_spans(spans).truncate(w));
                }
            }
            StepKind::Type { placeholder } => {
                out.push(Line::raw(String::new()));
                let typed = self.typed.read().expect("wizard poisoned").clone();
                let (text, style) = if typed.is_empty() {
                    (
                        placeholder.clone(),
                        Style::new().fg(Color::role(Role::Muted)),
                    )
                } else {
                    (typed, Style::new())
                };
                out.push(
                    Line::from_spans(vec![
                        Span::styled(
                            format!("  {} ", caps.g(Glyph::Prompt)),
                            Style::new().fg(Color::role(Role::Accent)),
                        ),
                        Span::styled(crate::text::for_screen(&text).into_owned(), style),
                    ])
                    .truncate(w),
                );
            }
            StepKind::Wait { .. } => {
                out.push(Line::raw(String::new()));
                out.push(
                    Line::styled(
                        format!("  {} 等待中", caps.spinner(vp.moment.tick)),
                        Style::new().fg(Color::role(Role::Muted)),
                    )
                    .truncate(w),
                );
            }
        }
        out.push(Line::raw(String::new()));
        out.push(Line::styled(
            width::take_width(&format!("  {}", self.hint(&step.kind, self.at())), w),
            Style::new().fg(Color::role(Role::Muted)),
        ));
        out
    }

    fn key(&self, press: KeyPress) -> Step {
        // One way out that is not an answer, from any step. What it means
        // downstream is "the person gave up", never "answered with nothing".
        if matches!(
            (press.key, press.mods),
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL)
        ) {
            return Step::Cancelled;
        }
        let Some(step) = self.step() else {
            return Step::Cancelled;
        };
        match step.kind {
            StepKind::Note => match press.key {
                Key::Enter => self.advance(Some(String::new())),
                Key::Left => {
                    self.back();
                    Step::Stay
                }
                _ => Step::Stay,
            },
            StepKind::Choose(choices) => match press.key {
                Key::Up => {
                    self.move_by(-1, choices.len());
                    Step::Stay
                }
                Key::Down => {
                    self.move_by(1, choices.len());
                    Step::Stay
                }
                Key::Enter => {
                    let at = *self.cursor.read().expect("wizard poisoned");
                    match choices.get(at) {
                        Some(choice) => self.advance(Some(choice.value.clone())),
                        None => Step::Stay,
                    }
                }
                Key::Left => {
                    self.back();
                    Step::Stay
                }
                _ => Step::Stay,
            },
            StepKind::Type { .. } => match (press.key, press.mods) {
                (Key::Enter, _) => {
                    let typed = self.typed.read().expect("wizard poisoned").clone();
                    self.advance(Some(typed))
                }
                (Key::Backspace, _) => {
                    self.typed.write().expect("wizard poisoned").pop();
                    Step::Stay
                }
                (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
                    self.typed.write().expect("wizard poisoned").push(c);
                    Step::Stay
                }
                _ => Step::Stay,
            },
            // Only the host finishes what the host is doing. A skippable one is
            // the exception, and skipping is not an answer — it is recorded as
            // the absence of one.
            StepKind::Wait { skippable } => match press.key {
                Key::Enter if skippable => self.advance(None),
                _ => Step::Stay,
            },
        }
    }

    fn size(&self) -> (u8, u8) {
        (70, 70)
    }

    fn rows(&self) -> Option<u16> {
        let step = self.step()?;
        let body = step.body.len() + step.picture.as_ref().map_or(0, |p| 1 + p.rows as usize);
        let extra = match &step.kind {
            StepKind::Note => 0,
            StepKind::Choose(choices) => 1 + choices.len(),
            StepKind::Type { .. } => 2,
            StepKind::Wait { .. } => 2,
        };
        // rail, title, blank, body, extra, blank, hint
        Some((3 + body + extra + 2) as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::Moment;
    use std::sync::Mutex;

    /// The ids of the steps that opened, in order, and the wizard they belong to.
    fn wizard(steps: Vec<StepDef>) -> (Arc<Wizard>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let to = seen.clone();
        let w = Wizard::new(
            "wizard",
            "设置",
            steps,
            Box::new(move |id| to.lock().expect("seen poisoned").push(id.to_string())),
        );
        (w, seen)
    }

    fn note(id: &str) -> StepDef {
        StepDef::new(id, format!("{id} 的标题"), StepKind::Note)
    }

    fn drawn(w: &Wizard) -> Vec<String> {
        let m = Moment::default();
        w.render(&Viewport::new(Rect::sized(40, 20), &m))
            .iter()
            .map(|l| l.plain())
            .collect()
    }

    /// Nothing here decides what is asked.
    ///
    /// The criterion behind 决策 5: the steps are data, so two different step
    /// lists are two different wizards with no code in between. A file that
    /// knew about onboarding would fail this by having an opinion that survived
    /// the swap.
    #[test]
    fn the_steps_are_the_ones_the_host_gave() {
        let (a, _) = wizard(vec![
            StepDef::new("greet", "欢迎", StepKind::Note).saying(vec!["第一句".into()])
        ]);
        let (b, _) = wizard(vec![
            StepDef::new("pick", "挑一个", StepKind::Note).saying(vec!["另一句".into()])
        ]);
        let one = drawn(&a).join("\n");
        let two = drawn(&b).join("\n");
        assert!(one.contains("欢迎") && one.contains("第一句"), "{one}");
        assert!(two.contains("挑一个") && two.contains("另一句"), "{two}");
        assert!(!one.contains("挑一个"), "{one}");
    }

    #[test]
    fn enter_opens_the_next_step_and_the_rail_moves_with_it() {
        let (w, _) = wizard(vec![note("a"), note("b"), note("c")]);
        assert_eq!(w.at(), 0);
        assert!(drawn(&w)[0].contains("1 / 3"), "{:?}", drawn(&w));
        assert_eq!(w.key(KeyPress::plain(Key::Enter)), Step::Stay);
        assert_eq!(w.at(), 1);
        assert!(drawn(&w)[0].contains("2 / 3"), "{:?}", drawn(&w));
        assert!(drawn(&w)[1].contains("b 的标题"), "{:?}", drawn(&w));
    }

    /// What is filed is the value, not what was on screen.
    #[test]
    fn a_choice_is_recorded_as_the_value_not_the_label() {
        let (w, _) = wizard(vec![
            StepDef::new(
                "language",
                "语言",
                StepKind::Choose(vec![
                    Choice::new("zh", "中文"),
                    Choice::new("en", "English"),
                ]),
            ),
            note("done"),
        ]);
        assert_eq!(w.key(KeyPress::plain(Key::Down)), Step::Stay);
        assert_eq!(w.key(KeyPress::plain(Key::Enter)), Step::Stay);
        assert_eq!(w.answers(), vec![("language".into(), Some("en".into()))]);
    }

    /// A step waiting on the host is finished by the host, and by nothing else.
    #[test]
    fn a_step_waiting_on_the_host_does_not_move_for_a_key() {
        let (w, seen) = wizard(vec![
            StepDef::new("login", "登录", StepKind::Wait { skippable: false }),
            note("confirm"),
        ]);
        for key in [Key::Enter, Key::Down, Key::Left, Key::Backspace] {
            assert_eq!(w.key(KeyPress::plain(key)), Step::Stay, "{key:?}");
            assert_eq!(w.at(), 0, "{key:?} moved a step only the host can move");
        }
        assert!(!w.resolve("token"), "not the last step");
        assert_eq!(w.at(), 1);
        assert_eq!(w.answers(), vec![("login".into(), Some("token".into()))]);
        assert_eq!(
            *seen.lock().expect("seen poisoned"),
            vec!["login".to_string(), "confirm".to_string()]
        );
    }

    /// Skipping is the absence of an answer, not an empty one.
    ///
    /// A host that cannot tell those apart would treat "I will do this later"
    /// as "I did this and it came to nothing".
    #[test]
    fn a_skip_is_not_an_answer() {
        let (w, _) = wizard(vec![
            StepDef::new("login", "登录", StepKind::Wait { skippable: true }),
            note("confirm"),
        ]);
        assert_eq!(w.key(KeyPress::plain(Key::Enter)), Step::Stay);
        assert_eq!(w.answers(), vec![("login".into(), None)]);
    }

    #[test]
    fn going_back_reopens_the_question_and_forgets_its_answer() {
        let (w, seen) = wizard(vec![note("a"), note("b")]);
        w.key(KeyPress::plain(Key::Enter));
        assert_eq!(w.answers().len(), 1);
        w.key(KeyPress::plain(Key::Left));
        assert_eq!(w.at(), 0);
        assert!(w.answers().is_empty(), "{:?}", w.answers());
        assert_eq!(
            *seen.lock().expect("seen poisoned"),
            vec!["a".to_string(), "b".to_string(), "a".to_string()],
            "the host is told a question it already started is open again"
        );
    }

    /// Esc is a refusal from anywhere, and it answers nothing.
    #[test]
    fn giving_up_is_not_an_answer() {
        let (w, _) = wizard(vec![note("a"), note("b")]);
        w.key(KeyPress::plain(Key::Enter));
        assert_eq!(w.key(KeyPress::plain(Key::Esc)), Step::Cancelled);
        assert_eq!(
            w.answers(),
            vec![("a".into(), Some(String::new()))],
            "what was answered before is not rewritten by giving up; \
             the host hears `None` from the modal and that is the refusal"
        );
    }

    #[test]
    fn the_last_step_closes_with_every_answer() {
        let (w, _) = wizard(vec![
            StepDef::new(
                "name",
                "名字",
                StepKind::Type {
                    placeholder: "…".into(),
                },
            ),
            note("confirm"),
        ]);
        for c in "lee".chars() {
            w.key(KeyPress::plain(Key::Char(c)));
        }
        w.key(KeyPress::plain(Key::Backspace));
        assert_eq!(w.key(KeyPress::plain(Key::Enter)), Step::Stay);
        assert_eq!(
            w.key(KeyPress::plain(Key::Enter)),
            Step::Chose(Wizard::DONE.to_string())
        );
        assert_eq!(
            w.answers(),
            vec![
                ("name".into(), Some("le".into())),
                ("confirm".into(), Some(String::new()))
            ]
        );
    }

    /// Work that lands late must not answer a question it was not about.
    #[test]
    fn a_resolve_that_arrives_after_the_step_moved_on_answers_nothing() {
        let (w, _) = wizard(vec![
            StepDef::new("login", "登录", StepKind::Wait { skippable: true }),
            note("confirm"),
        ]);
        w.key(KeyPress::plain(Key::Enter));
        assert_eq!(w.at(), 1);
        assert!(!w.resolve("a token from the login nobody is waiting for"));
        assert_eq!(
            w.answers(),
            vec![("login".into(), None)],
            "the skipped step stayed skipped and `confirm` was not answered for the person"
        );
        assert_eq!(w.at(), 1);
    }

    #[test]
    fn what_a_waiting_step_says_can_change_while_it_waits() {
        let (w, _) = wizard(vec![StepDef::new(
            "login",
            "登录",
            StepKind::Wait { skippable: false },
        )
        .saying(vec!["扫这个码".into()])]);
        assert!(drawn(&w).join("\n").contains("扫这个码"));
        w.say(vec!["码过期了".into()]);
        let after = drawn(&w).join("\n");
        assert!(after.contains("码过期了"), "{after}");
        assert!(!after.contains("扫这个码"), "{after}");
    }

    /// A picture is drawn where it can be read, and left out where it cannot.
    ///
    /// Not a nicety: a QR code packs two modules into one cell, the lower one
    /// being the cell's background, so a terminal that drops backgrounds draws
    /// the top half of a code — which still looks like something to scan and
    /// is not. The body carries the same thing in words, and that is what a
    /// person is left with.
    #[test]
    fn a_picture_is_drawn_only_where_the_terminal_paints_backgrounds() {
        let picture = crate::qr::code("https://example.com/login").expect("it fits");
        let tall = picture.rows;
        let (w, _) = wizard(vec![StepDef::new(
            "login",
            "登录",
            StepKind::Wait { skippable: false },
        )
        .saying(vec!["或者打开 https://example.com/login".into()])
        .showing(picture)]);

        let with_backgrounds = Moment::default();
        assert!(with_backgrounds.caps.cell_background);
        let drawn = w.render(&Viewport::new(Rect::sized(60, 40), &with_backgrounds));
        assert!(
            drawn.len() > tall as usize,
            "the picture is in there: {} lines for a {tall}-row picture",
            drawn.len()
        );

        let mut plain = Moment::default();
        plain.caps.cell_background = false;
        let without = w.render(&Viewport::new(Rect::sized(60, 40), &plain));
        assert_eq!(
            without.len(),
            drawn.len() - 1 - tall as usize,
            "the picture and its blank line are gone, and nothing else changed"
        );
        assert!(
            without
                .iter()
                .any(|l| l.plain().contains("https://example.com/login")),
            "what the picture said is still on screen in words"
        );
    }

    /// The last step being one the host finishes is not a special case.
    #[test]
    fn a_wait_that_is_the_last_step_says_the_wizard_is_done() {
        let (w, _) = wizard(vec![
            note("a"),
            StepDef::new("login", "登录", StepKind::Wait { skippable: false }),
        ]);
        w.key(KeyPress::plain(Key::Enter));
        assert!(
            w.resolve("token"),
            "the host is told to close it — nothing here holds the modals"
        );
        assert_eq!(
            w.answers(),
            vec![
                ("a".into(), Some(String::new())),
                ("login".into(), Some("token".into()))
            ]
        );
    }
}
