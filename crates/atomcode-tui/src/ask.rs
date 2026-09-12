//! Asking the person, through the screen they are already looking at.
//!
//! The `user-questions` seam is answered here rather than by the transcript,
//! and that split is deliberate: a stream producer that handled keystrokes
//! would not be a fold any more. So the asker owns the question, the host owns
//! the block and the keyboard, and they meet over one small mailbox.
//!
//! Nothing here decides *policy*. Whether a call needs asking about is the
//! approval row's business; this only knows how to put a question on a screen
//! and wait.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::seams::{Question, UserQuestions, ANSWER_ALLOW, ANSWER_ALWAYS, ANSWER_DENY};
use tokio::sync::oneshot;

use crate::overlay::{Overlay, Step};

/// A question waiting for an answer.
pub struct Pending {
    pub id: u64,
    pub question: Question,
    reply: oneshot::Sender<Option<String>>,
}

impl Pending {
    /// Deliver the answer. `None` is a refusal — every caller must read it that
    /// way, never as consent.
    pub fn answer(self, choice: Option<String>) {
        let _ = self.reply.send(choice);
    }
    /// What a number key picks, 1-based as the screen shows it.
    pub fn nth(&self, n: usize) -> Option<String> {
        self.question
            .options
            .get(n.wrapping_sub(1))
            .map(|a| a.value.clone())
    }
    /// What a letter picks, when an answer's value or label starts with it.
    pub fn by_prefix(&self, c: char) -> Option<String> {
        let c = c.to_ascii_lowercase();
        self.question
            .options
            .iter()
            .find(|a| {
                a.value.to_lowercase().starts_with(c) || a.label.to_lowercase().starts_with(c)
            })
            .map(|a| a.value.clone())
    }
}

/// The mailbox between the asker and the loop.
#[derive(Default)]
pub struct Asks {
    queue: Mutex<Vec<Pending>>,
    next: AtomicU64,
    wake: Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>,
}

impl Asks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The loop registers here so a question can wake it even mid-turn.
    pub fn notify_on(&self, tx: tokio::sync::mpsc::UnboundedSender<()>) {
        *self.wake.lock().expect("asks poisoned") = Some(tx);
    }

    /// The one currently on screen, if any.
    pub fn peek(&self) -> Option<(u64, Question)> {
        self.queue
            .lock()
            .expect("asks poisoned")
            .first()
            .map(|p| (p.id, p.question.clone()))
    }

    pub fn is_waiting(&self) -> bool {
        !self.queue.lock().expect("asks poisoned").is_empty()
    }

    /// Take the front question so it can be answered.
    pub fn take(&self) -> Option<Pending> {
        let mut q = self.queue.lock().expect("asks poisoned");
        if q.is_empty() {
            None
        } else {
            Some(q.remove(0))
        }
    }

    /// Refuse everything still waiting. For shutdown: a caller blocked on an
    /// answer that is never coming would hold the turn open forever.
    pub fn refuse_all(&self) {
        let waiting: Vec<Pending> = self
            .queue
            .lock()
            .expect("asks poisoned")
            .drain(..)
            .collect();
        for p in waiting {
            p.answer(None);
        }
    }

    fn push(&self, question: Question) -> oneshot::Receiver<Option<String>> {
        let (reply, rx) = oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.queue.lock().expect("asks poisoned").push(Pending {
            id,
            question,
            reply,
        });
        if let Some(tx) = self.wake.lock().expect("asks poisoned").as_ref() {
            let _ = tx.send(());
        }
        rx
    }
}

/// The pump's view of the screen's questions. Answers never arrive over the
/// wire here — the person answers on screen — so `answer` has nothing to
/// route; what the pump needs is the release on cancel and shutdown.
impl atomcode_harness::plugins::handle::Answers for Asks {
    fn answer(&self, _id: u64, _value: serde_json::Value) -> bool {
        false
    }
    fn refuse_all(&self) {
        Asks::refuse_all(self)
    }
    fn close(&self) {
        Asks::refuse_all(self)
    }
}

/// Fills `user-questions` by putting the question on the screen.
pub struct ScreenQuestions {
    asks: Arc<Asks>,
}

impl ScreenQuestions {
    pub fn new(asks: Arc<Asks>) -> Self {
        Self { asks }
    }
}

#[async_trait]
impl UserQuestions for ScreenQuestions {
    fn describe(&self) -> String {
        "the person at the terminal".into()
    }

