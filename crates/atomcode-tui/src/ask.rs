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

use crate::i18n::product::{t as pt, Msg as PMsg};
use crate::i18n::{t, Msg};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use atomcode_capabilities::tools::approval::{ApprovalRequest, APPROVAL_KIND};
use atomcode_capabilities::tools::request_user_input::{
    UserInputMode, UserInputRequest, UserInputResponse, REQUEST_USER_INPUT_KIND,
};
use atomcode_kernel::session::LoggedEvent;
use atomcode_kernel::session::{
    Answer, Question, SessionEvent, ANSWER_ALLOW, ANSWER_ALWAYS, ANSWER_ALWAYS_ALL, ANSWER_DENY,
};
use serde_json::Value;
use tokio::sync::oneshot;

/// One question as this screen puts it.
///
/// The kernel's [`Question`] is what the log can hold, and it holds no more
/// than a choice between named answers — which is all an approval, a
/// checkpoint or the `user-questions` seam ever asks. A model's own
/// `request_user_input` asks more: several answers at once, or words of the
/// person's own, and each offered answer may carry a sentence saying what it
/// means. That is kept here, beside the question, rather than added to
/// `Question`: the log's schema is not the place for how one tool likes its
/// questions drawn, and the product path does not write `Asked` for this tool
/// anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    pub question: Question,
    /// The request as the model sent it, when the question is a model's own
    /// `request_user_input` read off the wire. `None` for everything else —
    /// including a `request_user_input` the log already recorded, which is the
    /// seam's (every seam question is a choice between the answers it named).
    pub input: Option<UserInputRequest>,
}

impl From<Question> for Asked {
    fn from(question: Question) -> Self {
        Self {
            question,
            input: None,
        }
    }
}

/// What kind of answer a question wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// One of the offered answers and nothing else: approvals, checkpoints,
    /// the seam's questions.
    Choice,
    /// One of the offered answers, or words of the person's own.
    Single,
    /// Any number of the offered answers, plus words of the person's own.
    Multiple,
    /// Words of the person's own, and no offered answers at all.
    Text,
}

impl Asked {
    pub fn form(&self) -> Form {
        match self.input.as_ref().map(|r| &r.mode) {
            None => Form::Choice,
            Some(UserInputMode::Single) => Form::Single,
            Some(UserInputMode::Multiple) => Form::Multiple,
            Some(UserInputMode::Text) => Form::Text,
        }
    }

    /// The sentence the model gave an offered answer, when it gave one that
    /// says more than the answer's own name.
    pub fn description(&self, i: usize) -> Option<&str> {
        let option = self.input.as_ref()?.options.get(i)?;
        option
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty() && *d != option.label.trim())
    }

    /// The short name a batch's page tab carries: the model's own `header`,
    /// or the question's first line when there is none worth reading.
    pub fn title(&self) -> String {
        // `Question · x` is how the seam's own questions name who asks; the
        // name is still the best short handle there is.
        let header = self
            .input
            .as_ref()
            .map(|r| r.header.trim())
            .map(|h| h.strip_prefix("Question · ").unwrap_or(h).trim())
            .filter(|h| !h.is_empty() && *h != "Question");
        match header {
            Some(h) => one_line(h),
            None => one_line(&self.question.prompt),
        }
    }
}

/// What the person answered one question with.
///
/// The offered answers they picked, by value, and the words they typed, if
/// they typed any. A choice question only ever fills the first; the
/// `request_user_input` wire carries both, and a person may do both at once —
/// tick two options and add a note.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    pub selected: Vec<String>,
    pub text: Option<String>,
}

impl Reply {
    /// Words of the person's own, and nothing picked.
    pub fn typed(text: impl Into<String>) -> Self {
        Self {
            selected: Vec::new(),
            text: Some(text.into()),
        }
    }
    /// The one answer a choice question was given, for a caller that only
    /// ever offered choices: the first picked, else what was typed.
    pub fn value(&self) -> Option<&str> {
        self.selected
            .first()
            .map(String::as_str)
            .or(self.text.as_deref())
    }
    pub fn into_value(self) -> Option<String> {
        self.selected.into_iter().next().or(self.text)
    }
}

impl From<String> for Reply {
    fn from(value: String) -> Self {
        Self {
            selected: vec![value],
            text: None,
        }
    }
}

impl From<&str> for Reply {
    fn from(value: &str) -> Self {
        Self::from(value.to_string())
    }
}

/// Who is waiting on the answer: one caller for one question, or a batch's
/// caller for all of them at once.
enum Replier {
    One(oneshot::Sender<Option<Reply>>),
    Many(oneshot::Sender<Vec<Option<Reply>>>),
}

/// A request waiting for an answer: one question, or a batch of them put to
/// the person together.
pub struct Pending {
    pub id: u64,
    pub asked: Vec<Asked>,
    /// Answers given one at a time, by the fallback that draws a question at
    /// the foot of the stream when no panel is mounted. The panel answers a
    /// batch whole and never touches this.
    got: Vec<Option<Reply>>,
    reply: Replier,
}

