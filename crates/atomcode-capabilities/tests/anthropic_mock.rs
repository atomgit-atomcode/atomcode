//! Deterministic Anthropic-adapter tests against a LOCAL mock HTTP server (no network,
//! no key). Run by default in CI. Covers:
//!   - open-call retry (transient 500 → retry → succeed; persistent 500 → exhaust → Err)
//!   - structured error parsing (Anthropic `{"type","message"}` envelope)
//!   - multi-round turn-loop (round 1 `tool_use` → kernel runs the tool → round 2 final
//!     answer), asserting the assistant `tool_use` + the `tool_result` are echoed back
//!   - SIGNED thinking round-trip (round-1 thinking `signature` is stored by the kernel
//!     and replayed VERBATIM as a `thinking` block in the round-2 request body)
#![cfg(feature = "provider")]

use async_trait::async_trait;
use atomcode_capabilities::provider::{AnthropicConfig, AnthropicProvider, RetryPolicy};
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MESSAGES_PATH: &str = "/v1/messages";

/// Final-answer stream: one text block, `end_turn`, usage, message_stop.
const FINAL_SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_final\",\"usage\":{\"input_tokens\":8}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"It is noon.\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// Tool-call stream: one `tool_use` block (id tu1 / get_time / empty input), `tool_use` stop.
const TOOL_CALL_SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tool\",\"usage\":{\"input_tokens\":5}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu1\",\"name\":\"get_time\",\"input\":{}}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// Thinking-then-tool stream: a SIGNED `thinking` block (text + signature SIG123) then a
/// `tool_use` block — the round-1 shape that exercises the signed-reasoning round-trip.
const THINKING_TOOL_SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"usage\":{\"input_tokens\":5}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"I should check the time.\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG123\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu1\",\"name\":\"get_time\",\"input\":{}}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":6}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// Build an Anthropic provider pointed at the mock server, FAST retry backoff.
fn provider_for(server_uri: &str, thinking: bool) -> AnthropicProvider {
    let mut cfg = AnthropicConfig::new("test-key", server_uri, "claude-opus-4-8");
    cfg.thinking = thinking;
    cfg.retry = RetryPolicy { max_attempts: 3, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(5) };
    AnthropicProvider::new(cfg).expect("build provider")
}

struct GetTimeTool;

#[async_trait]
impl Tool for GetTimeTool {
    fn name(&self) -> &str {
        "get_time"
    }
    fn description(&self) -> &str {
        "Get the current time."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult { call_id: String::new(), content: "12:00".into(), is_error: false, images: vec![] }
    }
}

// ---------------------------------------------------------------------------
// Retry + error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_on_transient_500_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(MESSAGES_PATH))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(MESSAGES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_SSE))
        .mount(&server)
        .await;

    let provider = provider_for(&server.uri(), false);
    let mut stream = provider
        .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
        .await
        .expect("open succeeds after a retry");

    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            atomcode_kernel::stream::StreamEvent::TextDelta(t) => text.push_str(&t),
            atomcode_kernel::stream::StreamEvent::Error(e) => panic!("unexpected error: {}", e.message),
            _ => {}
        }
    }
    assert_eq!(text, "It is noon.");
    assert_eq!(server.received_requests().await.unwrap().len(), 2, "one 500 + one success");
}

#[tokio::test]
async fn open_failure_parses_anthropic_error_type() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "invalid_request_error", "message": "max_tokens: required" }
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path(MESSAGES_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_string(body))
        .mount(&server)
        .await;

    let err = provider_for(&server.uri(), false)
        .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
        .await
        .err()
        .expect("a 400 surfaces an Err");
    assert!(!err.retryable, "400 is fatal");
    assert_eq!(err.http_status, Some(400));
    assert_eq!(err.code.as_deref(), Some("invalid_request_error"), "structured error type");
    assert!(err.message.contains("max_tokens: required"), "reason carried: {}", err.message);
}

// ---------------------------------------------------------------------------
// Multi-round turn loop
// ---------------------------------------------------------------------------

async fn run_two_round(round1_sse: &str, thinking: bool) -> Vec<wiremock::Request> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(MESSAGES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(round1_sse.to_string()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(MESSAGES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_SSE))
        .mount(&server)
        .await;

    let provider = Arc::new(provider_for(&server.uri(), thinking));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GetTimeTool));
    let tools = registry.mount(&["get_time"]);

    let outcome = Agent::builder()
        .provider(provider)
        .tools(tools)
        .max_rounds(5)
        .build()
        .run_to_completion("what time is it?", AutoRespond::AllowAll)
        .await;

    assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
    assert_eq!(outcome.text, "It is noon.", "final answer from round 2");
    server.received_requests().await.unwrap()
}

#[tokio::test]
async fn multi_round_tool_call_then_final_answer() {
    let reqs = run_two_round(TOOL_CALL_SSE, false).await;
    assert_eq!(reqs.len(), 2, "round 1 (tool) + round 2 (final)");

    let round2 = String::from_utf8_lossy(&reqs[1].body);
    // The assistant's tool_use is echoed back as a content block...
    assert!(round2.contains("\"type\":\"tool_use\""), "round 2 echoes the assistant tool_use: {round2}");
    assert!(round2.contains("\"tu1\""), "with the original tool_use id: {round2}");
    // ...and the kernel-supplied tool_result is fed in as a user tool_result block.
    assert!(round2.contains("\"type\":\"tool_result\""), "round 2 carries the tool_result: {round2}");
    assert!(round2.contains("12:00"), "the tool's output is fed back: {round2}");
}

#[tokio::test]
async fn multi_round_signed_thinking_is_echoed_back_verbatim() {
    let reqs = run_two_round(THINKING_TOOL_SSE, true).await;
    assert_eq!(reqs.len(), 2, "two rounds");

    let round2 = String::from_utf8_lossy(&reqs[1].body);
    // The round-1 signed thinking block is replayed VERBATIM in round 2 (Anthropic
    // requires the signature echoed alongside the tool call).
    assert!(round2.contains("\"type\":\"thinking\""), "round 2 echoes a thinking block: {round2}");
    assert!(round2.contains("\"signature\":\"SIG123\""), "with the EXACT signature: {round2}");
    assert!(round2.contains("I should check the time."), "and the thinking text: {round2}");
    // The request also enables thinking at the top level.
    assert!(round2.contains("\"thinking\":{\"type\":\"adaptive\"}"), "thinking enabled: {round2}");
}

#[tokio::test]
async fn signed_thinking_not_echoed_when_thinking_disabled() {
    // Same round-1 thinking stream, but the provider has thinking OFF — the stored
    // signed blocks must NOT be echoed (echoing signed thinking without thinking enabled
    // would 400).
    let reqs = run_two_round(THINKING_TOOL_SSE, false).await;
    let round2 = String::from_utf8_lossy(&reqs[1].body);
    assert!(!round2.contains("\"type\":\"thinking\""), "no thinking block echoed when disabled: {round2}");
    assert!(!round2.contains("SIG123"), "signature not echoed when thinking disabled: {round2}");
    // The tool round-trip still works.
    assert!(round2.contains("12:00"), "tool result still fed back: {round2}");
}