    async fn ask(&self, question: &Question) -> Option<String> {
        // A question with no answers offered is still a question: give it the
        // two every front end can draw, rather than a prompt nobody can answer.
        let mut question = question.clone();
        if question.options.is_empty() {
            question.options = vec![
                atomcode_harness::seams::Answer::new("yes"),
                atomcode_harness::seams::Answer::new("no"),
            ];
        }
        let rx = self.asks.push(question);
        // No timeout here on purpose: the person is right there, and a question
        // that expired while they were reading it would deny a call they were
        // about to allow. Shutdown refuses everything instead.
        rx.await.ok().flatten()
    }
}

// ---- how a question is put on screen -------------------------------------

/// Who draws a question.
///
/// A seam, not a branch, for the reason every panel here is a row: the plain
/// lines at the foot of the stream are what a screen can always fall back to,
/// and anything better than that is a decision a product makes. Mount a row
/// that fills this and questions arrive as that row draws them; mount none and
/// the fallback still asks, still answers, still records.
pub trait AskView: Send + Sync {
    /// A modal for this question. It closes with an [`Answer::value`], or with
    /// `None` — which is a refusal.
    ///
    /// [`Answer::value`]: atomcode_harness::seams::Answer::value
    fn overlay(&self, question: &Question) -> Arc<dyn Overlay>;
}

/// What the approval card is called on screen, for the host to recognise its
/// own modal among any others.
pub const CARD: &str = "ask";

/// The shipped approval card.
pub struct Card;

impl AskView for Card {
    fn overlay(&self, question: &Question) -> Arc<dyn Overlay> {
        Arc::new(AskCard {
            question: question.clone(),
        })
    }
}

/// One question, as a modal.
///
/// What it shows is what a person needs to answer honestly: who is asking (a
/// delegated member is not the conversation), which tool, and — the part the
/// old prompt got wrong — *what the call actually does*, pulled out of the
/// arguments rather than dumped as JSON. A thousand lines of `content` in an
/// approval box is not something anyone reads, so bulk payloads are measured
/// instead of shown.
pub struct AskCard {
    question: Question,
}

/// What an answer is called on screen.
///
/// The three approval values have names of their own here; anything else wears
/// the label the asker gave it. That is the split: the harness says what an
/// answer *means*, the screen says what it *reads* like. Shared with the record
/// the transcript keeps, so the word a person picked is the word they see
/// afterwards.
pub fn answer_label(value: &str, fallback: &str) -> String {
    match value {
        ANSWER_ALLOW => "允许一次".into(),
        ANSWER_ALWAYS => "总是允许".into(),
        ANSWER_DENY => "拒绝".into(),
        _ => fallback.to_string(),
    }
}

/// How an answered question reads in the transcript afterwards.
///
/// Composed here rather than taken from the prompt because the screen has the
/// call itself: `write_file · notes.md` is what a person scrolling back needs,
/// and it is the same thing the card showed them.
pub fn recorded(question: &Question) -> String {
    let Some(about) = &question.about else {
        return question.prompt.clone();
    };
    let who = match &question.asker {
        Some(name) => format!("成员 {name} 请求 "),
        None => String::new(),
    };
    let what = highlights(&about.arguments)
        .first()
        .map(|(_, value)| format!(" · {value}"))
        .unwrap_or_default();
    format!("{who}{}{what}", about.tool)
}

impl AskCard {
    fn deny_value(&self) -> Option<String> {
        self.question
            .has(ANSWER_DENY)
            .then(|| ANSWER_DENY.to_string())
    }
}

