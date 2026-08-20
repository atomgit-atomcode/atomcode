//! Elicitation round-trip: maps the kernel's `request_user_input` tool
//! (structured questions the model poses to the user) onto the stable ACP
//! `elicitation/create` request in **form** mode, and feeds the client's
//! structured answer back to the kernel as the tool result.
//!
//! Flow: the kernel emits `AgentEvent::Request { kind:
//! "request_user_input", payload }`; the turn loop calls
//! [`handle_request_user_input`], which:
//!
//! 1. parses the payload (single question or a `questions` batch, ≤4);
//! 2. builds a flat form `requestedSchema` (one property per question —
//!    `single` → enum string, `multiple` → multi-select, `text` → free text);
//! 3. sends `elicitation/create` (session-scoped) to the client **only when**
//!    the client advertised `clientCapabilities.elicitation.form` at
//!    initialize; otherwise it fails closed exactly like today — the kernel
//!    round-trip is answered with `Null`, and the tool returns its
//!    "interactive questions are not supported" result;
//! 4. maps the client's `accept` content back to `UserInputResponse`
//!    (`selected` labels / free `text`); `decline`, `cancel`, and any
//!    round-trip failure degrade to `UserInputResponse::declined()` (fail
//!    closed, never tears the connection down).
//!
//! The wire types (`ElicitationSchema`, `CreateElicitationRequest`) are the
//! v1 schema's; the v2 chain reuses them through the same v1-shaped bridging
//! the approval path already uses.

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, ElicitationAction, ElicitationContentValue, ElicitationFormMode,
    ElicitationMode, ElicitationPropertySchema, ElicitationSchema, ElicitationScope,
    ElicitationSessionScope, MultiSelectPropertySchema, SessionId, StringPropertySchema,
};
use agent_client_protocol::{Client, ConnectionTo};
use atomcode_capabilities::tools::request_user_input::{
    parse_batch, UserInputMode, UserInputRequest, UserInputResponse,
};
use atomcode_coding::CodingRuntimeHandle;

/// Property name for a single-question elicitation.
const SINGLE_PROPERTY: &str = "answer";
/// Property name prefix for batch questions (`q1`..`qN`).
const BATCH_PROPERTY_PREFIX: &str = "q";

/// Build the flat form `requestedSchema` for one `request_user_input` batch.
///
/// One property per question, named `answer` for a single question and
/// `q1..qN` for a batch; every property is required. The property schema
/// follows the tool's `mode`: `single` → enum string, `multiple` →
/// multi-select, `text` → free-text string. Question text becomes the
/// property title so the client can render it.
pub fn build_form_schema(reqs: &[UserInputRequest]) -> ElicitationSchema {
    let is_batch = reqs.len() > 1;
    let mut schema = ElicitationSchema::new();
    for (index, req) in reqs.iter().enumerate() {
        let name = if is_batch {
            format!("{BATCH_PROPERTY_PREFIX}{}", index + 1)
        } else {
            SINGLE_PROPERTY.to_string()
        };
        let labels: Vec<String> = req.options.iter().map(|o| o.label.clone()).collect();
        let property = match req.mode {
            UserInputMode::Text => ElicitationPropertySchema::String(
                StringPropertySchema::new().title(req.header.clone()),
            ),
            UserInputMode::Multiple => ElicitationPropertySchema::Array(
                MultiSelectPropertySchema::new(labels)
                    .title(req.header.clone())
                    .description(req.question.clone()),
            ),
            UserInputMode::Single => ElicitationPropertySchema::String(
                StringPropertySchema::new()
                    .title(req.header.clone())
                    .enum_values(labels),
            ),
        };
        schema = schema.property(name, property, true);
    }
    schema
}

/// The human-readable `message` for the `elicitation/create` request.
fn request_message(reqs: &[UserInputRequest]) -> String {
    if reqs.len() == 1 {
        reqs[0].question.clone()
    } else {
        "Please answer the following questions.".to_string()
    }
}

