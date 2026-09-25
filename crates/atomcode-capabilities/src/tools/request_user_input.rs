//! The `request_user_input` tool: the model poses ONE structured question
//! (single / multiple / text); the turn pauses; the user answers in the driver UI;
//! the answer returns as the tool result. Rides the generic kernel request/respond
//! round-trip via `ToolContext::request`. Types are defined here for drivers to import.

use async_trait::async_trait;
use atomcode_kernel::message::ImageContent;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};

/// The `kind` string for the generic driver round-trip carrying a user-input request.
pub const REQUEST_USER_INPUT_KIND: &str = "request_user_input";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserInputMode {
    Single,
    Multiple,
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInputOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInputRequest {
    /// The question itself — the one thing a caller must supply. Declared before `header`
    /// so that a call carrying neither is reported as missing `question`: `header` is only
    /// a label derived from this text (see [`fill_default_header`]), and naming it would
    /// send the model off to fix a field it never needed to send.
    pub question: String,
    /// The short label drawn above the question. Still offered as required to the model,
    /// but repaired from the question when one is dropped (see [`fill_default_header`]).
    pub header: String,
    pub mode: UserInputMode,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
    /// Legacy wire field retained for backward-compatible deserialization.
    /// Model-authored requests are normalized to true so choice questions
    /// always offer a free-form answer.
    #[serde(default = "default_true")]
    pub custom: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct UserInputResponse {
    pub declined: bool,
    #[serde(default)]
    pub selected: Vec<String>, // single: len<=1; multiple: 0..N (labels)
    #[serde(default)]
    pub text: Option<String>, // text mode
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageContent>,
}

impl UserInputResponse {
    pub fn declined() -> Self {
        Self {
            declined: true,
            ..Default::default()
        }
    }
}

/// Max questions a single batch may pose.
pub const MAX_QUESTIONS: usize = 4;

fn validate_question(req: &UserInputRequest) -> Result<(), String> {
    if matches!(req.mode, UserInputMode::Single | UserInputMode::Multiple) && req.options.is_empty()
    {
        return Err(
            "request_user_input: single/multiple mode requires a non-empty `options` array".into(),
        );
    }
    Ok(())
}

/// Fill in a default `mode` when the model omits it — weak models frequently drop this
/// required field and the call would otherwise hard-fail with `missing field 'mode'`.
/// Infer intent from `options`: a non-empty `options` array means a choice (`single`),
/// otherwise free-form (`text`). A `mode` that is already present (even explicit `text`
/// alongside options) is left untouched. No-op on a non-object value.
fn fill_default_mode(value: &mut serde_json::Value) {
    let serde_json::Value::Object(map) = value else {
        return;
    };
    let missing = map
        .get("mode")
        .map(serde_json::Value::is_null)
        .unwrap_or(true);
    if !missing {
        return;
    }
    let has_options = map
        .get("options")
        .and_then(serde_json::Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let inferred = if has_options { "single" } else { "text" };
    map.insert("mode".into(), serde_json::Value::String(inferred.into()));
}

/// How many characters of a question become its derived `header`.
const HEADER_MAX_CHARS: usize = 24;

/// The short label for a question that arrived without one: its first non-blank line,
/// trimmed and cut to `HEADER_MAX_CHARS` **characters** — not bytes, so a Chinese question
/// is never split mid-glyph — with `…` marking the cut.
///
/// `None` when there is no question text to derive one from: then the missing `question`
/// is the field worth reporting, not `header`.
fn derive_header(question: &str) -> Option<String> {
    let first = question
        .lines()
        .find(|line| !line.trim().is_empty())?
        .trim();
    if first.is_empty() {
        return None;
    }
    let mut label: String = first.chars().take(HEADER_MAX_CHARS).collect();
    if first.chars().count() > HEADER_MAX_CHARS {
        label.push('…');
    }
    Some(label)
}

/// Fill in a default `header` when the model omits it, sends `null`, or sends something
/// that is not a usable non-blank string. Like `mode`, this is a required field weak models
/// drop — and the call would otherwise hard-fail with `missing field 'header'`, even though
/// `header` is only the short label the panel draws above the question. Derived from the
/// question (see [`derive_header`]). A usable `header` is left untouched, as is a payload
/// with no question text to derive one from. No-op on a non-object value.
fn fill_default_header(value: &mut serde_json::Value) {
    let serde_json::Value::Object(map) = value else {
        return;
    };
    let usable = map
        .get("header")
        .and_then(serde_json::Value::as_str)
        .map(|header| !header.trim().is_empty())
        .unwrap_or(false);
    if usable {
        return;
    }
    let label = map
        .get("question")
        .and_then(serde_json::Value::as_str)
        .and_then(derive_header);
    if let Some(label) = label {
        map.insert("header".into(), serde_json::Value::String(label));
    }
}

/// Repair the fields a weak model frequently drops, on the raw JSON before deserialization:
/// `mode` (inferred from `options`) and `header` (derived from the question). A call that
/// would have hard-failed becomes a usable one. Both the single and the batch path funnel
/// through here, so a question is repaired the same way wherever it arrived.
fn fill_defaults(value: &mut serde_json::Value) {
    fill_default_mode(value);
    fill_default_header(value);
}

/// Parse raw tool args into a `UserInputRequest`. A missing `mode` is inferred from
/// `options` (see [`fill_default_mode`]) and a missing `header` derived from the question
/// (see [`fill_default_header`]); choice modes with no options are rejected.
/// Returns a human message on failure (never panics).
pub fn parse_args(args: &str) -> Result<UserInputRequest, String> {
    let mut value: serde_json::Value = serde_json::from_str(args)
        .map_err(|e| format!("invalid request_user_input arguments: {e}"))?;
    fill_defaults(&mut value);
    let mut req: UserInputRequest = serde_json::from_value(value)
        .map_err(|e| format!("invalid request_user_input arguments: {e}"))?;
    // Keep accepting the legacy field for wire compatibility, but a
    // human-facing question must always leave the user a free-form escape hatch.
    req.custom = true;
    validate_question(&req)?;
    Ok(req)
}

/// Parse args into 1..=`MAX_QUESTIONS` questions. Accepts a `{ "questions": [...] }`
/// array (batch) or the flat single-question shape (legacy). The bool is `is_batch`
/// — the caller uses it to pick the wire shape. Clamps a batch to `MAX_QUESTIONS`.
/// Every question is repaired the same way [`parse_args`] repairs a lone one.
pub fn parse_batch(args: &str) -> Result<(Vec<UserInputRequest>, bool), String> {
    let val: serde_json::Value = serde_json::from_str(args)
        .map_err(|e| format!("invalid request_user_input arguments: {e}"))?;
    if let Some(qs) = val.get("questions").and_then(serde_json::Value::as_array) {
        if qs.is_empty() {
            return Err("request_user_input: `questions` must be a non-empty array".into());
        }
        let mut out = Vec::new();
        for q in qs.iter().take(MAX_QUESTIONS) {
            let mut q = q.clone();
            fill_defaults(&mut q);
            let mut req: UserInputRequest = serde_json::from_value(q)
                .map_err(|e| format!("invalid question in `questions`: {e}"))?;
            req.custom = true;
            validate_question(&req)?;
            out.push(req);
        }
        // A 1-element `questions` array is NOT a batch: send it down the single-question
        // wire so both drivers render the populated question. (A batch payload carries no
        // top-level header/question, so a driver that picks the single card off `len==1`
        // — e.g. the webui — would otherwise show an empty card.)
        let is_batch = out.len() > 1;
        Ok((out, is_batch))
    } else {
        Ok((vec![parse_args(args)?], false))
    }
}

/// Summarize a non-declined answer (shared by the single + batch paths).
///
/// `selected` and `text` are NOT mutually exclusive: in `multiple` mode a driver can let the
/// user tick options AND type a custom note, and both come back on the wire. Returning early
/// on `text` dropped every ticked option before the model ever saw it — the user picked
/// "Python" and added "plus Rust", and the model was only ever told about "plus Rust".
fn answer_summary(resp: &UserInputResponse) -> String {
    let text = resp.text.as_deref().filter(|t| !t.trim().is_empty());
    let selected = (!resp.selected.is_empty()).then(|| {
        resp.selected
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    });
    let mut summary = match (selected, text) {
        (Some(sel), Some(t)) => format!("User selected: {sel}, and User answered: {t:?}"),
        (Some(sel), None) => format!("User selected: {sel}"),
        (None, Some(t)) => format!("User answered: {t:?}"),
        // Nothing ticked and nothing typed. A text-mode submission that is present but
        // blank keeps its historical shape; a wholly absent answer reads as no selection.
        (None, None) => match &resp.text {
            Some(t) => format!("User answered: {t:?}"),
            None => "User selected nothing.".to_string(),
        },
    };
    if !resp.images.is_empty() {
        let noun = if resp.images.len() == 1 {
            "image"
        } else {
            "images"
        };
        summary.push_str(&format!(", and User attached {} {noun}", resp.images.len()));
    }
    summary
}

/// Map one question's response to its answer clause (shared by single + batch).
fn answer_clause(resp: &UserInputResponse) -> String {
    if resp.declined {
        return "No answer (declined).".to_string();
    }
    answer_summary(resp)
}

/// Format a batch of answers, one line per question keyed by its `header`. When every
/// question was declined, degrade to the same "no answer" guidance a single decline gives.
pub fn format_batch_result(reqs: &[UserInputRequest], resps: &[UserInputResponse]) -> ToolResult {
    if resps.len() >= reqs.len() && resps.iter().all(|r| r.declined) {
        return ok_result(NO_ANSWER);
    }
    let lines: Vec<String> = reqs
        .iter()
        .enumerate()
        .map(|(i, req)| {
            let clause = resps
                .get(i)
                .map(answer_clause)
                .unwrap_or_else(|| "No answer (declined).".to_string());
            format!("Q{} ({}): {}", i + 1, req.header, clause)
        })
        .collect();
    ToolResult {
        call_id: String::new(),
        content: lines.join("\n"),
        is_error: false,
        images: resps
            .iter()
            .flat_map(|r| r.images.iter().cloned())
            .collect(),
    }
}

fn err_result(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: msg.into(),
        is_error: true,
        images: vec![],
    }
}

fn ok_result(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: msg.into(),
        is_error: false,
        images: vec![],
    }
}

/// What the model is told when nobody answered — one question or all of a batch.
pub const NO_ANSWER: &str = "No answer was provided. Proceed with your own best judgment; \
                             only ask again if you are truly blocked.";

/// One question's answer, as a result read back says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadAnswer {
    /// Closed without an answer: declined, or nobody there.
    Declined,
    /// What was picked and what was typed. `text` is `None` when nothing was typed.
    Given {
        selected: Vec<String>,
        text: Option<String>,
        images: usize,
    },
}