impl Pending {
    /// Deliver every answer at once, one per question, in order. `None` is that
    /// question declined — every caller must read it that way, never as
    /// consent — and a question left off the end is declined too.
    pub fn finish(self, mut replies: Vec<Option<Reply>>) {
        match self.reply {
            Replier::One(tx) => {
                let _ = tx.send(replies.into_iter().next().flatten());
            }
            Replier::Many(tx) => {
                replies.resize(self.asked.len(), None);
                let _ = tx.send(replies);
            }
        }
    }
    /// Deliver the answer to the first question. `None` is a refusal.
    pub fn answer(self, choice: Option<Reply>) {
        self.finish(vec![choice]);
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

    /// The request currently on screen, whole, if any.
    pub fn peek(&self) -> Option<(u64, Vec<Asked>)> {
        self.queue
            .lock()
            .expect("asks poisoned")
            .first()
            .map(|p| (p.id, p.asked.clone()))
    }

    /// The question the fallback at the foot of the stream is showing: the
    /// first of the front request's that it has not answered yet.
    pub fn current(&self) -> Option<Asked> {
        let q = self.queue.lock().expect("asks poisoned");
        let front = q.first()?;
        front.asked.get(front.got.len()).cloned()
    }

    /// Answer the question [`Asks::current`] names, and deliver the request
    /// once every one of its questions has an answer. The fallback's way of
    /// answering a batch: one question at a time, in the order they were asked.
    pub fn answer_current(&self, choice: Option<Reply>) {
        let done = {
            let mut q = self.queue.lock().expect("asks poisoned");
            let Some(front) = q.first_mut() else {
                return;
            };
            front.got.push(choice);
            match front.got.len() >= front.asked.len() {
                true => Some(q.remove(0)),
                false => None,
            }
        };
        if let Some(mut p) = done {
            let got = std::mem::take(&mut p.got);
            p.finish(got);
        }
    }

    pub fn is_waiting(&self) -> bool {
        !self.queue.lock().expect("asks poisoned").is_empty()
    }

    /// Take the front request so it can be answered.
    pub fn take(&self) -> Option<Pending> {
        let mut q = self.queue.lock().expect("asks poisoned");
        if q.is_empty() {
            None
        } else {
            Some(q.remove(0))
        }
    }

    /// Take the front request only if it is the one with this id — the one the
    /// panel was drawing when the person answered it. Anything else at the front
    /// is a request they have not seen yet, and an answer is not transferable.
    pub fn take_id(&self, id: u64) -> Option<Pending> {
        let mut q = self.queue.lock().expect("asks poisoned");
        match q.first() {
            Some(p) if p.id == id => Some(q.remove(0)),
            _ => None,
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
            p.finish(Vec::new());
        }
    }

    /// Post a question and get the channel its answer will arrive on. A choice
    /// with no answers offered is still a question: it gets the two every front
    /// end can draw, rather than a prompt nobody can answer. A question that
    /// wants words is left without — the words are the answer.
    pub(crate) fn push(&self, asked: impl Into<Asked>) -> oneshot::Receiver<Option<Reply>> {
        let (reply, rx) = oneshot::channel();
        self.enqueue(vec![asked.into()], Replier::One(reply));
        rx
    }

    /// Post several questions to be answered together, and get the channel
    /// their answers arrive on — one per question, in order.
    pub(crate) fn push_batch(&self, asked: Vec<Asked>) -> oneshot::Receiver<Vec<Option<Reply>>> {
        let (reply, rx) = oneshot::channel();
        self.enqueue(asked, Replier::Many(reply));
        rx
    }

    fn enqueue(&self, mut asked: Vec<Asked>, reply: Replier) {
        for one in &mut asked {
            if one.form() == Form::Choice && one.question.options.is_empty() {
                one.question.options = vec![Answer::new("yes"), Answer::new("no")];
            }
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.queue.lock().expect("asks poisoned").push(Pending {
            id,
            asked,
            got: Vec::new(),
            reply,
        });
        if let Some(tx) = self.wake.lock().expect("asks poisoned").as_ref() {
            let _ = tx.send(());
        }
    }
}

/// The question a request puts to the person, or `None` for a kind this
/// screen cannot draw.
///
/// The agent writes the question into the log just before it asks, so the fact
/// is on screen by the time the request is: that one is drawn when it is there
/// — options, asker and call exactly as recorded, which the request's wire
/// shape does not carry. The newest match, because a call can be asked about
/// twice. Otherwise the question is read off the request — and a model's own
/// `request_user_input` read off the request keeps the request beside it, so
/// the panel can ask for what the model asked for: several answers, or words.
pub fn question_for(kind: &str, payload: &Value, events: &[LoggedEvent]) -> Option<Asked> {
    if kind == REQUEST_USER_INPUT_KIND {
        let request: UserInputRequest = serde_json::from_value(payload.clone()).ok()?;
        let recorded = events.iter().rev().find_map(|logged| match &logged.event {
            SessionEvent::Asked { question, .. } if question.prompt == request.question => {
                Some(question.clone())
            }
            _ => None,
        });
        return Some(match recorded {
            // The seam's own question, asked over this wire: drawn as recorded,
            // and answered as the choice it is.
            Some(question) => Asked::from(question),
            None => Asked {
                question: Question {
                    prompt: request.question.clone(),
                    // The answer's own name is what is drawn; what it means is
                    // the description, drawn under it (`Asked::description`).
                    options: request
                        .options
                        .iter()
                        .map(|o| Answer::labelled(o.label.clone(), o.label.clone()))
                        .collect(),
                    asker: request
                        .header
                        .strip_prefix("Question · ")
                        .map(str::to_string),
                    about: None,
                },
                input: Some(request),
            },
        });
    }
    choice_for(kind, payload, events).map(Asked::from)
}

/// Every kind but `request_user_input`: a choice between named answers.
fn choice_for(kind: &str, payload: &Value, events: &[LoggedEvent]) -> Option<Question> {
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
                    q.about.as_ref().is_some_and(|a| {
                        // Match on the exact executing bytes, not the tool NAME. A gate
                        // renames the tool it asks about (`bash (writes outside the
                        // workspace)`) while the wire request carries the raw call name
                        // (`bash`), so a name compare misses and the fallback below would
                        // synthesize a poorer question — no "allow all bash" option, the
                        // raw header — that disagrees with the card the log already drew.
                        // The arguments ARE the reconciliation anchor; a tool-name prefix
                        // keeps the match scoped without demanding the gate's exact wording.
                        a.arguments == request.args
                            && (a.tool == request.tool
                                || a.tool.starts_with(&format!("{} ", request.tool)))
                    })
                })
                .unwrap_or_else(|| {
                    Question::approval(&request.tool, &request.args, Some(""), None, None)
                }),
            )
        }
        // The kernel's own two checkpoints: a turn that hit the round fuse, and
        // one whose output kept being cut off after the automatic recovery gave
        // up. Both ask the *driver* — not the model — whether to go on, and
        // both were arriving at a screen that did not know the kind and so
        // answered nothing (`docs/plans/2026-09-18-…-inventory.md` A10). What a
        // screen that says nothing means downstream is "stop", so a turn simply
        // ended and nobody was told why.
        atomcode_kernel::event::ROUND_CAP_CHECKPOINT_KIND
        | atomcode_kernel::event::OUTPUT_TRUNCATION_CHECKPOINT_KIND => {
            let (prompt, asker) = if kind == atomcode_kernel::event::ROUND_CAP_CHECKPOINT_KIND {
                (t(Msg::AskStepLimitQuestion), t(Msg::AskStepLimitTitle))
            } else {
                (t(Msg::AskTruncatedQuestion), t(Msg::AskTruncatedTitle))
            };
            Some(Question {
                prompt: prompt.into(),
                options: vec![
                    Answer::labelled(CONTINUE.to_string(), t(Msg::AskContinue).into_owned()),
                    Answer::labelled(STOP.to_string(), t(Msg::AskStop).into_owned()),
                ],
                asker: Some(asker.into()),
                about: None,
            })
        }
        // A kind this screen has never been taught. Drawn from the payload
        // rather than refused.
        //
        // The refusal used to be `None`, which the caller turns into `Null`,
        // which the asking tool reads as "no driver can present this" and tells
        // the model **interactive questions are not supported in this
        // environment** — said by a screen that is sitting in front of somebody
        // who could have answered. That was reported as a bug once already; the
        // fix then was to teach this file one more kind, and teaching it one
        // more kind per asker is not a fix, it is a queue.
        //
        // So the contract flips: what a new asker has to do to be askable is
        // put its question in the payload, not teach this file about itself.
        other => generic(other, payload),
    }
}

/// A question from a payload nobody here recognises.
///
/// Anything with words in it can be put to a person. `None` only when there are
/// no words — a payload with nothing to read out is genuinely not a question,
/// and asking "好 / 不了" about nothing would be worse than refusing.
fn generic(kind: &str, payload: &Value) -> Option<Question> {
    let prompt = ["prompt", "question", "message", "text"]
        .iter()
        .find_map(|key| payload.get(key).and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())?;
    let offered: Vec<Answer> = payload
        .get("options")
        .and_then(Value::as_array)
        .map(|given| given.iter().filter_map(offered_answer).collect())
        .unwrap_or_default();
    Some(Question {
        prompt: prompt.to_string(),
        // Yes or no when the asker named no choices: a question with no answers
        // on screen is a question nobody can answer.
        options: if offered.is_empty() {
            vec![
                Answer::labelled(YES.to_string(), t(Msg::AskYes).into_owned()),
                Answer::labelled(NO.to_string(), t(Msg::AskNo).into_owned()),
            ]
        } else {
            offered
        },
        asker: Some(kind.to_string()),
        about: None,
    })
}

/// One offered answer: a bare string, or an object with `value`/`label`.
fn offered_answer(value: &Value) -> Option<Answer> {
    if let Some(text) = value.as_str() {
        return Some(Answer::new(text.to_string()));
    }
    let object = value.as_object()?;
    let pick = |key: &str| object.get(key).and_then(Value::as_str);
    let value = pick("value").or_else(|| pick("label"))?;
    Some(Answer::labelled(
        value.to_string(),
        pick("label").unwrap_or(value).to_string(),
    ))
}

/// The questions of a batch request, when the payload is one.
///
/// `request_user_input` puts several questions in one round trip as
/// `{"questions": [...]}` and waits for `{"responses": [...]}`. `None` for
/// every other payload, including a single question — a one-element array is
/// deliberately *not* a batch on the tool's side either.
/// What a yes and a no are called on the wire, for a question that offered no
/// answers of its own. Plain words rather than `allow`/`deny`: this is not an
/// approval, and an asker reading `allow` back would be entitled to think it
/// was.
pub const YES: &str = "yes";
pub const NO: &str = "no";