impl Overlay for AskCard {
    fn id(&self) -> &'static str {
        CARD
    }

    fn title(&self) -> String {
        let what = if self.question.about.is_some() {
            "审批"
        } else {
            "提问"
        };
        match &self.question.asker {
            // The member's name, because "allow `write_file`?" from an agent
            // the person never spoke to is a question they cannot answer.
            Some(who) => format!("{what} · 来自成员 {who}"),
            None => what.to_string(),
        }
    }

    fn render(&self, vp: &crate::moment::Viewport<'_>) -> Vec<crate::frame::Line> {
        use crate::frame::{Line, Span, Style};
        use crate::theme::{self, Role};
        use crate::width;

        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let dim = theme::fg(Role::Muted);
        let body = w.saturating_sub(4);
        let mut out = vec![Line::empty()];

        match &self.question.about {
            Some(about) => {
                out.push(
                    Line::from_spans(vec![
                        Span::styled("  ", Style::new()),
                        Span::styled(about.tool.clone(), theme::fg(Role::Accent)),
                    ])
                    .truncate(w),
                );
                for (key, value) in highlights(&about.arguments) {
                    let mut spans = vec![Span::styled("  ", Style::new())];
                    if !key.is_empty() {
                        spans.push(Span::styled(format!("{key} "), dim));
                    }
                    spans.push(Span::styled(
                        width::take_width(&value, body.saturating_sub(width::str_width(&key) + 1)),
                        Style::new(),
                    ));
                    out.push(Line::from_spans(spans).truncate(w));
                }
            }
            // Not an approval: the sentence is all there is, and it is enough.
            None => {
                for line in textwrap(&self.question.prompt, body) {
                    out.push(Line::styled(format!("  {line}"), Style::new()).truncate(w));
                }
            }
        }

        out.push(Line::empty());
        for (i, answer) in self.question.options.iter().enumerate() {
            let mut spans = vec![
                Span::styled(format!("  {}  ", i + 1), theme::fg(Role::Accent)),
                Span::styled(answer_label(&answer.value, &answer.label), Style::new()),
            ];
            // What "always" would actually cover. A person saying it is owed
            // the scope they are saying it to — and "every call of this tool"
            // is a very different promise from "this one command".
            if answer.value == ANSWER_ALWAYS {
                if let Some(grant) = self
                    .question
                    .about
                    .as_ref()
                    .and_then(|a| a.grant.as_deref())
                {
                    let covers = if grant.trim().is_empty() {
                        "这个工具的全部调用".to_string()
                    } else {
                        format!("仅限 {}", one_line(grant))
                    };
                    spans.push(Span::styled(format!("  {covers}"), dim));
                }
            }
            out.push(Line::from_spans(spans).truncate(w));
        }
        out.push(Line::empty());
        out.push(Line::styled(width::take_width("  数字键选择 · esc 拒绝", w), dim).truncate(w));
        out
    }

    fn key(&self, press: crate::surface::KeyPress) -> Step {
        use crate::surface::{Key, Mods};
        match (press.key, press.mods) {
            // Refusing is always one key away, and it is an answer — never a
            // hang, never consent.
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => match self.deny_value() {
                Some(deny) => Step::Chose(deny),
                None => Step::Cancelled,
            },
            (Key::Char(c), _) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap_or(0) as usize;
                match self.question.options.get(n.wrapping_sub(1)) {
                    Some(answer) => Step::Chose(answer.value.clone()),
                    None => Step::Stay,
                }
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
                let c = c.to_ascii_lowercase();
                match self
                    .question
                    .options
                    .iter()
                    .find(|a| a.value.to_lowercase().starts_with(c))
                {
                    Some(answer) => Step::Chose(answer.value.clone()),
                    None => Step::Stay,
                }
            }
            // Enter takes the only answer when there is one, and never picks
            // among several: a stray return must not approve anything.
            (Key::Enter, _) if self.question.options.len() == 1 => {
                Step::Chose(self.question.options[0].value.clone())
            }
            _ => Step::Stay,
        }
    }

    fn size(&self) -> (u8, u8) {
        // Wide enough for a path or a command; the height comes from `rows`,
        // so the box is the size of what is in it.
        (80, 45)
    }

    /// Blank, the call, a blank, one line per answer, a blank, the hint.
    fn rows(&self) -> Option<u16> {
        let about = match &self.question.about {
            Some(about) => 1 + highlights(&about.arguments).len(),
            // Without a call there is only the sentence, and two lines is
            // enough for the ones a tool actually asks.
            None => 2,
        };
        Some((about + self.question.options.len() + 4).min(u16::MAX as usize) as u16)
    }
}