/// The answers a result carries, one per question asked, read back from the text
/// [`format_result`] / [`format_batch_result`] wrote for the model.
///
/// The log keeps no other record of them: a model's question goes over the request
/// round-trip, not the `user-questions` seam, so no `Asked`/`Answered` fact is ever
/// committed for it — the result text IS the answer, in a resumed session as much
/// as a live one. Read here, beside the code that writes it, so a change to the
/// wording breaks the round-trip criterion below instead of a screen that parses
/// English it does not own.
///
/// `questions` is how many were asked (see [`parse_batch`]). `None` for any text
/// these writers did not produce — an error, a result from an older wording — and
/// the caller then shows the text as it is.
pub fn read_result(content: &str, questions: usize) -> Option<Vec<ReadAnswer>> {
    if questions == 0 {
        return None;
    }
    if content == NO_ANSWER {
        return Some(vec![ReadAnswer::Declined; questions]);
    }
    if questions == 1 {
        return read_clause(content).map(|one| vec![one]);
    }
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.len() != questions {
        return None;
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let rest = line.strip_prefix(&format!("Q{} (", i + 1))?;
            // The header is the asker's own words and may hold `): ` itself; the
            // clause is the first split that reads as one.
            rest.match_indices("): ")
                .find_map(|(at, sep)| read_clause(&rest[at + sep.len()..]))
        })
        .collect()
}