pub fn batch_for(kind: &str, payload: &Value, events: &[LoggedEvent]) -> Option<Vec<Asked>> {
    if kind != REQUEST_USER_INPUT_KIND {
        return None;
    }
    let asked = payload.get("questions")?.as_array()?;
    if asked.len() < 2 {
        return None;
    }
    // Each one through the single-question path, so a batch and a lone question
    // are drawn by the same code and cannot drift apart.
    Some(
        asked
            .iter()
            .filter_map(|one| question_for(kind, one, events))
            .collect(),
    )
}

/// One answer of a batch, in the shape the asking tool reads.
///
/// `None` — esc — is that question **declined**, which the tool turns into
/// "no answer was provided, use your own judgement". Not a refusal of the
/// batch: a person who skips one question has not skipped the others.
pub fn declinable(answer: Option<Reply>) -> Value {
    response_for(REQUEST_USER_INPUT_KIND, answer)
}

/// What a question is answered with when the person would rather talk it over
/// than pick: words for the model, not for the screen, so they are not in a
/// locale table. The model is told to stop and listen — a question answered
/// with "let's discuss" and then decided anyway is the model guessing again.
pub const CHAT_INSTEAD: &str = "The user chose to discuss this in the chat instead of \
     answering here. Do not assume an answer: stop and wait for their next message.";

/// What the two kernel checkpoints are answered with. Their own words rather
/// than `allow` / `deny`: this is not an approval, and reusing those would put
/// "允许" on a question about whether to keep going.
pub const CONTINUE: &str = "continue";
pub const STOP: &str = "stop";

/// The answer to send back for a request, in the request's own terms. `None` —
/// declined, or nobody answered — is a refusal, never consent.
pub fn response_for(kind: &str, reply: Option<Reply>) -> Value {
    if kind == REQUEST_USER_INPUT_KIND {
        // The wire carries both halves, and both go back: what was ticked and
        // what was typed. Words that are only whitespace are not an answer.
        let response = match reply {
            Some(reply) => UserInputResponse {
                declined: false,
                selected: reply.selected,
                text: reply.text.filter(|t| !t.trim().is_empty()),
                ..Default::default()
            },
            None => UserInputResponse::declined(),
        };
        return serde_json::to_value(response).unwrap_or(Value::Null);
    }
    let answer = reply.and_then(Reply::into_value);
    match kind {
        APPROVAL_KIND => match answer.as_deref() {
            Some(ANSWER_ALLOW) => serde_json::json!({ "decision": "allow" }),
            Some(ANSWER_ALWAYS) => serde_json::json!({ "decision": "allow_always" }),
            // The blanket answer carries the old front end's wire shape so a driver
            // that parses `PermissionDecision` (daemon / ACP) reads it as `AllowAlwaysAll`.
            Some(ANSWER_ALWAYS_ALL) => {
                serde_json::json!({ "decision": "allow", "remember": true, "grant_scope": "all" })
            }
            _ => serde_json::json!({ "decision": "deny" }),
        },
        // `{"continue": bool}`, and anything that is not an explicit "keep
        // going" is a stop — the kernel degrades a missing or malformed answer
        // to `false`, and this agrees with it rather than hoping.
        atomcode_kernel::event::ROUND_CAP_CHECKPOINT_KIND
        | atomcode_kernel::event::OUTPUT_TRUNCATION_CHECKPOINT_KIND => {
            serde_json::json!({ "continue": answer.as_deref() == Some(CONTINUE) })
        }
        // A kind this screen does not know: the value the person picked, as it
        // was written. `Null` stays what a *declined* question answers with —
        // an asker has to be able to tell "they said no" from "nobody could
        // ask", and before this both arrived as `Null`.
        _ => match answer {
            Some(chosen) => Value::String(chosen),
            None => Value::Null,
        },
    }
}

// ---- the panel: which page is up, which row is lit, what has been given ----

/// One row a page of the panel offers, in the order it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// An offered answer, by index into `question.options`.
    Pick(usize),
    /// Words of the person's own, beside the offered answers.
    Other,
    /// Words of the person's own as the whole answer — a question that
    /// offered none.
    Input,
    /// A multiple choice's "that is all of them".
    Submit,
    /// Talk it over instead of answering here.
    Chat,
    /// The review page's two ways out.
    Send,
    Cancel,
}

impl Slot {
    /// Whether keys pressed on this row are words rather than commands.
    pub fn types(self) -> bool {
        matches!(self, Slot::Other | Slot::Input)
    }
    /// Whether the row carries a number a digit key can reach. Every row does
    /// but the one that sends a multiple choice: it is not an answer, and
    /// numbering it would put a number between the answers and the way out.
    pub fn numbered(self) -> bool {
        !matches!(self, Slot::Submit)
    }
}

/// What one question has been given so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    /// The lit row on this question's page. Per question, so turning back to
    /// a page finds the row where it was left.
    pub cursor: usize,
    /// Which offered answers are ticked, for a multiple choice.
    pub checked: Vec<bool>,
    /// What has been typed on the page's typing row.
    pub typed: String,
    /// Where in `typed` the next character goes, as a byte offset kept on a
    /// character boundary — the same caret every single-line field on this
    /// screen keeps ([`crate::text::step_caret`] and its siblings move it).
    pub caret: usize,
    /// The answer this page was given, in a batch — what its tab is marked
    /// for and what the review page lists. A lone question is delivered the
    /// moment it is answered and never keeps one.
    pub answer: Option<Reply>,
}

/// The panel's state while a request is on screen.
///
/// Here rather than in the module's folded state because a question is not a
/// fact until it is answered; here rather than in [`crate::moment`] because
/// the keys that change it and the answers it produces are this file's
/// business, and a drawing module should only have to read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sheet {
    /// The request this is — an answer goes back only to it.
    pub id: u64,
    pub asked: Vec<Asked>,
    /// Which page is up: a question's index, or `asked.len()` for a batch's
    /// review page.
    pub tab: usize,
    pub drafts: Vec<Draft>,
    /// The review page's lit row.
    pub review: usize,
}

/// What a key did to the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to deliver yet. The panel may have changed.
    Stay,
    /// The request is answered: one reply per question, `None` declined.
    Deliver(Vec<Option<Reply>>),
}

impl Sheet {
    pub fn new(id: u64, asked: Vec<Asked>) -> Self {
        let drafts = asked
            .iter()
            .map(|a| Draft {
                checked: vec![false; a.question.options.len()],
                ..Draft::default()
            })
            .collect();
        Self {
            id,
            asked,
            tab: 0,
            drafts,
            review: 0,
        }
    }

    /// One question, for a caller with no queue behind it.
    pub fn one(asked: impl Into<Asked>) -> Self {
        Self::new(0, vec![asked.into()])
    }

    /// Several questions put together get a page each, tabs to move between
    /// them, and a page to check the answers on before they go. One question
    /// is answered where it stands.
    pub fn is_batch(&self) -> bool {
        self.asked.len() > 1
    }

    pub fn reviewing(&self) -> bool {
        self.is_batch() && self.tab >= self.asked.len()
    }

    /// The question whose page is up; `None` on the review page.
    pub fn page(&self) -> Option<&Asked> {
        match self.reviewing() {
            true => None,
            false => self.asked.get(self.tab),
        }
    }

    /// What the page that is up has been given; `None` on the review page.
    pub fn draft(&self) -> Option<&Draft> {
        match self.reviewing() {
            true => None,
            false => self.drafts.get(self.tab),
        }
    }

    /// The rows the page that is up offers, top to bottom.
    pub fn slots(&self) -> Vec<Slot> {
        let Some(asked) = self.page() else {
            return vec![Slot::Send, Slot::Cancel];
        };
        let picks = (0..asked.question.options.len()).map(Slot::Pick);
        match asked.form() {
            Form::Choice => picks.collect(),
            Form::Single => picks.chain([Slot::Other, Slot::Chat]).collect(),
            Form::Multiple => picks
                .chain([Slot::Other, Slot::Submit, Slot::Chat])
                .collect(),
            Form::Text => vec![Slot::Input, Slot::Chat],
        }
    }