/// What a call is really doing, pulled out of its arguments.
///
/// Deliberately a short fixed list rather than a schema walk: these are the
/// keys that decide whether a person says yes, and everything else is noise in
/// a box they are reading under time pressure. Arguments that are not an
/// object, or carry none of these, fall back to the bytes themselves — which
/// is still the truth, just less kind.
pub fn highlights(arguments: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return vec![(String::new(), one_line(arguments))];
    };
    let Some(object) = value.as_object() else {
        return vec![(String::new(), one_line(arguments))];
    };
    let mut out = Vec::new();
    for key in [
        "command",
        "file_path",
        "path",
        "url",
        "pattern",
        "action",
        "name",
        "role",
    ] {
        if let Some(found) = object.get(key).and_then(|v| v.as_str()) {
            if !found.trim().is_empty() {
                out.push((key.to_string(), one_line(found)));
            }
        }
    }
    // Bulk payloads are measured, not shown.
    for key in ["content", "new_string", "text", "task"] {
        if let Some(found) = object.get(key).and_then(|v| v.as_str()) {
            out.push((
                key.to_string(),
                format!(
                    "{} 行 · {} 字",
                    found.lines().count(),
                    found.chars().count()
                ),
            ));
        }
    }
    if out.is_empty() {
        out.push((String::new(), one_line(arguments)));
    }
    // A box a person reads under time pressure, not a log line. The bytes are
    // still what executes; this is what they are being shown of them.
    out.truncate(MOST_HIGHLIGHTS);
    out
}

/// How many argument lines a card will show before it stops.
const MOST_HIGHLIGHTS: usize = 4;

fn one_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim().to_string();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

