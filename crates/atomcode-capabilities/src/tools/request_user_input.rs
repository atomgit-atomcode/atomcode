//! The `request_user_input` tool: the model poses ONE structured question
//! (single / multiple / text); the turn pauses; the user answers in the driver UI;
//! the answer returns as the tool result. Rides the generic kernel request/respond
//! round-trip via `ToolContext::request`. Types are defined here (drivers import them);
//! atomcode-core stays agnostic.

use async_trait::async_trait;
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInputRequest {
    pub header: String,
    pub question: String,
    pub mode: UserInputMode,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct UserInputResponse {
    pub declined: bool,
    #[serde(default)]
    pub selected: Vec<String>, // single: len<=1; multiple: 0..N (labels)
    #[serde(default)]
    pub text: Option<String>, // text mode
}

impl UserInputResponse {
    pub fn declined() -> Self {
        Self {
            declined: true,
            ..Default::default()
        }
    }
}

/// Parse raw tool args into a `UserInputRequest`. Rejects choice modes with no options.
/// Returns a human message on failure (never panics).
pub fn parse_args(args: &str) -> Result<UserInputRequest, String> {
    let req: UserInputRequest = serde_json::from_str(args)
        .map_err(|e| format!("invalid request_user_input arguments: {e}"))?;
    if matches!(req.mode, UserInputMode::Single | UserInputMode::Multiple) && req.options.is_empty()
    {
        return Err(
            "request_user_input: single/multiple mode requires a non-empty `options` array".into(),
        );
    }
    Ok(req)
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
    if let Some(t) = &resp.text {
        return ok_result(format!("User answered: {t:?}"));
    }
    if resp.selected.is_empty() {
        return ok_result("User selected nothing.");
    }
    let joined = resp
        .selected
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    ok_result(format!("User selected: {joined}"))
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
        "Ask the user ONE structured question and wait for their answer before continuing. \
         Use ONLY for a decision that is genuinely the user's to make — a preference, a \
         confirmation, a choice between approaches — NOT for anything you can decide, look \
         up, or verify yourself. `mode`: \"single\" (pick one), \"multiple\" (pick any), or \
         \"text\" (free-form). Provide a non-empty `options` array for single/multiple. Keep \
         `header` short (a few words)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let req = match parse_args(args) {
            Ok(r) => r,
            Err(e) => return err_result(e),
        };
        let payload = match serde_json::to_value(&req) {
            Ok(v) => v,
            Err(e) => return err_result(format!("request_user_input: serialize failed: {e}")),
        };
        let resp_val = ctx.request(REQUEST_USER_INPUT_KIND, payload).await;
        if resp_val.is_null() {
            return null_result();
        }
        match serde_json::from_value::<UserInputResponse>(resp_val) {
            Ok(resp) => format_result(&resp),
            Err(_) => format_result(&UserInputResponse::declined()),
        }
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
            })
            .content,
            r#"User selected: "A", "B""#
        );
        assert_eq!(
            format_result(&UserInputResponse {
                declined: false,
                selected: vec![],
                text: None,
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
    fn roundtrip_serde() {
        let req = UserInputRequest {
            header: "H".into(),
            question: "Q?".into(),
            mode: UserInputMode::Single,
            options: vec![UserInputOption {
                label: "A".into(),
                description: None,
            }],
        };
        assert_eq!(
            serde_json::from_str::<UserInputRequest>(&serde_json::to_string(&req).unwrap())
                .unwrap(),
            req
        );
    }
}