/// Extract one `ElicitationContentValue` as a `UserInputResponse` for a
/// single question (non-batch).
fn single_answer(value: &ElicitationContentValue, mode: UserInputMode) -> UserInputResponse {
    match (mode, value) {
        (UserInputMode::Text, ElicitationContentValue::String(text)) => UserInputResponse {
            declined: false,
            selected: vec![],
            text: Some(text.clone()),
            images: vec![],
        },
        (UserInputMode::Multiple, ElicitationContentValue::StringArray(labels)) => {
            UserInputResponse {
                declined: false,
                selected: labels.clone(),
                text: None,
                images: vec![],
            }
        }
        (UserInputMode::Multiple, ElicitationContentValue::String(label)) => UserInputResponse {
            declined: false,
            selected: vec![label.clone()],
            text: None,
            images: vec![],
        },
        (UserInputMode::Single, ElicitationContentValue::String(label)) => UserInputResponse {
            declined: false,
            selected: vec![label.clone()],
            text: None,
            images: vec![],
        },
        _ => UserInputResponse::declined(),
    }
}

/// Map the client's `accept` content back to the kernel's wire response.
///
/// Single question → a bare `UserInputResponse`; batch → `{ "responses": [
/// ... ] }` (the shape `request_user_input::format_batch_result` expects).
/// Unknown properties and non-string values degrade per-question to declined.
fn accept_content_to_response(
    content: &std::collections::BTreeMap<String, ElicitationContentValue>,
    reqs: &[UserInputRequest],
) -> serde_json::Value {
    if reqs.len() == 1 {
        let response = match content.get(SINGLE_PROPERTY) {
            Some(value) => single_answer(value, reqs[0].mode.clone()),
            None => UserInputResponse::declined(),
        };
        return serde_json::to_value(response)
            .unwrap_or_else(|_| serde_json::to_value(UserInputResponse::declined()).unwrap());
    }
    let responses: Vec<UserInputResponse> = reqs
        .iter()
        .enumerate()
        .map(|(index, req)| {
            let name = format!("{BATCH_PROPERTY_PREFIX}{}", index + 1);
            match content.get(&name) {
                Some(value) => single_answer(value, req.mode.clone()),
                None => UserInputResponse::declined(),
            }
        })
        .collect();
    serde_json::json!({ "responses": responses })
}

