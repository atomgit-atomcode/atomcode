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

pub fn batch_for(kind: &str, payload: &Value, events: &[LoggedEvent]) -> Option<Vec<Question>> {
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
pub fn declinable(question: &Question, answer: Option<String>) -> Value {
    response_for(REQUEST_USER_INPUT_KIND, question, answer)
}

/// What the two kernel checkpoints are answered with. Their own words rather
/// than `allow` / `deny`: this is not an approval, and reusing those would put
/// "允许" on a question about whether to keep going.
pub const CONTINUE: &str = "continue";
pub const STOP: &str = "stop";

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
        let questions = batch_for(REQUEST_USER_INPUT_KIND, &two, &[])
            .expect("a batch is a batch, not an unreadable payload");
        assert_eq!(questions.len(), 2, "both are put to the person");
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
        let answered = declinable(&questions[0], Some("独立本体仓".into()));
        assert_eq!(answered["selected"][0], "独立本体仓");
        assert_eq!(answered["declined"], false);
        let declined = declinable(&questions[1], None);
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
            let question = question_for(kind, &serde_json::json!({}), &[])
                .unwrap_or_else(|| panic!("{kind} is a question a person can answer"));
            assert_eq!(
                question.options.len(),
                2,
                "two ways out, and both named: {question:?}"
            );
            assert_eq!(nth(&question, 1).as_deref(), Some(CONTINUE));
            assert_eq!(nth(&question, 2).as_deref(), Some(STOP));

            assert_eq!(
                response_for(kind, &question, Some(CONTINUE.into())),
                serde_json::json!({ "continue": true }),
                "{kind}: continuing says so"
            );
            for answer in [Some(STOP.to_string()), None] {
                assert_eq!(
                    response_for(kind, &question, answer.clone()),
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
        let question = question_for("some-future-capability", &payload, &[]).expect("drawn");
        assert_eq!(question.prompt, "要不要把这条也带上?");
        assert_eq!(question.values(), vec!["带上", "skip"]);
        assert_eq!(question.asker.as_deref(), Some("some-future-capability"));
        assert_eq!(
            response_for("some-future-capability", &question, Some("skip".into())),
            serde_json::json!("skip")
        );
        // Declined stays distinguishable from "nobody could ask".
        assert_eq!(
            response_for("some-future-capability", &question, None),
            Value::Null
        );

        // No answers named: yes or no, so there is something to press.
        let bare = question_for("another", &serde_json::json!({ "message": "继续?" }), &[])
            .expect("drawn");
        assert_eq!(bare.values(), vec![YES, NO]);

        // And a payload with no words in it is genuinely not a question.
        assert!(question_for("another", &serde_json::json!({ "n": 1 }), &[]).is_none());
    }
}
