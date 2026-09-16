//! Asking the person, through the screen they are already looking at.
//!
//! A question arrives over the connection as a request, is answered here
//! rather than by the transcript, and that split is deliberate: a stream
//! producer that handled keystrokes would not be a fold any more. So the queue
//! owns the question, the host owns the block and the keyboard, and they meet
//! over one small mailbox.
//!
//! What is *drawn* is not here. It was, as a modal card the event loop opened out
//! of a seam; it is now a module riding the stream's tail
//! ([`crate::modules::ask`]), because a question is the newest thing in the
//! conversation and that is where the newest thing goes. What stayed is the part
//! that is not about drawing: the queue, the keys that pick an answer, and the
//! words an answered question is recorded with.
//!
//! Nothing here decides *policy*. Whether a call needs asking about is the
//! approval row's business; this only knows how to put a question on a screen
//! and wait.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use atomcode_capabilities::tools::approval::{ApprovalRequest, APPROVAL_KIND};
use atomcode_capabilities::tools::request_user_input::{
    UserInputRequest, UserInputResponse, REQUEST_USER_INPUT_KIND,
};
use atomcode_kernel::session::LoggedEvent;
use atomcode_kernel::session::{
    Answer, Question, SessionEvent, ANSWER_ALLOW, ANSWER_ALWAYS, ANSWER_DENY,
};
use serde_json::Value;
use tokio::sync::oneshot;

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
        nth(&self.question, n)
    }
    /// What a letter picks, when an answer's value or label starts with it.
    pub fn by_prefix(&self, c: char) -> Option<String> {
        by_prefix(&self.question, c)
    }
}

/// What a number key picks, 1-based as the panel shows it.
///
/// Free functions rather than methods on the queue, because they are answers to a
/// `Question` and nothing else: a caller holding the question — the key handler,
/// which peeks rather than takes — has no `Pending` to ask, and building one to
/// ask it would mean constructing a reply channel for a question it is not
/// answering.
pub fn nth(question: &Question, n: usize) -> Option<String> {
    question
        .options
        .get(n.wrapping_sub(1))
        .map(|a| a.value.clone())
}