/// Handle one kernel `request_user_input` round-trip.
///
/// `form_supported` mirrors the client's `clientCapabilities.elicitation.form`
/// advertisement captured at initialize; when `false` the round-trip is
/// answered with `Null` (the tool's existing unsupported-environment result).
///
/// This function must NEVER propagate its error: it runs inside the spawned
/// prompt turn, and returning `Err` there tears the whole ACP connection down.
/// Any failure — client declined, cancelled, sent an unexpected action, or a
/// transport error — answers the kernel with a declined result (fail closed)
/// and keeps the turn going.
pub async fn handle_request_user_input(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    runtime: &CodingRuntimeHandle,
    req_id: u64,
    payload: serde_json::Value,
    form_supported: bool,
) {
    if !form_supported {
        // Same behavior as before elicitation: `Null` round-trip → the tool's
        // `null_result` ("interactive questions are not supported…").
        let _ = runtime.respond(req_id, serde_json::Value::Null).await;
        return;
    }
    let (reqs, is_batch) = match parse_batch(&payload.to_string()) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("acp: request_user_input: bad payload ({e}); answering declined");
            let _ = runtime
                .respond(
                    req_id,
                    serde_json::to_value(UserInputResponse::declined()).unwrap(),
                )
                .await;
            return;
        }
    };
    let scope = ElicitationScope::Session(ElicitationSessionScope::new(session_id.clone()));
    let mode = ElicitationMode::Form(ElicitationFormMode::new(scope, build_form_schema(&reqs)));
    let request = CreateElicitationRequest::new(mode, request_message(&reqs));
    let response = match cx.send_request(request).block_task().await {
        Ok(response) => response.action,
        Err(e) => {
            eprintln!("acp: elicitation/create round-trip failed ({e}); answering declined");
            let _ = runtime
                .respond(
                    req_id,
                    serde_json::to_value(UserInputResponse::declined()).unwrap(),
                )
                .await;
            return;
        }
    };
    let value = match response {
        ElicitationAction::Accept(accept) => {
            accept_content_to_response(&accept.content.unwrap_or_default(), &reqs)
        }
        // Decline, cancel, and unknown/future actions all degrade to declined.
        _ => {
            if is_batch {
                serde_json::json!({ "responses": vec![UserInputResponse::declined(); reqs.len()] })
            } else {
                serde_json::to_value(UserInputResponse::declined()).unwrap()
            }
        }
    };
    let _ = runtime.respond(req_id, value).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(header: &str, question: &str, mode: UserInputMode, labels: &[&str]) -> UserInputRequest {
        UserInputRequest {
            header: header.to_string(),
            question: question.to_string(),
            mode,
            options: labels
                .iter()
                .map(
                    |label| atomcode_capabilities::tools::request_user_input::UserInputOption {
                        label: label.to_string(),
                        description: None,
                    },
                )
                .collect(),
            custom: true,
        }
    }

    #[test]
    fn single_text_mode_builds_free_text_property() {
        let schema = build_form_schema(&[req("Lang", "Which language?", UserInputMode::Text, &[])]);
        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["type"], "object");
        assert_eq!(json["properties"]["answer"]["type"], "string");
        assert_eq!(json["required"][0], "answer");
    }

    #[test]
    fn single_choice_mode_builds_enum_property() {
        let schema = build_form_schema(&[req(
            "Pick",
            "Which one?",
            UserInputMode::Single,
            &["A", "B"],
        )]);
        let json = serde_json::to_value(&schema).unwrap();
        let answer = &json["properties"]["answer"];
        assert_eq!(answer["type"], "string");
        assert_eq!(
            answer["enum"].as_array().unwrap(),
            &[serde_json::json!("A"), serde_json::json!("B")]
        );
    }

    #[test]
    fn batch_builds_one_property_per_question() {
        let schema = build_form_schema(&[
            req("Q1", "First?", UserInputMode::Single, &["x"]),
            req("Q2", "Second?", UserInputMode::Text, &[]),
        ]);
        let json = serde_json::to_value(&schema).unwrap();
        assert!(json["properties"].get("q1").is_some());
        assert!(json["properties"].get("q2").is_some());
        let required: Vec<String> = json["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(required, vec!["q1", "q2"]);
    }

    #[test]
    fn accept_single_choice_maps_to_selected() {
        let content = std::collections::BTreeMap::from([(
            SINGLE_PROPERTY.to_string(),
            ElicitationContentValue::String("B".to_string()),
        )]);
        let value = accept_content_to_response(
            &content,
            &[req("P", "Q?", UserInputMode::Single, &["A", "B"])],
        );
        let parsed: UserInputResponse = serde_json::from_value(value).unwrap();
        assert!(!parsed.declined);
        assert_eq!(parsed.selected, vec!["B"]);
    }

    #[test]
    fn accept_batch_maps_each_question() {
        let content = std::collections::BTreeMap::from([
            (
                "q1".to_string(),
                ElicitationContentValue::String("x".to_string()),
            ),
            (
                "q2".to_string(),
                ElicitationContentValue::String("typed".to_string()),
            ),
        ]);
        let reqs = [
            req("Q1", "First?", UserInputMode::Single, &["x", "y"]),
            req("Q2", "Second?", UserInputMode::Text, &[]),
        ];
        let value = accept_content_to_response(&content, &reqs);
        let responses = value["responses"].clone();
        let first: UserInputResponse = serde_json::from_value(responses[0].clone()).unwrap();
        assert_eq!(first.selected, vec!["x"]);
        let second: UserInputResponse = serde_json::from_value(responses[1].clone()).unwrap();
        assert_eq!(second.text.as_deref(), Some("typed"));
    }

    #[test]
    fn missing_content_degrades_to_declined() {
        let empty = std::collections::BTreeMap::new();
        let value =
            accept_content_to_response(&empty, &[req("P", "Q?", UserInputMode::Single, &["A"])]);
        let parsed: UserInputResponse = serde_json::from_value(value).unwrap();
        assert!(parsed.declined);
    }

    #[test]
    fn kind_constant_matches_the_tool() {
        assert_eq!(
            atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND,
            "request_user_input"
        );
    }
}
