//! Deterministic Ollama-adapter tests against a LOCAL mock HTTP server (no network, no
//! daemon). Run by default in CI. Covers:
//!   - open-call retry (transient 500 → retry → succeed)
//!   - multi-round turn-loop (round 1 `tool_calls` → kernel runs the tool → round 2 final
//!     answer), asserting the synthesized-id call pairs with its `role:"tool"` result and
//!     both are echoed back in the round-2 NDJSON request body
#![cfg(feature = "provider")]

use async_trait::async_trait;
use atomcode_capabilities::provider::{OllamaConfig, OllamaProvider, RetryPolicy};
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHAT_PATH: &str = "/api/chat";

/// Final-answer NDJSON: one content line then a `done:true` line with usage.
const FINAL_NDJSON: &str = concat!(
    "{\"message\":{\"role\":\"assistant\",\"content\":\"It is noon.\"},\"done\":false}\n",
    "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":8,\"eval_count\":4}\n",
);

/// Tool-call NDJSON: a whole `tool_calls` entry (get_time, empty args), then `done:true`.
const TOOL_CALL_NDJSON: &str = concat!(
    "{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"get_time\",\"arguments\":{}}}]},\"done\":false}\n",
    "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":5,\"eval_count\":3}\n",
);

fn provider_for(server_uri: &str) -> OllamaProvider {
    let mut cfg = OllamaConfig::new(server_uri, "qwen3");
    cfg.retry = RetryPolicy { max_attempts: 3, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(5) };
    OllamaProvider::new(cfg).expect("build provider")
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

#[tokio::test]
async fn retry_on_transient_500_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_NDJSON))
        .mount(&server)
        .await;

    let mut stream = provider_for(&server.uri())
        .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
        .await
        .expect("open succeeds after a retry");

    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Error(e) => panic!("unexpected error: {}", e.message),
            _ => {}
        }
    }
    assert_eq!(text, "It is noon.");
    assert_eq!(server.received_requests().await.unwrap().len(), 2, "one 500 + one success");
}

#[tokio::test]
async fn multi_round_tool_call_then_final_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(TOOL_CALL_NDJSON))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(CHAT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_NDJSON))
        .mount(&server)
        .await;

    let provider = Arc::new(provider_for(&server.uri()));
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

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2, "round 1 (tool) + round 2 (final)");
    let round2 = String::from_utf8_lossy(&reqs[1].body);
    // The assistant tool_call (function get_time) is echoed back...
    assert!(round2.contains("\"tool_calls\""), "round 2 echoes the assistant tool_calls: {round2}");
    assert!(round2.contains("get_time"), "with the tool name: {round2}");
    // ...and the kernel-supplied result is fed in as a role:"tool" message.
    assert!(round2.contains("\"role\":\"tool\""), "round 2 carries a tool-role result: {round2}");
    assert!(round2.contains("12:00"), "the tool's output is fed back: {round2}");
}