/// Wrap on cell width, not on bytes — the box is drawn in cells.
fn textwrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for paragraph in text.lines() {
        let mut rest = paragraph.to_string();
        while crate::width::str_width(&rest) > width {
            let head = crate::width::take_width(&rest, width);
            let taken = head.chars().count();
            rest = rest.chars().skip(taken).collect();
            out.push(head);
        }
        out.push(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_question_reaches_the_screen_and_the_answer_comes_back() {
        let asks = Asks::new();
        let q = ScreenQuestions::new(asks.clone());
        let asking = tokio::spawn(async move {
            q.ask(&Question::plain("Allow `write_file`?", &["yes", "no"]))
                .await
        });
        // The loop would see it here.
        for _ in 0..100 {
            if asks.is_waiting() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let (_, question) = asks.peek().expect("a question is waiting");
        assert!(question.prompt.contains("write_file"));
        assert_eq!(question.values(), vec!["yes", "no"]);
        asks.take().unwrap().answer(Some("yes".into()));
        assert_eq!(asking.await.unwrap().as_deref(), Some("yes"));
    }

    #[tokio::test]
    async fn no_answer_is_a_refusal_not_a_hang() {
        let asks = Asks::new();
        let q = ScreenQuestions::new(asks.clone());
        let asking = tokio::spawn(async move { q.ask(&Question::plain("Allow?", &[])).await });
        for _ in 0..100 {
            if asks.is_waiting() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        // Shutting down must release the caller, refusing.
        asks.refuse_all();
        assert_eq!(asking.await.unwrap(), None, "never consent by default");
    }

    // ---- the card -------------------------------------------------------

    use crate::frame::Rect;
    use crate::moment::{Moment, Viewport};
    use crate::surface::{Key, KeyPress, Mods};
    use atomcode_harness::seams::{AboutCall, Answer, Question as Q};

    fn approval(asker: Option<&str>, tool: &str, args: &str, grant: Option<&str>) -> Q {
        Q {
            prompt: format!("Allow `{tool}` to run?"),
            options: vec![
                Answer::labelled(ANSWER_ALLOW, "allow once"),
                Answer::labelled(ANSWER_ALWAYS, "always allow"),
                Answer::labelled(ANSWER_DENY, "deny"),
            ],
            asker: asker.map(str::to_string),
            about: Some(AboutCall {
                tool: tool.into(),
                arguments: args.into(),
                grant: grant.map(str::to_string),
            }),
        }
    }

    fn drawn(question: &Q) -> String {
        let card = AskCard {
            question: question.clone(),
        };
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(60, 12), &moment);
        card.render(&vp)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_card_names_the_member_that_is_asking() {
        let card = AskCard {
            question: approval(Some("scribe"), "write_file", "{}", None),
        };
        assert!(card.title().contains("scribe"), "{}", card.title());
        // The conversation's own call is not "from" anyone.
        let mine = AskCard {
            question: approval(None, "write_file", "{}", None),
        };
        assert!(!mine.title().contains("成员"), "{}", mine.title());
    }

    #[test]
    fn the_card_shows_what_the_call_does_not_its_json() {
        let drew = drawn(&approval(
            None,
            "write_file",
            r#"{"file_path":"src/lib.rs","content":"a\nb\nc"}"#,
            Some(""),
        ));
        assert!(drew.contains("src/lib.rs"), "{drew}");
        assert!(drew.contains("3 行"), "bulk is measured, not shown: {drew}");
        assert!(!drew.contains("\"file_path\""), "not raw json: {drew}");
    }

    #[test]
    fn always_says_what_it_would_cover() {
        let wide = drawn(&approval(None, "write_file", "{}", Some("")));
        assert!(wide.contains("这个工具的全部调用"), "{wide}");
        let narrow = drawn(&approval(
            None,
            "bash",
            r#"{"command":"rm -rf build"}"#,
            Some("rm -rf build"),
        ));
        assert!(narrow.contains("仅限 rm -rf build"), "{narrow}");
        // And the command itself is on the card, because that is what is being
        // approved.
        assert!(narrow.contains("rm -rf build"), "{narrow}");
    }

    #[test]
    fn a_card_is_as_tall_as_what_is_in_it() {
        let small = AskCard {
            question: approval(None, "write_file", r#"{"file_path":"a"}"#, None),
        };
        let big = AskCard {
            question: approval(
                None,
                "bash",
                r#"{"command":"x","path":"y","name":"z","action":"w"}"#,
                None,
            ),
        };
        assert!(
            big.rows() > small.rows(),
            "{:?} vs {:?}",
            big.rows(),
            small.rows()
        );
    }

    #[test]
    fn a_number_picks_esc_denies_and_a_stray_return_approves_nothing() {
        let card = AskCard {
            question: approval(Some("scribe"), "write_file", "{}", None),
        };
        assert_eq!(
            card.key(KeyPress::ch('1')),
            Step::Chose(ANSWER_ALLOW.into())
        );
        assert_eq!(
            card.key(KeyPress::ch('2')),
            Step::Chose(ANSWER_ALWAYS.into())
        );
        assert_eq!(
            card.key(KeyPress::plain(Key::Esc)),
            Step::Chose(ANSWER_DENY.into()),
            "esc is a deny, not a dismissal"
        );
        assert_eq!(
            card.key(KeyPress::plain(Key::Enter)),
            Step::Stay,
            "a stray return must never approve a call"
        );
        assert_eq!(card.key(KeyPress::ch('9')), Step::Stay);
        assert_eq!(
            card.key(KeyPress::new(Key::Char('c'), Mods::CTRL)),
            Step::Chose(ANSWER_DENY.into())
        );
    }

    #[test]
    fn a_question_with_no_call_behind_it_still_draws() {
        let card = AskCard {
            question: Q::plain("Keep going?", &["yes", "no"]),
        };
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(40, 10), &moment);
        let drew = card
            .render(&vp)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(drew.contains("Keep going?"), "{drew}");
        // Unknown answers keep the words the asker gave them.
        assert!(drew.contains("yes") && drew.contains("no"), "{drew}");
        assert_eq!(card.key(KeyPress::plain(Key::Esc)), Step::Cancelled);
    }

    #[test]
    fn keys_pick_by_number_or_by_first_letter() {
        let asks = Asks::new();
        // The receiver is dropped on purpose: this test only cares how the
        // pending question answers key presses, not who is waiting on it.
        drop(asks.push(Question::plain("q", &["yes", "no"])));
        let p = asks.take().unwrap();
        assert_eq!(p.nth(1).as_deref(), Some("yes"));
        assert_eq!(p.nth(2).as_deref(), Some("no"));
        assert_eq!(p.nth(3), None, "out of range picks nothing");
        assert_eq!(p.nth(0), None, "the screen is 1-based; so is this");
        assert_eq!(p.by_prefix('N').as_deref(), Some("no"));
        assert_eq!(p.by_prefix('z'), None);
    }

    #[tokio::test]
    async fn questions_queue_rather_than_overwrite_each_other() {
        let asks = Asks::new();
        let a = ScreenQuestions::new(asks.clone());
        let b = ScreenQuestions::new(asks.clone());
        let one = tokio::spawn(async move { a.ask(&Question::plain("first", &["y"])).await });
        let two = tokio::spawn(async move { b.ask(&Question::plain("second", &["y"])).await });
        for _ in 0..100 {
            if asks.queue.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(asks.queue.lock().unwrap().len(), 2, "both are waiting");
        // Answered in order, so a person never answers one prompt for another.
        while let Some(p) = asks.take() {
            p.answer(Some("y".into()));
        }
        assert_eq!(one.await.unwrap().as_deref(), Some("y"));
        assert_eq!(two.await.unwrap().as_deref(), Some("y"));
    }
}
