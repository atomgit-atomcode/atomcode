//! Deterministic provider tests against a LOCAL mock HTTP server (no network, no key).
//! Run by default in CI. Covers:
//!   - open-call retry (transient 500 → retry → succeed; persistent 500 → exhaust → Err)
//!   - multi-round turn-loop (round 1 tool call → kernel executes tool → round 2 final
//!     answer), with the general WireLogHooks observing each round.
#![cfg(feature = "provider")]

use async_trait::async_trait;
use atomcode_capabilities::hooks::WireLogHooks;
use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider, RetryPolicy};
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use futures::StreamExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHAT_PATH: &str = "/chat/completions";

/// A final-answer SSE response (text + stop + usage + [DONE]).
const FINAL_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"It is noon.\"}}]}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":4}}\n\
data: [DONE]\n";

/// A tool-call SSE response (assembles one whole ToolCall + finish_reason=tool_calls).
const TOOL_CALL_SSE: &str = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{}\"}}]}}]}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\
data: [DONE]\n";

/// Round-1 SSE from a THINKING model: reasoning_content deltas FOLLOWED by a tool call.
/// The kernel accumulates the reasoning onto the assistant message; whether it is
/// echoed back next round is the per-model ReasoningPolicy under test.
const ROUND1_REASONING_TOOL_SSE: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me think. \"}}]}\n\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"I should call get_time.\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{}\"}}]}}]}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\
data: [DONE]\n";

/// Build a provider pointed at the mock server, with FAST retry backoff so the
/// exhaustion test does not sleep for real seconds.
fn provider_for(server_uri: &str, model: &str) -> OpenAiCompatProvider {
    let mut cfg = OpenAiCompatConfig::new("test-key", server_uri, model);
    cfg.retry = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    };
    OpenAiCompatProvider::new(cfg).expect("build provider")
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_on_transient_500_then_succeeds() {
    let server = MockServer::start().await;
    // First request: 500 (transient). Mounted first + limited so it serves once.
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Subsequent: 200 with a real SSE body.
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_SSE))
        .mount(&server)
        .await;

    let provider = provider_for(&server.uri(), "glm-test");
    let mut stream = provider
        .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
        .await
        .expect("open should succeed after one retry");

    let mut text = String::new();
    let mut done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Done { .. } => done = true,
            StreamEvent::Error(e) => panic!("unexpected stream error: {}", e.message),
            _ => {}
        }
    }
    assert!(done, "stream must complete");
    assert_eq!(text, "It is noon.");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2, "one 500 + one 200 = the retry actually happened");
}

#[tokio::test]
async fn retry_exhausts_on_persistent_500_returns_err() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let provider = provider_for(&server.uri(), "glm-test"); // max_attempts = 3
    let res = provider
        .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
        .await;

    let err = res.err().expect("persistent 500 must return Err on the open");
    assert!(err.retryable, "500 is classified retryable");
    assert!(err.message.contains("500"), "message should carry the status: {}", err.message);

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 3, "should try exactly max_attempts (3) times then give up");
}

#[tokio::test]
async fn auth_4xx_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;

    let provider = provider_for(&server.uri(), "glm-test");
    let err = provider
        .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
        .await
        .err()
        .expect("401 must error");
    assert!(!err.retryable, "401 must be fatal, not retryable");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "a fatal 401 must NOT be retried");
}

// ---------------------------------------------------------------------------
// Multi-round turn-loop
// ---------------------------------------------------------------------------

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
        // call_id left empty: the kernel fills it from the originating tool_call.
        ToolResult { call_id: String::new(), content: "12:00".into(), is_error: false }
    }
}

