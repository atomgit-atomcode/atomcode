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
    pub header: String,
    pub question: String,
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

/// Parse raw tool args into a `UserInputRequest`. Rejects choice modes with no options.
/// Returns a human message on failure (never panics).
pub fn parse_args(args: &str) -> Result<UserInputRequest, String> {
    let mut req: UserInputRequest = serde_json::from_str(args)
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
pub fn parse_batch(args: &str) -> Result<(Vec<UserInputRequest>, bool), String> {
    let val: serde_json::Value = serde_json::from_str(args)
        .map_err(|e| format!("invalid request_user_input arguments: {e}"))?;
    if let Some(qs) = val.get("questions").and_then(serde_json::Value::as_array) {
        if qs.is_empty() {
            return Err("request_user_input: `questions` must be a non-empty array".into());
        }
        let mut out = Vec::new();
        for q in qs.iter().take(MAX_QUESTIONS) {
            let mut req: UserInputRequest = serde_json::from_value(q.clone())
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
        let noun = if resp.images.len() == 1 { "image" } else { "images" };
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
        return ok_result(
            "No answer was provided. Proceed with your own best judgment; only ask again if you \
             are truly blocked.",
        );
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
        images: resps.iter().flat_map(|r| r.images.iter().cloned()).collect(),
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

/// Map the user's answer to a tool result string.
pub fn format_result(resp: &UserInputResponse) -> ToolResult {
    if resp.declined {
        return ok_result(
            "No answer was provided. Proceed with your own best judgment; only ask again if you \
             are truly blocked.",
        );
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
         list as prose. For ONE question, set `header`, `question`, `mode` \
         (\"single\"=pick one, \"multiple\"=pick any, \"text\"=free-form) and `options` \
         (non-empty for single/multiple). To ask up to 4 related questions answered in ONE \
         interaction, pass a `questions` array of those same objects instead. A free-text \
         \"type your own answer\" row is always added automatically for single/multiple, so do \
         NOT add your own \"Other\"/catch-all option. Keep each \
         `header` short (a few words)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let question = serde_json::json!({
            "type": "object",
            "required": ["header", "question", "mode"],
            "properties": {
                "header": {"type": "string", "description": "Very short label (a few words)."},
                "question": {"type": "string", "description": "One clear sentence, ideally ending in '?'."},
                "mode": {"type": "string", "enum": ["single", "multiple", "text"]},
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
        let r: UserInputResponse = serde_json::from_str(
            r#"{"declined":false,"selected":["A"],"text":null}"#,
        )
        .unwrap();
        assert!(r.images.is_empty());
    }
}