    /// Which row is lit, as an index into [`Sheet::slots`].
    pub fn cursor(&self) -> usize {
        match self.reviewing() {
            true => self.review,
            false => self.drafts.get(self.tab).map_or(0, |d| d.cursor),
        }
    }

    pub fn pointed(&self) -> Option<Slot> {
        self.slots().get(self.cursor()).copied()
    }

    /// The number row `k` is drawn with, 1-based, when it has one.
    pub fn number(&self, k: usize) -> Option<usize> {
        let slots = self.slots();
        slots.get(k).filter(|s| s.numbered())?;
        Some(slots[..=k].iter().filter(|s| s.numbered()).count())
    }

    /// Light `row`, clamped to the rows there are. True when it moved.
    ///
    /// Clamped rather than rejected: a pointer on the panel's last row of
    /// padding, or an arrow pressed past the end, means the nearest row.
    pub fn point_at(&mut self, row: usize) -> bool {
        let row = row.min(self.slots().len().saturating_sub(1));
        let reviewing = self.reviewing();
        let cursor = match reviewing {
            true => &mut self.review,
            false => match self.drafts.get_mut(self.tab) {
                Some(d) => &mut d.cursor,
                None => return false,
            },
        };
        if *cursor == row {
            return false;
        }
        *cursor = row;
        true
    }

    /// Move the light by `delta` rows. Clamped, not wrapped: a light that jumps
    /// from the last row to the first reads as a slip.
    pub fn move_by(&mut self, delta: i32) -> bool {
        let last = self.slots().len().saturating_sub(1) as i32;
        let row = (self.cursor() as i32 + delta).clamp(0, last) as usize;
        self.point_at(row)
    }

    /// Turn to another page of a batch, clamped at both ends.
    pub fn turn(&mut self, delta: i32) -> bool {
        if !self.is_batch() {
            return false;
        }
        let tab = (self.tab as i32 + delta).clamp(0, self.asked.len() as i32) as usize;
        if tab == self.tab {
            return false;
        }
        self.tab = tab;
        true
    }

    /// Whether the lit row takes typing.
    pub fn typing(&self) -> bool {
        self.pointed().is_some_and(Slot::types)
    }

    /// Whether the lit row takes typing and already has words in it — the one
    /// state in which left and right move a caret rather than turn a page. An
    /// empty line has nowhere for a caret to go, so there the arrows keep their
    /// page-turning job.
    pub fn editing(&self) -> bool {
        self.typing() && self.draft().is_some_and(|d| !d.typed.is_empty())
    }

    /// The lit row's words and caret, when the lit row takes words.
    fn field(&mut self) -> Option<(&mut String, &mut usize)> {
        if !self.typing() {
            return None;
        }
        let draft = self.drafts.get_mut(self.tab)?;
        Some((&mut draft.typed, &mut draft.caret))
    }

    /// Type into the lit row at its caret, when it takes words. One line: a
    /// pasted line break becomes a space.
    pub fn type_text(&mut self, text: &str) -> bool {
        let Some((typed, caret)) = self.field() else {
            return false;
        };
        for c in text.chars() {
            let c = if c.is_control() { ' ' } else { c };
            crate::text::insert_at(typed, caret, c);
        }
        true
    }