/// What a letter picks: the first answer whose value or label starts with it.
///
/// The label counts as well as the value, because the two are not always the
/// same word — an approval's value is `allow` while the panel says `允许一次`,
/// and a person pressing `允` means the answer they can see.
pub fn by_prefix(question: &Question, c: char) -> Option<String> {
    let c = c.to_lowercase().next()?;
    question
        .options
        .iter()
        .find(|a| a.value.to_lowercase().starts_with(c) || a.label.to_lowercase().starts_with(c))
        .map(|a| a.value.clone())
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

    /// Post a question and get the channel its answer will arrive on. A
    /// question with no answers offered is still a question: it gets the two
    /// every front end can draw, rather than a prompt nobody can answer.
    pub(crate) fn push(&self, mut question: Question) -> oneshot::Receiver<Option<String>> {
        if question.options.is_empty() {
            question.options = vec![Answer::new("yes"), Answer::new("no")];
        }
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

/// The question a request puts to the person, or `None` for a kind this
/// screen cannot draw.
///
/// The agent writes the question into the log just before it asks, so the fact
/// is on screen by the time the request is: that one is drawn when it is there
/// — options, asker and call exactly as recorded, which the request's wire
/// shape does not carry. The newest match, because a call can be asked about
/// twice. Otherwise the question is read off the request.
pub fn question_for(kind: &str, payload: &Value, events: &[LoggedEvent]) -> Option<Question> {
    let asked = |matches: &dyn Fn(&Question) -> bool| {
        events.iter().rev().find_map(|logged| match &logged.event {
            SessionEvent::Asked { question, .. } if matches(question) => Some(question.clone()),
            _ => None,
        })
    };
    match kind {
        APPROVAL_KIND => {
            let request: ApprovalRequest = serde_json::from_value(payload.clone()).ok()?;
            Some(
                asked(&|q| {
                    q.about
                        .as_ref()
                        .is_some_and(|a| a.tool == request.tool && a.arguments == request.args)
                })
                .unwrap_or_else(|| {
                    Question::approval(&request.tool, &request.args, Some(""), None)
                }),
            )
        }
        REQUEST_USER_INPUT_KIND => {
            let request: UserInputRequest = serde_json::from_value(payload.clone()).ok()?;
            Some(asked(&|q| q.prompt == request.question).unwrap_or_else(|| {
                Question {
                    prompt: request.question.clone(),
                    options: request
                        .options
                        .iter()
                        .map(|o| {
                            Answer::labelled(
                                o.label.clone(),
                                o.description.clone().unwrap_or_else(|| o.label.clone()),
                            )
                        })
                        .collect(),
                    asker: request
                        .header
                        .strip_prefix("Question · ")
                        .map(str::to_string),
                    about: None,
                }
            }))
        }
        _ => None,
    }
}

/// The answer to send back for a request, in the request's own terms. `None` —
/// declined, or nobody answered — is a refusal, never consent.
pub fn response_for(kind: &str, _question: &Question, answer: Option<String>) -> Value {
    match kind {
        APPROVAL_KIND => serde_json::json!({
            "decision": match answer.as_deref() {
                Some(ANSWER_ALLOW) => "allow",
                Some(ANSWER_ALWAYS) => "allow_always",
                _ => "deny",
            }
        }),
        REQUEST_USER_INPUT_KIND => {
            let response = match answer {
                Some(chosen) => UserInputResponse {
                    declined: false,
                    selected: vec![chosen],
                    ..Default::default()
                },
                None => UserInputResponse::declined(),
            };
            serde_json::to_value(response).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
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
/// and it is the same thing the panel showed them.
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

/// How many argument lines the panel will show before it stops.
const MOST_HIGHLIGHTS: usize = 4;

/// The first line of `text`, with a mark when there was more.
///
/// Shared with [`crate::modules::ask`], which draws the same call the old modal
/// card did: the two must summarise a payload the same way, or an approval and
/// the panel that follows it would describe one call in two ways.
pub fn one_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim().to_string();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

/// Wrap on cell width, not on bytes — the box is drawn in cells.
///
/// Shared with [`crate::modules::ask`] for the reason [`one_line`] is.
pub fn textwrap(text: &str, width: usize) -> Vec<String> {
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

    fn asked(question: Question) -> LoggedEvent {
        LoggedEvent {
            seq: 1,
            event: SessionEvent::Asked { turn: 1, question },
        }
    }

    #[tokio::test]
    async fn no_answer_is_a_refusal_not_a_hang() {
        let asks = Asks::new();
        let answer = asks.push(Question::plain("Allow?", &[]));
        // Shutting down must release whoever waits, refusing.
        asks.refuse_all();
        assert_eq!(answer.await.unwrap(), None, "never consent by default");
    }

    /// An approval is drawn as the agent recorded it — whether "always" is on
    /// offer, and who asked, are not in the request — and answered in the
    /// request's own decision words.
    #[test]
    fn an_approval_is_drawn_as_recorded_and_answered_as_a_decision() {
        let recorded = Question::approval(
            "write_file",
            r#"{"file_path":"a"}"#,
            None,
            Some("scout".into()),
        );
        let payload = serde_json::json!({ "call_id": "c", "tool": "write_file", "args": r#"{"file_path":"a"}"# });
        let question =
            question_for(APPROVAL_KIND, &payload, &[asked(recorded.clone())]).expect("drawn");
        assert_eq!(question, recorded);
        assert!(!question.has(ANSWER_ALWAYS), "never offered what was not");

        for (answer, decision) in [
            (Some(ANSWER_ALLOW), "allow"),
            (Some(ANSWER_ALWAYS), "allow_always"),
            (Some(ANSWER_DENY), "deny"),
            (None, "deny"),
        ] {
            let value = response_for(APPROVAL_KIND, &question, answer.map(str::to_string));
            assert_eq!(value["decision"], decision, "{answer:?}");
        }
    }

    #[test]
    fn a_question_read_off_the_request_when_nothing_was_recorded() {
        let payload = serde_json::json!({
            "header": "Question · scout",
            "question": "Which one?",
            "mode": "single",
            "options": [{ "label": "vanilla" }, { "label": "pistachio", "description": "green" }],
        });
        let question = question_for(REQUEST_USER_INPUT_KIND, &payload, &[]).expect("drawn");
        assert_eq!(question.prompt, "Which one?");
        assert_eq!(question.values(), vec!["vanilla", "pistachio"]);
        assert_eq!(question.asker.as_deref(), Some("scout"));

        let picked = response_for(REQUEST_USER_INPUT_KIND, &question, Some("pistachio".into()));
        let picked: UserInputResponse = serde_json::from_value(picked).unwrap();
        assert!(!picked.declined);
        assert_eq!(picked.selected, vec!["pistachio"]);
        let declined: UserInputResponse =
            serde_json::from_value(response_for(REQUEST_USER_INPUT_KIND, &question, None)).unwrap();
        assert!(declined.declined);
        assert!(question_for("something-else", &payload, &[]).is_none());
    }
}