/// One answer clause, the inverse of [`answer_clause`].
fn read_clause(clause: &str) -> Option<ReadAnswer> {
    if clause == "No answer (declined)." {
        return Some(ReadAnswer::Declined);
    }
    let (said, images) = match clause.rsplit_once(", and User attached ") {
        Some((said, count)) => match count
            .strip_suffix(" images")
            .or_else(|| count.strip_suffix(" image"))
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) => (said, n),
            // The words were inside a quoted answer, not the suffix.
            None => (clause, 0),
        },
        None => (clause, 0),
    };
    if said == "User selected nothing." {
        return Some(ReadAnswer::Given {
            selected: Vec::new(),
            text: None,
            images,
        });
    }
    let typed = |rest: &str| -> Option<String> {
        let (text, after) = read_quoted(rest)?;
        after.is_empty().then_some(text)
    };
    if let Some(rest) = said.strip_prefix("User answered: ") {
        return Some(ReadAnswer::Given {
            selected: Vec::new(),
            text: Some(typed(rest)?),
            images,
        });
    }
    let mut rest = said.strip_prefix("User selected: ")?;
    let mut selected = Vec::new();
    loop {
        let (label, after) = read_quoted(rest)?;
        selected.push(label);
        if after.is_empty() {
            return Some(ReadAnswer::Given {
                selected,
                text: None,
                images,
            });
        }
        if let Some(text) = after.strip_prefix(", and User answered: ") {
            return Some(ReadAnswer::Given {
                selected,
                text: Some(typed(text)?),
                images,
            });
        }
        rest = after.strip_prefix(", ")?;
    }
}