    /// Answer a key. Every key is the panel's while it is up.
    ///
    /// **Every answer is reachable by one key.** Up/down walk the rows; enter
    /// takes the lit one; a digit goes straight to a numbered row; a letter picks
    /// the answer it starts; left/right (or tab) turn a batch's pages; esc
    /// declines. On a row that takes words the letters, digits and space are
    /// words — a person typing an answer is not choosing between rows.
    pub fn key(&mut self, press: crate::surface::KeyPress) -> Step {
        use crate::surface::{Key, Mods};
        let form = self.page().map(Asked::form);
        match (press.key, press.mods) {
            // Esc and ctrl-c decline — the whole request. Declining is an
            // answer; it is never consent, and it is always one keystroke away.
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => return self.decline(),
            (Key::Up, _) | (Key::Char('k'), Mods::CTRL) => {
                self.move_by(-1);
            }
            (Key::Down, _) | (Key::Char('j'), Mods::CTRL) => {
                self.move_by(1);
            }
            // On a line with words in it the arrows move the caret; Tab still
            // turns the page, so a batch can be left from anywhere.
            (Key::Left | Key::Right, _) if self.editing() => {
                if let Some((typed, caret)) = self.field() {
                    *caret = crate::text::step_caret(typed, *caret, press.key == Key::Right);
                }
            }
            (Key::Home, _) | (Key::Char('a'), Mods::CTRL) if self.typing() => {
                if let Some((_, caret)) = self.field() {
                    *caret = 0;
                }
            }
            (Key::End, _) | (Key::Char('e'), Mods::CTRL) if self.typing() => {
                if let Some((typed, caret)) = self.field() {
                    *caret = typed.len();
                }
            }
            (Key::Left, _) | (Key::BackTab, _) => {
                self.turn(-1);
            }
            (Key::Right, _) | (Key::Tab, _) => {
                self.turn(1);
            }
            (Key::Enter, _) => return self.enter(),
            (Key::Backspace, _) if self.typing() => {
                if let Some((typed, caret)) = self.field() {
                    crate::text::backspace_at(typed, caret);
                }
            }
            (Key::Delete, _) if self.typing() => {
                if let Some((typed, caret)) = self.field() {
                    crate::text::delete_at(typed, caret);
                }
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) if self.typing() => {
                self.type_text(&c.to_string());
            }
            (Key::Char(' '), Mods::NONE) => {
                if let (Some(Slot::Pick(i)), Some(Form::Multiple)) = (self.pointed(), form) {
                    self.toggle(i);
                }
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap_or(0) as usize;
                let Some(k) = (0..self.slots().len()).find(|k| self.number(*k) == Some(n)) else {
                    return Step::Stay;
                };
                self.point_at(k);
                return match self.slots()[k] {
                    // A number on a multiple choice ticks, like space: sending is
                    // its own row.
                    Slot::Pick(i) if form == Some(Form::Multiple) => {
                        self.toggle(i);
                        Step::Stay
                    }
                    // A number on the typing row goes there to be typed into.
                    Slot::Other | Slot::Input => Step::Stay,
                    _ => self.enter(),
                };
            }
            // A letter picks the answer that starts with it, where an answer is
            // one pick. It does *not* fall back to typing: off the typing row the
            // answers are what there is to choose between.
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT)
                if matches!(form, Some(Form::Choice | Form::Single)) =>
            {
                let at = self.page().and_then(|asked| {
                    let value = by_prefix(&asked.question, c)?;
                    asked.question.options.iter().position(|a| a.value == value)
                });
                if let Some(i) = at {
                    self.point_at(i);
                    return self.enter();
                }
            }
            _ => {}
        }
        Step::Stay
    }

    /// Take the lit row.
    pub fn enter(&mut self) -> Step {
        let Some(slot) = self.pointed() else {
            return Step::Stay;
        };
        let form = self.page().map(Asked::form);
        match slot {
            Slot::Pick(i) if form == Some(Form::Multiple) => {
                self.toggle(i);
                Step::Stay
            }
            Slot::Pick(i) => {
                let value = self
                    .page()
                    .and_then(|a| a.question.options.get(i))
                    .map(|a| a.value.clone());
                match value {
                    Some(value) => self.commit(Reply::from(value)),
                    None => Step::Stay,
                }
            }
            // On a multiple choice the words are part of the answer by being
            // there, so enter moves on to the row that sends it.
            Slot::Other if form == Some(Form::Multiple) => {
                if let Some(k) = self.slots().iter().position(|s| *s == Slot::Submit) {
                    self.point_at(k);
                }
                Step::Stay
            }
            // Nothing typed is nothing to send: the row stays, waiting.
            Slot::Other | Slot::Input => {
                let typed = self
                    .draft()
                    .map(|d| d.typed.trim().to_string())
                    .unwrap_or_default();
                match typed.is_empty() {
                    true => Step::Stay,
                    false => self.commit(Reply::typed(typed)),
                }
            }
            Slot::Submit => match self.ticked() {
                Some(reply) => self.commit(reply),
                None => Step::Stay,
            },
            Slot::Chat => Step::Deliver(vec![Some(Reply::typed(CHAT_INSTEAD)); self.asked.len()]),
            Slot::Send => Step::Deliver(self.drafts.iter().map(|d| d.answer.clone()).collect()),
            Slot::Cancel => self.decline(),
        }
    }

    /// Everything declined.
    fn decline(&self) -> Step {
        Step::Deliver(vec![None; self.asked.len()])
    }

    fn toggle(&mut self, i: usize) {
        if let Some(checked) = self
            .drafts
            .get_mut(self.tab)
            .and_then(|d| d.checked.get_mut(i))
        {
            *checked = !*checked;
        }
    }

    /// What a multiple choice sends: everything ticked, in the order offered,
    /// and the words, if there are any. `None` when there is neither — a send
    /// with nothing in it is not an answer, and esc is there for "none".
    pub fn ticked(&self) -> Option<Reply> {
        let asked = self.page()?;
        let draft = self.draft()?;
        let selected: Vec<String> = asked
            .question
            .options
            .iter()
            .zip(&draft.checked)
            .filter(|(_, ticked)| **ticked)
            .map(|(a, _)| a.value.clone())
            .collect();
        let typed = draft.typed.trim();
        let text = (!typed.is_empty()).then(|| typed.to_string());
        if selected.is_empty() && text.is_none() {
            return None;
        }
        Some(Reply { selected, text })
    }

    /// A question answered: sent at once when it is the only one, and in a
    /// batch kept on its page while the next unanswered one after it comes up —
    /// the review page when there is none.
    fn commit(&mut self, reply: Reply) -> Step {
        if !self.is_batch() {
            return Step::Deliver(vec![Some(reply)]);
        }
        if let Some(draft) = self.drafts.get_mut(self.tab) {
            draft.answer = Some(reply);
        }
        self.tab = (self.tab + 1..self.asked.len())
            .find(|i| self.drafts.get(*i).is_some_and(|d| d.answer.is_none()))
            .unwrap_or(self.asked.len());
        Step::Stay
    }

    /// How question `i`'s answer reads on the review page: the offered
    /// answers' own words, then what was typed. `None` when it has none.
    pub fn recap(&self, i: usize) -> Option<String> {
        let reply = self.drafts.get(i)?.answer.as_ref()?;
        let asked = self.asked.get(i)?;
        let mut said: Vec<String> = reply
            .selected
            .iter()
            .map(|value| {
                let label = asked
                    .question
                    .options
                    .iter()
                    .find(|a| &a.value == value)
                    .map_or(value.as_str(), |a| a.label.as_str());
                answer_label(value, label)
            })
            .collect();
        said.extend(reply.text.clone());
        Some(said.join(", "))
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
    // Two of the three are the product's own words — the other front end's
    // approval panel offers them — so they are read from its table rather than
    // restated. "Always" is this screen's: the product's entry names the tool
    // it covers, and this row has the scope on a line of its own.
    match value {
        ANSWER_ALLOW => pt(PMsg::ApprovalAllowOnce).into_owned(),
        ANSWER_ALWAYS => t(Msg::AskAlwaysAllow).into_owned(),
        // The session-wide blanket — the same product words the old front end offered.
        ANSWER_ALWAYS_ALL => pt(PMsg::ApprovalAllowAllBash).into_owned(),
        ANSWER_DENY => pt(PMsg::ApprovalDeny).into_owned(),
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
        Some(name) => t(Msg::AskMemberRequests { name }).into_owned(),
        None => String::new(),
    };
    let what = highlights(&about.arguments)
        .first()
        // The scrollback record is one line — the full command lives in the panel
        // that asked; here it is a summary someone scrolling past reads at a glance.
        .map(|(_, value)| format!(" · {}", one_line(value)))
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
                // The command is the exact thing being approved, so it is kept
                // WHOLE — a heredoc body cut to `…` hides what would actually run.
                // The panel wraps it and caps it to leave room for the options.
                // Every other key is single-line, so `one_line` is a no-op there.
                let value = match key {
                    "command" => found.trim().to_string(),
                    _ => one_line(found),
                };
                out.push((key.to_string(), value));
            }
        }
    }
    // Bulk payloads are measured, not shown.
    for key in ["content", "new_string", "text", "task"] {
        if let Some(found) = object.get(key).and_then(|v| v.as_str()) {
            out.push((
                key.to_string(),
                t(Msg::AskLinesChars {
                    lines: found.lines().count(),
                    chars: found.chars().count(),
                })
                .into_owned(),
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
            let mut head = crate::width::take_width(&rest, width);
            // A character wider than the whole row still has to go somewhere, or
            // this never gets shorter: it goes on a row of its own, and the
            // drawing cuts what does not fit.
            if head.is_empty() {
                head = rest.chars().next().map(String::from).unwrap_or_default();
            }
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
            at: 0,
            event: SessionEvent::Asked { turn: 1, question },
        }
    }

    /// Several questions in one request are several questions on screen, and
    /// the answers go back together.
    ///
    /// Found while using it: the model asked two things at once and the person
    /// was told **「Interactive questions are not supported in this
    /// environment」** — by a screen with a question panel, sitting right in
    /// front of them. The payload of a batch is `{"questions": [...]}`, which
    /// the single-question parse could not read, and a screen that cannot read
    /// a request answers `Null`, which the asking tool reads as "no driver can
    /// present this".
    #[test]
    fn several_questions_in_one_request_are_several_questions_on_screen() {
        let two = serde_json::json!({
            "questions": [
                {
                    "header": "Question · 建仓方案",
                    "question": "git 仓怎么建?",
                    "mode": "single",
                    "options": [
                        { "label": "独立本体仓", "description": "只含本体与证据" },
                        { "label": "当前目录建仓", "description": "要严格 .gitignore" },
                    ],
                },
                {
                    "header": "Question · 敏感信息",
                    "question": "真人姓名怎么处理?",
                    "mode": "single",
                    "options": [
                        { "label": "保留姓名,私仓" },
                        { "label": "脱敏后再入库" },
                    ],
                },
            ]
        });
        let asked = batch_for(REQUEST_USER_INPUT_KIND, &two, &[])
            .expect("a batch is a batch, not an unreadable payload");
        let questions: Vec<Question> = asked.iter().map(|a| a.question.clone()).collect();
        assert_eq!(questions.len(), 2, "both are put to the person");
        assert_eq!(
            asked[0].title(),
            "建仓方案",
            "each page's tab is its header"
        );
        assert!(questions[0].prompt.contains("git 仓"), "{:?}", questions[0]);
        assert_eq!(
            questions[0].options.len(),
            2,
            "with their own options: {:?}",
            questions[0]
        );
        // The header names which question it is, as the single path does.
        assert_eq!(questions[1].asker.as_deref(), Some("敏感信息"));

        // Answering one and declining the other: the first choice reaches the
        // tool, and the second is a decline rather than a made-up answer.
        let answered = declinable(Some(Reply::from("独立本体仓")));
        assert_eq!(answered["selected"][0], "独立本体仓");
        assert_eq!(answered["declined"], false);
        let declined = declinable(None);
        assert_eq!(
            declined["declined"], true,
            "skipping one question is not answering it: {declined}"
        );

        // A single question is not a batch — the tool sends those down the
        // other wire, and a one-element array on this one would draw an empty
        // card in the drivers that pick the single question off it.
        let one = serde_json::json!({ "questions": [two["questions"][0].clone()] });
        assert!(batch_for(REQUEST_USER_INPUT_KIND, &one, &[]).is_none());
        assert!(batch_for(APPROVAL_KIND, &two, &[]).is_none());
    }

    /// The kernel's own checkpoints are questions this screen can answer
    /// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A10).
    ///
    /// Both pause a turn and ask the *driver* whether to go on. A screen that
    /// does not know the kind answers nothing, and nothing means stop — so the
    /// turn ended and the person was never asked. Answering "continue" has to
    /// reach the kernel as `{"continue": true}`, and everything else as `false`,
    /// because that is what it degrades a missing answer to.
    #[test]
    fn the_kernels_own_checkpoints_are_asked_and_answered() {
        for kind in [
            atomcode_kernel::event::ROUND_CAP_CHECKPOINT_KIND,
            atomcode_kernel::event::OUTPUT_TRUNCATION_CHECKPOINT_KIND,
        ] {
            let question = q(kind, &serde_json::json!({}), &[])
                .unwrap_or_else(|| panic!("{kind} is a question a person can answer"));
            assert_eq!(
                question.options.len(),
                2,
                "two ways out, and both named: {question:?}"
            );
            assert_eq!(nth(&question, 1).as_deref(), Some(CONTINUE));
            assert_eq!(nth(&question, 2).as_deref(), Some(STOP));

            assert_eq!(
                response_for(kind, Some(Reply::from(CONTINUE))),
                serde_json::json!({ "continue": true }),
                "{kind}: continuing says so"
            );
            for answer in [Some(STOP.to_string()), None] {
                assert_eq!(
                    response_for(kind, answer.clone().map(Reply::from)),
                    serde_json::json!({ "continue": false }),
                    "{kind}: {answer:?} stops — nothing but an explicit yes goes on"
                );
            }
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
            None,
        );
        let payload = serde_json::json!({ "call_id": "c", "tool": "write_file", "args": r#"{"file_path":"a"}"# });
        let question = q(APPROVAL_KIND, &payload, &[asked(recorded.clone())]).expect("drawn");
        assert_eq!(question, recorded);
        assert!(!question.has(ANSWER_ALWAYS), "never offered what was not");

        for (answer, decision) in [
            (Some(ANSWER_ALLOW), "allow"),
            (Some(ANSWER_ALWAYS), "allow_always"),
            (Some(ANSWER_DENY), "deny"),
            (None, "deny"),
        ] {
            let value = response_for(APPROVAL_KIND, answer.map(Reply::from));
            assert_eq!(value["decision"], decision, "{answer:?}");
        }
    }

    /// The live modal reconciles against the RENAMED gate tool. A gate asks about
    /// `bash (writes outside the workspace)` and logs that question — with the
    /// allow-all option — while the wire request carries the raw call name `bash`.
    /// Matching on the executing BYTES (not the tool name) means the modal draws the
    /// recorded 4-option question instead of synthesizing a poorer 3-option one, so
    /// the scrollback card and the interactive panel show the same options.
    #[test]
    fn an_approval_reconciles_against_a_renamed_gate_tool_by_its_bytes() {
        let recorded = Question::approval(
            "bash (writes outside the workspace)",
            r#"{"command":"rm -rf build"}"#,
            Some(""),
            None,
            Some("bash"),
        );
        assert!(
            recorded.has(ANSWER_ALWAYS_ALL),
            "the gate offered allow-all"
        );
        // The wire request names the RAW tool, not the gate's renamed one.
        let payload = serde_json::json!({
            "call_id": "c",
            "tool": "bash",
            "args": r#"{"command":"rm -rf build"}"#,
        });
        let drawn = q(APPROVAL_KIND, &payload, &[asked(recorded.clone())]).expect("drawn");
        assert_eq!(
            drawn, recorded,
            "the recorded question, not a synthesized 3-option one"
        );
        assert!(
            drawn.has(ANSWER_ALWAYS_ALL),
            "the modal keeps the allow-all option"
        );
    }

    /// The session-wide "allow all Bash" blanket is offered only when the policy
    /// says the call is eligible (`allow_all = Some`), and it answers in the old
    /// front end's wire shape so a `PermissionDecision` parser reads `AllowAlwaysAll`.
    #[test]
    fn the_allow_all_blanket_is_offered_when_eligible_and_answers_in_the_old_wire_shape() {
        let none = Question::approval("bash", r#"{"command":"ls"}"#, Some(""), None, None);
        assert!(
            !none.has(ANSWER_ALWAYS_ALL),
            "not offered when the policy withheld it (sensitive / not a group)"
        );

        let offered = Question::approval(
            "bash",
            r#"{"command":"rm -rf x"}"#,
            Some(""),
            None,
            Some("bash"),
        );
        assert!(offered.has(ANSWER_ALWAYS_ALL), "offered when eligible");
        assert!(
            offered.has(ANSWER_ALLOW) && offered.has(ANSWER_ALWAYS) && offered.has(ANSWER_DENY)
        );

        // The blanket answer carries `remember + grant_scope:"all"` (the AllowAlwaysAll wire).
        let value = response_for(APPROVAL_KIND, Some(Reply::from(ANSWER_ALWAYS_ALL)));
        assert_eq!(value["decision"], "allow");
        assert_eq!(value["remember"], true);
        assert_eq!(value["grant_scope"], "all");
    }

    #[test]
    fn a_question_read_off_the_request_when_nothing_was_recorded() {
        let payload = serde_json::json!({
            "header": "Question · scout",
            "question": "Which one?",
            "mode": "single",
            "options": [{ "label": "vanilla" }, { "label": "pistachio", "description": "green" }],
        });
        let question = q(REQUEST_USER_INPUT_KIND, &payload, &[]).expect("drawn");
        assert_eq!(question.prompt, "Which one?");
        assert_eq!(question.values(), vec!["vanilla", "pistachio"]);
        assert_eq!(question.asker.as_deref(), Some("scout"));

        let picked = response_for(REQUEST_USER_INPUT_KIND, Some(Reply::from("pistachio")));
        let picked: UserInputResponse = serde_json::from_value(picked).unwrap();
        assert!(!picked.declined);
        assert_eq!(picked.selected, vec!["pistachio"]);
        let declined: UserInputResponse =
            serde_json::from_value(response_for(REQUEST_USER_INPUT_KIND, None)).unwrap();
        assert!(declined.declined);
    }

    /// A kind this screen has never been taught is still asked, and the answer
    /// gets back to whoever asked.
    ///
    /// The alternative is what shipped before: the screen answers `Null`, the
    /// tool reads that as "no driver can present this", and the model is told
    /// interactive questions are not supported — by a screen with a question
    /// panel on it. A new asker should have to put its question in the payload,
    /// not teach `ask.rs` about itself.
    #[test]
    fn a_kind_this_screen_never_heard_of_is_still_asked() {
        let payload = serde_json::json!({
            "prompt": "要不要把这条也带上?",
            "options": ["带上", { "value": "skip", "label": "跳过" }],
        });
        let question = q("some-future-capability", &payload, &[]).expect("drawn");
        assert_eq!(question.prompt, "要不要把这条也带上?");
        assert_eq!(question.values(), vec!["带上", "skip"]);
        assert_eq!(question.asker.as_deref(), Some("some-future-capability"));
        assert_eq!(
            response_for("some-future-capability", Some(Reply::from("skip"))),
            serde_json::json!("skip")
        );
        // Declined stays distinguishable from "nobody could ask".
        assert_eq!(response_for("some-future-capability", None), Value::Null);

        // No answers named: yes or no, so there is something to press.
        let bare = q("another", &serde_json::json!({ "message": "继续?" }), &[]).expect("drawn");
        assert_eq!(bare.values(), vec![YES, NO]);

        // And a payload with no words in it is genuinely not a question.
        assert!(q("another", &serde_json::json!({ "n": 1 }), &[]).is_none());
    }

    /// The question a request draws, for a test that only reads the words.
    fn q(kind: &str, payload: &Value, events: &[LoggedEvent]) -> Option<Question> {
        question_for(kind, payload, events).map(|a| a.question)
    }

    /// A model's own question, as the wire brings it.
    fn model(payload: Value) -> Asked {
        question_for(REQUEST_USER_INPUT_KIND, &payload, &[]).expect("drawn")
    }

    fn keys(sheet: &mut Sheet, keys: &[crate::surface::Key]) -> Step {
        let mut last = Step::Stay;
        for key in keys {
            last = sheet.key(crate::surface::KeyPress::plain(*key));
        }
        last
    }

    /// What reaches the model: the response the tool reads, parsed back.
    fn wire(step: Step) -> Vec<UserInputResponse> {
        let Step::Deliver(replies) = step else {
            panic!("nothing was delivered: {step:?}");
        };
        replies
            .into_iter()
            .map(|r| serde_json::from_value(declinable(r)).expect("the tool's own shape"))
            .collect()
    }

    fn single() -> Asked {
        model(serde_json::json!({
            "header": "口味",
            "question": "要哪个?",
            "mode": "single",
            "options": [{ "label": "vanilla" }, { "label": "pistachio", "description": "green" }],
        }))
    }

    fn multiple() -> Asked {
        model(serde_json::json!({
            "header": "语言",
            "question": "要支持哪些语言?",
            "mode": "multiple",
            "options": [{ "label": "Python" }, { "label": "Rust" }, { "label": "Go" }],
        }))
    }

    fn text() -> Asked {
        model(serde_json::json!({
            "header": "名字",
            "question": "新仓库叫什么?",
            "mode": "text",
        }))
    }

    /// The mode the model asked in survives the trip to the screen, and each
    /// answer keeps its own name — what it means is drawn under it, not in its
    /// place.
    #[test]
    fn a_models_question_keeps_the_mode_it_was_asked_in() {
        assert_eq!(single().form(), Form::Single);
        assert_eq!(multiple().form(), Form::Multiple);
        assert_eq!(text().form(), Form::Text);
        let one = single();
        assert_eq!(one.question.options[1].label, "pistachio");
        assert_eq!(one.description(1), Some("green"));
        assert_eq!(one.description(0), None);
        // The seam's own question, recorded in the log, is a choice as it was.
        let recorded = Question::plain("要哪个?", &["vanilla", "pistachio"]);
        let payload = serde_json::json!({
            "header": "Question", "question": "要哪个?", "mode": "single",
            "options": [{ "label": "vanilla" }, { "label": "pistachio" }],
        });
        let seam = question_for(REQUEST_USER_INPUT_KIND, &payload, &[asked(recorded)]).unwrap();
        assert_eq!(seam.form(), Form::Choice);
    }

    /// A single choice: an offered answer is sent as picked; the row of one's
    /// own sends the words, typed, as the tool's `text` — not as a pick of an
    /// answer that was never offered.
    #[test]
    fn a_single_choice_takes_a_pick_or_words_of_ones_own() {
        use crate::surface::Key;
        let mut sheet = Sheet::one(single());
        assert_eq!(
            sheet.slots(),
            vec![Slot::Pick(0), Slot::Pick(1), Slot::Other, Slot::Chat]
        );
        let picked = wire(keys(&mut sheet.clone(), &[Key::Down, Key::Enter]));
        assert_eq!(picked[0].selected, vec!["pistachio"]);
        assert_eq!(picked[0].text, None);

        // To the typing row by its number; enter on nothing typed is nothing.
        assert_eq!(keys(&mut sheet, &[Key::Char('3'), Key::Enter]), Step::Stay);
        assert!(sheet.typing());
        // Letters, digits and spaces are words there, not picks.
        let typed = keys(
            &mut sheet,
            &[
                Key::Char('m'),
                Key::Char('i'),
                Key::Char('n'),
                Key::Char('t'),
                Key::Char(' '),
                Key::Char('2'),
                Key::Char('x'),
                Key::Backspace,
                Key::Enter,
            ],
        );
        let answered = wire(typed);
        assert!(!answered[0].declined);
        assert!(answered[0].selected.is_empty(), "{answered:?}");
        assert_eq!(answered[0].text.as_deref(), Some("mint 2"));
    }

    /// A multiple choice sends everything ticked, and the words beside them —
    /// both, because the tool reads both.
    #[test]
    fn a_multiple_choice_sends_everything_ticked_and_the_words_beside_them() {
        use crate::surface::Key;
        let mut sheet = Sheet::one(multiple());
        assert_eq!(
            sheet.slots(),
            vec![
                Slot::Pick(0),
                Slot::Pick(1),
                Slot::Pick(2),
                Slot::Other,
                Slot::Submit,
                Slot::Chat
            ]
        );
        // Space ticks, and so does enter on an answer: sending is its own row.
        assert_eq!(
            keys(
                &mut sheet,
                &[Key::Char(' '), Key::Down, Key::Down, Key::Enter]
            ),
            Step::Stay
        );
        // Untick and tick again: a box is a toggle.
        keys(&mut sheet, &[Key::Char(' '), Key::Char(' ')]);
        let mut words = sheet.clone();
        let sent = wire(keys(&mut sheet, &[Key::Down, Key::Down, Key::Enter]));
        assert_eq!(sent[0].selected, vec!["Python", "Go"]);
        assert_eq!(sent[0].text, None);

        // With words on the row of one's own: enter there moves on to the row
        // that sends, and both halves go.
        keys(&mut words, &[Key::Down, Key::Char('C'), Key::Enter]);
        assert_eq!(words.pointed(), Some(Slot::Submit));
        let both = wire(words.enter());
        assert_eq!(both[0].selected, vec!["Python", "Go"]);
        assert_eq!(both[0].text.as_deref(), Some("C"));

        // Nothing ticked and nothing typed is not an answer — esc is for "none".
        let empty = Sheet::one(multiple());
        assert_eq!(
            empty.number(4),
            None,
            "the row that sends carries no number"
        );
        assert_eq!(
            empty.number(5),
            Some(5),
            "and the way out counts on past it"
        );
        let mut empty = Sheet::one(multiple());
        empty.point_at(4);
        assert_eq!(empty.enter(), Step::Stay);
    }

    /// A text question is words: no yes and no no, and nothing sent until
    /// something is typed.
    #[test]
    fn a_text_question_is_answered_with_words() {
        use crate::surface::Key;
        let asks = Asks::new();
        drop(asks.push(text()));
        let (_, waiting) = asks.peek().unwrap();
        assert!(
            waiting[0].question.options.is_empty(),
            "no yes/no put in front of a question that wants words"
        );
        let mut sheet = Sheet::one(waiting[0].clone());
        assert_eq!(sheet.slots(), vec![Slot::Input, Slot::Chat]);
        assert!(sheet.typing(), "the line to type on is lit from the start");
        assert_eq!(keys(&mut sheet, &[Key::Char(' '), Key::Enter]), Step::Stay);
        sheet.type_text("lab\nrepo");
        let sent = wire(sheet.enter());
        assert_eq!(sent[0].text.as_deref(), Some("lab repo"), "one line");
        assert!(sent[0].selected.is_empty());
    }

    /// Talking it over instead is an answer the model can act on: stop and
    /// listen — not "no answer, use your judgement", which is the guess the
    /// person just declined to let it make.
    #[test]
    fn chat_instead_tells_the_model_to_wait_for_the_person() {
        let mut sheet = Sheet::one(single());
        sheet.point_at(3);
        assert_eq!(sheet.pointed(), Some(Slot::Chat));
        let sent = wire(sheet.enter());
        assert!(!sent[0].declined);
        assert_eq!(sent[0].text.as_deref(), Some(CHAT_INSTEAD));

        let mut batch = Sheet::new(1, vec![single(), text()]);
        let sent = wire(keys(&mut batch, &[crate::surface::Key::Char('4')]));
        assert_eq!(sent.len(), 2, "the whole batch, not one page of it");
        assert!(sent.iter().all(|r| r.text.as_deref() == Some(CHAT_INSTEAD)));
    }

    /// A batch is answered page by page and sent from its review page, every
    /// answer in the order asked; a page skipped is that question declined, and
    /// cancelling or esc declines them all.
    #[test]
    fn a_batch_is_answered_page_by_page_and_sent_from_the_review_page() {
        use crate::surface::Key;
        let mut sheet = Sheet::new(9, vec![single(), multiple(), text()]);
        // Page one: pick pistachio — the page turns by itself.
        assert_eq!(keys(&mut sheet, &[Key::Char('2')]), Step::Stay);
        assert_eq!(sheet.tab, 1);
        // Page two: skip it. Page three: type.
        keys(&mut sheet, &[Key::Tab]);
        assert_eq!(sheet.tab, 2);
        sheet.type_text("lab");
        assert_eq!(keys(&mut sheet, &[Key::Enter]), Step::Stay);
        assert!(
            sheet.reviewing(),
            "the last answer leads to the review page"
        );
        assert_eq!(sheet.recap(0).as_deref(), Some("pistachio"));
        assert_eq!(sheet.recap(1), None);
        assert_eq!(sheet.recap(2).as_deref(), Some("lab"));

        // Back to page two (Tab, since page three has words the arrows would move
        // through) and answer it after all; the pages keep what they had.
        keys(
            &mut sheet,
            &[Key::BackTab, Key::BackTab, Key::Char(' '), Key::Char('1')],
        );
        assert_eq!(sheet.tab, 1);
        assert_eq!(
            sheet.drafts[1].checked,
            vec![false, false, false],
            "ticked, unticked"
        );
        keys(&mut sheet, &[Key::Char('2'), Key::Char('3')]);
        sheet.point_at(4);
        keys(&mut sheet, &[Key::Enter]);
        assert!(sheet.reviewing());

        let mut cancel = sheet.clone();
        let sent = wire(keys(&mut sheet, &[Key::Char('1')]));
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].selected, vec!["pistachio"]);
        assert_eq!(sent[1].selected, vec!["Rust", "Go"]);
        assert_eq!(sent[2].text.as_deref(), Some("lab"));

        assert!(wire(keys(&mut cancel, &[Key::Down, Key::Enter]))
            .iter()
            .all(|r| r.declined));
        let mut skipped = Sheet::new(9, vec![single(), text()]);
        let sent = wire(keys(&mut skipped, &[Key::Right, Key::Right, Key::Enter]));
        assert!(
            sent.iter().all(|r| r.declined),
            "nothing answered, nothing made up"
        );
        let mut esc = Sheet::new(9, vec![single(), text()]);
        keys(&mut esc, &[Key::Char('1')]);
        let sent = wire(keys(&mut esc, &[Key::Esc]));
        assert!(sent.iter().all(|r| r.declined), "esc is the whole request");
    }

    /// A batch pushed through the queue comes back as one answer per question,
    /// in order — and shutdown refuses every one of them.
    #[tokio::test]
    async fn a_batch_comes_back_whole_through_the_queue() {
        let asks = Asks::new();
        let answer = asks.push_batch(vec![single(), text()]);
        let (id, asked) = asks.peek().unwrap();
        assert_eq!(asked.len(), 2);
        assert!(
            asks.take_id(id + 1).is_none(),
            "an answer is not transferable"
        );
        asks.take_id(id)
            .unwrap()
            .finish(vec![Some(Reply::from("vanilla"))]);
        assert_eq!(
            answer.await.unwrap(),
            vec![Some(Reply::from("vanilla")), None],
            "a question left off the end is declined"
        );

        let refused = asks.push_batch(vec![single(), text()]);
        asks.refuse_all();
        assert_eq!(refused.await.unwrap(), vec![None, None]);

        // Without a panel, the fallback answers a batch one question at a time.
        let answer = asks.push_batch(vec![single(), text()]);
        assert_eq!(asks.current().unwrap(), single());
        asks.answer_current(Some(Reply::from("pistachio")));
        assert_eq!(asks.current().unwrap(), text());
        asks.answer_current(None);
        assert!(!asks.is_waiting());
        assert_eq!(
            answer.await.unwrap(),
            vec![Some(Reply::from("pistachio")), None]
        );
    }

    fn typed(sheet: &Sheet) -> (String, usize) {
        let d = sheet.draft().expect("a question page");
        (d.typed.clone(), d.caret)
    }

    /// A typing row is edited where its caret is: arrows move it a character
    /// at a time, Home/End go to the ends, backspace and Delete take out the
    /// character before and under it, and typing goes in at it.
    #[test]
    fn a_typing_row_is_edited_at_its_caret() {
        use crate::surface::Key;
        let mut sheet = Sheet::one(text());
        sheet.type_text("helo");
        assert_eq!(typed(&sheet), ("helo".into(), 4));
        keys(&mut sheet, &[Key::Left, Key::Char('l')]);
        assert_eq!(typed(&sheet), ("hello".into(), 4), "typed in at the caret");
        keys(
            &mut sheet,
            &[Key::Home, Key::Char('>'), Key::Right, Key::Delete],
        );
        assert_eq!(typed(&sheet), (">hllo".into(), 2));
        keys(&mut sheet, &[Key::Backspace]);
        assert_eq!(
            typed(&sheet),
            (">llo".into(), 1),
            "backspace takes the one before"
        );
        keys(&mut sheet, &[Key::End, Key::Right, Key::Backspace]);
        assert_eq!(typed(&sheet), (">ll".into(), 3), "past the end is the end");
        keys(&mut sheet, &[Key::Home, Key::Left, Key::Backspace]);
        assert_eq!(
            typed(&sheet),
            (">ll".into(), 0),
            "before the start is the start"
        );
        // A paste goes in at the caret too, whole.
        sheet.type_text("a b");
        assert_eq!(typed(&sheet), ("a b>ll".into(), 3));
        assert_eq!(wire(sheet.enter())[0].text.as_deref(), Some("a b>ll"));
    }

    /// Wide characters are one step each, however many bytes and cells they
    /// take — a caret that stepped a byte would land inside one and panic.
    #[test]
    fn the_caret_steps_over_wide_characters_whole() {
        use crate::surface::Key;
        let mut sheet = Sheet::one(text());
        sheet.type_text("中文字");
        keys(&mut sheet, &[Key::Left]);
        assert_eq!(typed(&sheet), ("中文字".into(), 6));
        keys(&mut sheet, &[Key::Char('a')]);
        assert_eq!(typed(&sheet), ("中文a字".into(), 7));
        keys(&mut sheet, &[Key::Left, Key::Left, Key::Backspace]);
        assert_eq!(typed(&sheet), ("文a字".into(), 0));
        keys(&mut sheet, &[Key::Delete, Key::End, Key::Backspace]);
        assert_eq!(typed(&sheet), ("a".into(), 1));
    }

    /// Left and right move the caret only on a line with words in it. Off the
    /// typing row, or on an empty one, they turn a batch's pages as before —
    /// and Tab turns them from anywhere.
    #[test]
    fn the_arrows_turn_pages_unless_there_are_words_to_move_through() {
        use crate::surface::Key;
        let mut batch = Sheet::new(1, vec![text(), single()]);
        assert!(batch.typing() && !batch.editing());
        keys(&mut batch, &[Key::Right]);
        assert_eq!(batch.tab, 1, "an empty line: the arrow turns the page");
        keys(&mut batch, &[Key::Left]);
        assert_eq!(batch.tab, 0);
        batch.type_text("ab");
        keys(&mut batch, &[Key::Left, Key::Right, Key::Left]);
        assert_eq!(
            batch.tab, 0,
            "words to move through: the arrows stay on the page"
        );
        assert_eq!(typed(&batch), ("ab".into(), 1));
        keys(&mut batch, &[Key::Tab]);
        assert_eq!(batch.tab, 1, "Tab still turns it");
        keys(&mut batch, &[Key::Right]);
        assert_eq!(batch.tab, 2, "off the typing row the arrows turn pages");
        keys(&mut batch, &[Key::BackTab, Key::BackTab]);
        assert_eq!(batch.tab, 0);
        assert_eq!(
            typed(&batch),
            ("ab".into(), 1),
            "and the page kept its caret"
        );
    }
}