#[tokio::test]
async fn multi_round_tool_loop_executes_tool_and_logs_each_round() {
    let server = MockServer::start().await;
    // Round 1: the model calls get_time (mounted first + limited → serves once).
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(TOOL_CALL_SSE))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Round 2+: the model gives the final answer.
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_SSE))
        .mount(&server)
        .await;

    let provider = Arc::new(provider_for(&server.uri(), "glm-test"));

    // Capture the general wire-log hook's output to assert it fired per round.
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let log_sink = log.clone();
    let hooks = WireLogHooks::with_sink(Arc::new(move |s: &str| log_sink.lock().unwrap().push(s.to_string())));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GetTimeTool));
    let tools = registry.mount(&["get_time"]);

    let outcome = Agent::builder()
        .provider(provider)
        .tools(tools)
        .hook(Arc::new(hooks))
        .max_rounds(5)
        .build()
        .run_to_completion("what time is it?", AutoRespond::AllowAll)
        .await;

    assert!(outcome.error.is_none(), "no error expected: {:?}", outcome.error);
    assert_eq!(outcome.text, "It is noon.", "final answer comes from round 2");

    // The server saw exactly two LLM rounds.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2, "expected 2 rounds: tool call + final answer");

    // Round 2's request must carry BOTH the assistant tool_call and the tool result.
    let round2 = String::from_utf8_lossy(&reqs[1].body);
    assert!(round2.contains("get_time"), "round 2 must echo the assistant tool_call");
    assert!(round2.contains("12:00"), "round 2 must carry the tool result fed back by the kernel");
    assert!(round2.contains("\"role\":\"tool\""), "round 2 must include a tool-role result message");

    // The general WireLogHooks logged a request for EACH round (round 1 and round 2).
    let logs = log.lock().unwrap();
    let request_logs = logs.iter().filter(|l| l.contains("request (round")).count();
    assert_eq!(request_logs, 2, "WireLogHooks must observe both rounds: {logs:?}");
}

// ---------------------------------------------------------------------------
// Multi-round REASONING round-trip (the load-bearing thinking-model behavior)
// ---------------------------------------------------------------------------

/// Drive a 2-round tool loop for `model`: round 1 emits `round1_sse`, round 2 the final
/// answer. Returns the requests the server received (so a test can inspect round 2's
/// body). Asserts the loop completed cleanly with the final answer.
async fn run_two_round(model: &str, round1_sse: &str) -> Vec<wiremock::Request> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(round1_sse.to_string()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_SSE))
        .mount(&server)
        .await;

    let provider = Arc::new(provider_for(&server.uri(), model));
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

/// DeepSeek-V4 (Include): the round-1 reasoning the model returned is STORED by the
/// kernel and ECHOED BACK as `reasoning_content` on the assistant tool-call message in
/// round 2 — the round-trip DeepSeek-V4 REQUIRES (HTTP 400 "must be passed back" else).
#[tokio::test]
async fn multi_round_reasoning_is_echoed_back_for_deepseek_v4() {
    let reqs = run_two_round("deepseek-v4-flash", ROUND1_REASONING_TOOL_SSE).await;
    assert_eq!(reqs.len(), 2, "two rounds");

    let round2 = String::from_utf8_lossy(&reqs[1].body);
    assert!(
        round2.contains("reasoning_content"),
        "V4 (Include) must echo reasoning_content back in round 2"
    );
    assert!(
        round2.contains("I should call get_time."),
        "round 2 must echo the EXACT round-1 reasoning the kernel stored: {round2}"
    );
}

/// DeepSeek-R1 (Exclude): even though the model returned reasoning in round 1 (and the
/// kernel stored it), it must NOT be echoed back in round 2 — R1 returns HTTP 400 if
/// `reasoning_content` is sent. Proves the kernel STORES but the L1 policy decides the
/// wire echo (mechanism vs policy).
#[tokio::test]
async fn multi_round_reasoning_is_stripped_for_deepseek_r1() {
    let reqs = run_two_round("deepseek-r1", ROUND1_REASONING_TOOL_SSE).await;
    assert_eq!(reqs.len(), 2, "two rounds");

    let round2 = String::from_utf8_lossy(&reqs[1].body);
    assert!(
        !round2.contains("reasoning_content"),
        "R1 (Exclude) must NOT echo reasoning_content (400 otherwise): {round2}"
    );
    // sanity: the round still carried the tool result + the assistant tool_call.
    assert!(round2.contains("12:00"), "tool result still fed back");
    assert!(round2.contains("get_time"), "assistant tool_call still echoed");
}