/// A string as `{:?}` wrote it, and what follows it.
fn read_quoted(s: &str) -> Option<(String, &str)> {
    let mut chars = s.strip_prefix('"')?.char_indices();
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &s[1 + i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '0' => out.push('\0'),
                'u' => {
                    let mut hex = String::new();
                    if chars.next()?.1 != '{' {
                        return None;
                    }
                    loop {
                        match chars.next()?.1 {
                            '}' => break,
                            h => hex.push(h),
                        }
                    }
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// Map the user's answer to a tool result string.
pub fn format_result(resp: &UserInputResponse) -> ToolResult {
    if resp.declined {
        return ok_result(NO_ANSWER);
    }
    ToolResult {
        call_id: String::new(),
        content: answer_summary(resp),
        is_error: false,
        images: resp.images.clone(),
    }
}

/// Result when no driver can present the question (Null round-trip / missing requester).
pub fn null_result() -> ToolResult {
    err_result("Interactive questions are not supported in this environment.")
}

pub struct RequestUserInputTool;

#[async_trait]
impl Tool for RequestUserInputTool {
    fn name(&self) -> &str {
        "request_user_input"
    }

    fn description(&self) -> &str {
        "Ask the user structured question(s) and wait for their answer before continuing. \
         Use ONLY for decisions that are genuinely the user's to make — a preference, a \
         confirmation, a choice between approaches — NOT for anything you can decide, look \
         up, or verify yourself. When the user explicitly asks you to recommend, compare, or \
         offer choices for THEM to pick/select from (e.g. \"recommend a few X for me to \
         choose\", \"let me pick one\"), surface the concrete options HERE (set \
         `mode`=\"multiple\" when they may want to select several) instead of writing the \
         list as prose. For ONE question, set `header`, `question`, and `options` \
         (non-empty for a choice question); `mode` is optional — omit it and it is inferred \
         (\"single\" when `options` is given, else \"text\"), but set it explicitly to \
         \"multiple\" when the user may pick several (or \"text\" to force free-form). To ask \
         up to 4 related questions answered in ONE \
         interaction, pass a `questions` array of those same objects instead. A free-text \
         \"type your own answer\" row is always added automatically for single/multiple, so do \
         NOT add your own \"Other\"/catch-all option. Keep each \
         `header` short (a few words)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let question = serde_json::json!({
            "type": "object",
            "required": ["header", "question"],
            "properties": {
                "header": {"type": "string", "description": "Very short label (a few words)."},
                "question": {"type": "string", "description": "One clear sentence, ideally ending in '?'."},
                "mode": {"type": "string", "enum": ["single", "multiple", "text"], "description": "Optional; if omitted, defaults to \"single\" when `options` is non-empty, else \"text\"."},
                "options": {
                    "type": "array",
                    "description": "Choices for single/multiple; omit for text.",
                    "items": {
                        "type": "object",
                        "required": ["label"],
                        "properties": {
                            "label": {"type": "string"},
                            "description": {"type": "string"}
                        }
                    }
                }
            }
        });
        serde_json::json!({
            "type": "object",
            "properties": {
                "header": question["properties"]["header"],
                "question": question["properties"]["question"],
                "mode": question["properties"]["mode"],
                "options": question["properties"]["options"],
                "questions": {
                    "type": "array",
                    "description": "Up to 4 questions answered in one interaction. Provide EITHER top-level header/question/mode/options for a single question, OR this array.",
                    "maxItems": 4,
                    "items": question
                }
            }
        })
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let (reqs, is_batch) = match parse_batch(args) {
            Ok(x) => x,
            Err(e) => return err_result(e),
        };
        if !is_batch {
            // Legacy single-question path — wire + result unchanged.
            let payload = match serde_json::to_value(&reqs[0]) {
                Ok(v) => v,
                Err(e) => return err_result(format!("request_user_input: serialize failed: {e}")),
            };
            let resp_val = ctx.request(REQUEST_USER_INPUT_KIND, payload).await;
            if resp_val.is_null() {
                return null_result();
            }
            return match serde_json::from_value::<UserInputResponse>(resp_val) {
                Ok(resp) => format_result(&resp),
                Err(_) => format_result(&UserInputResponse::declined()),
            };
        }
        // Batch path.
        let payload = serde_json::json!({ "questions": reqs });
        let resp_val = ctx.request(REQUEST_USER_INPUT_KIND, payload).await;
        if resp_val.is_null() {
            return null_result();
        }
        let resps: Vec<UserInputResponse> = resp_val
            .get("responses")
            .and_then(|r| serde_json::from_value::<Vec<UserInputResponse>>(r.clone()).ok())
            .unwrap_or_default();
        format_batch_result(&reqs, &resps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_choice_without_options() {
        assert!(
            parse_args(r#"{"header":"H","question":"Q?","mode":"single","options":[]}"#).is_err()
        );
    }

    #[test]
    fn parse_text_ignores_options() {
        let r = parse_args(r#"{"header":"H","question":"Q?","mode":"text"}"#).unwrap();
        assert_eq!(r.mode, UserInputMode::Text);
    }

    #[test]
    fn omitted_mode_with_options_infers_single() {
        // Weak models drop the required `mode`; a call carrying `options` clearly wants
        // a choice → infer `single` instead of hard-failing with `missing field 'mode'`.
        let r =
            parse_args(r#"{"header":"H","question":"Q?","options":[{"label":"A"},{"label":"B"}]}"#)
                .unwrap();
        assert_eq!(r.mode, UserInputMode::Single);
        assert_eq!(r.options.len(), 2);
    }

    #[test]
    fn omitted_mode_without_options_infers_text() {
        let r = parse_args(r#"{"header":"H","question":"Q?"}"#).unwrap();
        assert_eq!(r.mode, UserInputMode::Text);
    }

    /// A required field weak models drop, in the flat shape: `header` is only the label the
    /// panel draws above the question, so derive one rather than hard-failing the call.
    #[test]
    fn omitted_header_is_derived_from_the_question() {
        let r = parse_args(r#"{"question":"Which database?"}"#).unwrap();
        assert_eq!(r.header, "Which database?");
        assert_eq!(r.mode, UserInputMode::Text);
    }

    /// The same repair in the batch shape, per question. A batch used to lose every one of
    /// its questions to `invalid question in questions: missing field header`.
    #[test]
    fn batch_questions_without_headers_get_one_each() {
        let (reqs, is_batch) = parse_batch(
            r#"{"questions":[
                {"question":"First one?","options":[{"label":"A"}]},
                {"header":"Kept","question":"Second one?"}
            ]}"#,
        )
        .unwrap();
        assert!(is_batch);
        assert_eq!(reqs[0].header, "First one?");
        assert_eq!(reqs[1].header, "Kept", "a usable header is left alone");
        assert_eq!(reqs[0].mode, UserInputMode::Single);
    }

    /// Blank, null and non-string: all three are "no header", because a question labelled
    /// `7` has no label at all.
    #[test]
    fn unusable_header_is_replaced_by_the_derived_one() {
        for args in [
            r#"{"header":"   ","question":"Q?"}"#,
            r#"{"header":null,"question":"Q?"}"#,
            r#"{"header":7,"question":"Q?"}"#,
        ] {
            let r = parse_args(args).unwrap_or_else(|e| panic!("{args}: {e}"));
            assert_eq!(r.header, "Q?", "{args}");
        }
    }

    #[test]
    fn usable_header_is_never_overridden() {
        let r = parse_args(r#"{"header":"Auth","question":"A much longer question?"}"#).unwrap();
        assert_eq!(r.header, "Auth");
    }

    /// The cut counts characters, so a Chinese question is never split mid-glyph.
    #[test]
    fn derived_header_cuts_on_char_boundaries() {
        let question = "这是一个非常长的中文问题需要被截断成一个短标签而且不能把任何一个汉字切开?";
        let r = parse_args(&format!(r#"{{"question":"{question}"}}"#)).unwrap();
        assert_eq!(
            r.header.chars().count(),
            HEADER_MAX_CHARS + 1,
            "{:?}",
            r.header
        );
        assert!(r.header.ends_with('…'), "{:?}", r.header);
    }

    /// Nothing to derive from: the missing `question` is what the caller must hear about,
    /// not the `header` that was going to be built out of it.
    #[test]
    fn without_a_question_the_missing_header_is_not_what_is_reported() {
        let err = parse_args(r#"{"mode":"text"}"#).unwrap_err();
        assert!(err.contains("question"), "{err}");
        assert!(!err.contains("header"), "{err}");
    }

    #[test]
    fn explicit_mode_is_never_overridden_by_inference() {
        // Explicit `text` alongside options stays text (options are ignored per
        // `parse_text_ignores_options`), and null is treated as omitted.
        let r =
            parse_args(r#"{"header":"H","question":"Q?","mode":"text","options":[{"label":"A"}]}"#)
                .unwrap();
        assert_eq!(r.mode, UserInputMode::Text);
        let r =
            parse_args(r#"{"header":"H","question":"Q?","mode":null,"options":[{"label":"A"}]}"#)
                .unwrap();
        assert_eq!(r.mode, UserInputMode::Single);
    }

    #[test]
    fn batch_infers_mode_per_question() {
        let (reqs, is_batch) = parse_batch(
            r#"{"questions":[
                {"header":"A","question":"Q1?","options":[{"label":"x"}]},
                {"header":"B","question":"Q2?"}
            ]}"#,
        )
        .unwrap();
        assert!(is_batch);
        assert_eq!(reqs[0].mode, UserInputMode::Single);
        assert_eq!(reqs[1].mode, UserInputMode::Text);
    }

    #[test]
    fn parse_single_ok() {
        let r = parse_args(
            r#"{"header":"Auth","question":"Which?","mode":"single","options":[{"label":"OAuth"}]}"#,
        )
        .unwrap();
        assert_eq!(r.options.len(), 1);
    }

    #[test]
    fn format_single() {
        let r = format_result(&UserInputResponse {
            declined: false,
            selected: vec!["OAuth".into()],
            text: None,
            images: vec![],
        });
        assert_eq!(r.content, r#"User selected: "OAuth""#);
        assert!(!r.is_error);
    }

    #[test]
    fn format_multiple_and_empty() {
        assert_eq!(
            format_result(&UserInputResponse {
                declined: false,
                selected: vec!["A".into(), "B".into()],
                text: None,
                images: vec![],
            })
            .content,
            r#"User selected: "A", "B""#
        );
        assert_eq!(
            format_result(&UserInputResponse {
                declined: false,
                selected: vec![],
                text: None,
                images: vec![],
            })
            .content,
            "User selected nothing."
        );
    }

    #[test]
    fn format_text_and_declined() {
        assert_eq!(
            format_result(&UserInputResponse {
                declined: false,
                selected: vec![],
                text: Some("hi".into()),
                images: vec![],
            })
            .content,
            r#"User answered: "hi""#
        );
        let d = format_result(&UserInputResponse::declined());
        assert!(
            !d.is_error,
            "declined must not be an error — model should proceed, not retry/abort"
        );
        assert_eq!(
            d.content,
            "No answer was provided. Proceed with your own best judgment; only ask again if you \
             are truly blocked.",
        );
    }

    #[test]
    fn parse_batch_reads_questions_array_and_clamps_to_four() {
        let args = r#"{"questions":[
            {"header":"A","question":"Q1?","mode":"single","options":[{"label":"x"}]},
            {"header":"B","question":"Q2?","mode":"text"},
            {"header":"C","question":"Q3?","mode":"text"},
            {"header":"D","question":"Q4?","mode":"text"},
            {"header":"E","question":"Q5?","mode":"text"}
        ]}"#;
        let (reqs, is_batch) = parse_batch(args).unwrap();
        assert!(is_batch);
        assert_eq!(reqs.len(), 4, "clamped to MAX_QUESTIONS");
        assert_eq!(reqs[0].header, "A");
    }

    #[test]
    fn parse_batch_single_element_questions_is_not_a_batch() {
        // A 1-element `questions` array must go down the single wire (is_batch=false) so the
        // flat populated payload reaches the driver — otherwise a batch payload has no
        // top-level header/question and a length-based driver renders an empty card.
        let (reqs, is_batch) =
            parse_batch(r#"{"questions":[{"header":"H","question":"Q?","mode":"text"}]}"#).unwrap();
        assert!(!is_batch, "one question is not a batch");
        assert_eq!(reqs.len(), 1);
    }

    #[test]
    fn parse_batch_falls_back_to_single_legacy_shape() {
        let (reqs, is_batch) =
            parse_batch(r#"{"header":"H","question":"Q?","mode":"text"}"#).unwrap();
        assert!(!is_batch);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].mode, UserInputMode::Text);
    }

    #[test]
    fn parse_batch_validates_each_question_options() {
        let args = r#"{"questions":[{"header":"A","question":"Q?","mode":"single","options":[]}]}"#;
        assert!(parse_batch(args).is_err(), "choice question needs options");
    }

    #[test]
    fn format_batch_keys_each_line_by_header_and_declines_untouched() {
        let reqs = vec![
            UserInputRequest {
                header: "Auth".into(),
                question: "?".into(),
                mode: UserInputMode::Single,
                options: vec![UserInputOption {
                    label: "OAuth".into(),
                    description: None,
                }],
                custom: true,
            },
            UserInputRequest {
                header: "Note".into(),
                question: "?".into(),
                mode: UserInputMode::Text,
                options: vec![],
                custom: true,
            },
        ];
        let resps = vec![
            UserInputResponse {
                declined: false,
                selected: vec!["OAuth".into()],
                text: None,
                images: vec![],
            },
            UserInputResponse::declined(),
        ];
        let out = format_batch_result(&reqs, &resps).content;
        assert_eq!(
            out,
            "Q1 (Auth): User selected: \"OAuth\"\nQ2 (Note): No answer (declined)."
        );
    }

    #[test]
    fn format_batch_all_declined_is_the_single_no_answer_guidance() {
        let reqs = vec![UserInputRequest {
            header: "A".into(),
            question: "?".into(),
            mode: UserInputMode::Text,
            options: vec![],
            custom: true,
        }];
        let out = format_batch_result(&reqs, &[UserInputResponse::declined()]);
        assert!(!out.is_error);
        assert!(out.content.starts_with("No answer was provided."));
    }

    #[test]
    fn legacy_custom_false_is_accepted_but_normalized_true() {
        let r = parse_args(
            r#"{"header":"H","question":"Q?","mode":"single","options":[{"label":"A"}]}"#,
        )
        .unwrap();
        assert!(r.custom, "custom absent → defaults true");
        let r2 = parse_args(
            r#"{"header":"H","question":"Q?","mode":"single","options":[{"label":"A"}],"custom":false}"#,
        )
        .unwrap();
        assert!(
            r2.custom,
            "legacy custom:false must not remove the user's free-form escape hatch"
        );
        let schema = RequestUserInputTool.parameters_schema();
        assert!(
            schema["properties"].get("custom").is_none(),
            "the model-facing schema must not expose the legacy switch"
        );
    }

    #[test]
    fn roundtrip_serde() {
        let req = UserInputRequest {
            header: "H".into(),
            question: "Q?".into(),
            mode: UserInputMode::Single,
            options: vec![UserInputOption {
                label: "A".into(),
                description: None,
            }],
            custom: true,
        };
        assert_eq!(
            serde_json::from_str::<UserInputRequest>(&serde_json::to_string(&req).unwrap())
                .unwrap(),
            req
        );
    }

    /// Ticking an option AND typing a note must surface BOTH — the ticked option used to be
    /// dropped outright once `text` was present.
    #[test]
    fn selection_and_free_text_both_reach_the_model() {
        let r = format_result(&UserInputResponse {
            declined: false,
            selected: vec!["Python".into()],
            text: Some("plus Rust".into()),
            images: vec![],
        });
        assert_eq!(
            r.content,
            r#"User selected: "Python", and User answered: "plus Rust""#
        );
    }

    /// Same in the batch path, which shares `answer_summary`.
    #[test]
    fn batch_keeps_selection_alongside_free_text() {
        let reqs = vec![UserInputRequest {
            header: "Lang".into(),
            question: "Pick".into(),
            mode: UserInputMode::Multiple,
            options: vec![
                UserInputOption {
                    label: "Python".into(),
                    description: None,
                },
                UserInputOption {
                    label: "Rust".into(),
                    description: None,
                },
            ],
            custom: true,
        }];
        let resps = vec![UserInputResponse {
            declined: false,
            selected: vec!["Python".into(), "Rust".into()],
            text: Some("plus Go".into()),
            images: vec![],
        }];
        let r = format_batch_result(&reqs, &resps);
        assert_eq!(
            r.content,
            r#"Q1 (Lang): User selected: "Python", "Rust", and User answered: "plus Go""#
        );
    }

    /// A blank custom field is not an answer: it must not shadow the ticked options.
    #[test]
    fn blank_free_text_does_not_shadow_selection() {
        let r = format_result(&UserInputResponse {
            declined: false,
            selected: vec!["Python".into()],
            text: Some("   ".into()),
            images: vec![],
        });
        assert_eq!(r.content, r#"User selected: "Python""#);
    }

    #[test]
    fn attached_image_is_forwarded_in_tool_result() {
        let image = ImageContent {
            media_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
        };
        let r = format_result(&UserInputResponse {
            declined: false,
            selected: vec![],
            text: Some("reference".into()),
            images: vec![image.clone()],
        });
        assert_eq!(r.images, vec![image]);
        assert!(r.content.contains("User attached 1 image"));
    }

    #[test]
    fn old_response_without_images_remains_compatible() {
        let r: UserInputResponse =
            serde_json::from_str(r#"{"declined":false,"selected":["A"],"text":null}"#).unwrap();
        assert!(r.images.is_empty());
    }

    /// Every answer the writers can say reads back as what was answered.
    ///
    /// The screen draws a model's question and its answer from the result text
    /// alone (the log has nothing else, see [`read_result`]), so the reader and
    /// the writers are one contract: rewording either side must fail here, not
    /// turn a transcript's answers back into English meant for the model.
    #[test]
    fn a_result_reads_back_as_the_answers_it_was_written_from() {
        let image = || ImageContent {
            media_type: "image/png".into(),
            data: "x".into(),
        };
        let given = |selected: &[&str], text: Option<&str>, images: usize| UserInputResponse {
            declined: false,
            selected: selected.iter().map(|s| s.to_string()).collect(),
            text: text.map(str::to_string),
            images: (0..images).map(|_| image()).collect(),
        };
        // What each response should read back as: blank words typed beside a
        // pick are not said to the model, so they are not read back either.
        let expected = |r: &UserInputResponse| {
            if r.declined {
                return ReadAnswer::Declined;
            }
            let text = if r.selected.is_empty() {
                r.text.clone()
            } else {
                r.text.clone().filter(|t| !t.trim().is_empty())
            };
            ReadAnswer::Given {
                selected: r.selected.clone(),
                text,
                images: r.images.len(),
            }
        };
        let answers = vec![
            given(&["推"], None, 0),
            given(&["a", "b"], None, 0),
            given(&["Python"], Some("plus Rust"), 0),
            given(&[], Some("看下日志为什么没有生效？"), 0),
            given(&[], Some("   "), 0),
            given(&[], None, 0),
            given(&["x"], Some("  "), 0),
            given(&["截图"], None, 1),
            given(&[], Some("见图"), 3),
            // Words that look like the wording itself, quotes, escapes, lines.
            given(&[r#"a", "b"#, "): User selected: \"z\""], None, 0),
            given(
                &["line\nbreak\ttab\\slash"],
                Some("bell\u{7}, and User attached 2 images"),
                0,
            ),
            given(&["C. 导航 + 全部操作 🚀"], Some("it's fine"), 2),
            UserInputResponse::declined(),
        ];

        for r in &answers {
            let content = format_result(r).content;
            assert_eq!(
                read_result(&content, 1),
                Some(vec![expected(r)]),
                "single: {content}"
            );
        }

        let req = |header: &str| UserInputRequest {
            question: "Q?".into(),
            header: header.into(),
            mode: UserInputMode::Text,
            options: vec![],
            custom: true,
        };
        for pair in answers.windows(2) {
            let reqs = [req("用途"), req("tricky): header")];
            let content = format_batch_result(&reqs, pair).content;
            assert_eq!(
                read_result(&content, 2),
                Some(pair.iter().map(expected).collect()),
                "batch: {content}"
            );
        }
        // A batch nobody answered at all is the single-question wording.
        let none = [UserInputResponse::declined(), UserInputResponse::declined()];
        let content = format_batch_result(&[req("A"), req("B")], &none).content;
        assert_eq!(
            read_result(&content, 2),
            Some(vec![ReadAnswer::Declined, ReadAnswer::Declined])
        );
    }

    /// Text these writers did not produce is not guessed at: the caller shows it
    /// as it is.
    #[test]
    fn a_result_nobody_here_wrote_is_not_read_as_answers() {
        for content in [
            "Interactive questions are not supported in this environment.",
            "invalid request_user_input arguments: missing field `question`",
            "User selected: unquoted",
            r#"User selected: "a" and more"#,
            "",
        ] {
            assert_eq!(read_result(content, 1), None, "{content}");
        }
        // A batch that does not have one line per question.
        assert_eq!(read_result(r#"Q1 (A): User selected: "x""#, 2), None);
    }
}
