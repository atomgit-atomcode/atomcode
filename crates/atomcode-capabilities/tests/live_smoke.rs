//! Gated LIVE smoke test — hits a real OpenAI-compatible endpoint.
//!
//! Excluded from the default build/test; only compiles + runs under `--features live`.
//! Run:
//!   ATOMCODE_LIVE_API_KEY=sk-... \
//!   ATOMCODE_LIVE_BASE_URL=https://api.deepseek.com \
//!   ATOMCODE_LIVE_MODEL=deepseek-chat \
//!   cargo test -p atomcode-capabilities --features live --test live_smoke -- --nocapture
//!
//! GLM example: BASE_URL=https://open.bigmodel.cn/api/paas/v4  MODEL=glm-4-flash
#![cfg(feature = "live")]

use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use futures::StreamExt;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} to run the live smoke test"))
}

/// Open a real stream, consume it, and assert we get text + a clean Done.
#[tokio::test]
async fn live_smoke_streams_text_and_done() {
    let cfg = OpenAiCompatConfig::new(
        env("ATOMCODE_LIVE_API_KEY"),
        env("ATOMCODE_LIVE_BASE_URL"),
        env("ATOMCODE_LIVE_MODEL"),
    );
    let provider = OpenAiCompatProvider::new(cfg).expect("build provider");

    let messages = vec![Message::user("Reply with exactly the single word: pong")];
    let mut stream = provider
        .chat_stream(&messages, &[], &ChatOptions::default())
        .await
        .expect("open stream");

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut usage = None;
    let mut saw_done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Reasoning(r) => reasoning.push_str(&r),
            StreamEvent::Usage(u) => usage = Some(u),
            StreamEvent::Done { truncated } => {
                saw_done = true;
                eprintln!("[live] Done(truncated={truncated})");
            }
            StreamEvent::ToolCall(tc) => eprintln!("[live] unexpected ToolCall: {}", tc.name),
            StreamEvent::Error(e) => panic!("[live] stream error: {}", e.message),
        }
    }

    eprintln!("[live] text={text:?}");
    if !reasoning.is_empty() {
        eprintln!("[live] reasoning_len={}", reasoning.len());
    }
    if let Some(u) = usage {
        eprintln!("[live] usage prompt={} completion={} cached={}", u.prompt, u.completion, u.cached);
    }
    assert!(saw_done, "stream must reach Done");
    assert!(!text.trim().is_empty(), "expected some text content, got empty");
}

/// Run a real Agent turn-loop with the provider-agnostic `WireLogHooks` attached —
/// proving the GENERAL logging hook fires through the kernel loop (request via
/// on_request, response via on_model_response), not via any adapter-specific code.
#[tokio::test]
async fn live_agent_turn_loop_logs_via_hook() {
    use atomcode_capabilities::hooks::WireLogHooks;
    use atomcode_kernel::agent::{Agent, AutoRespond};
    use atomcode_kernel::tool::ToolRegistry;
    use std::sync::Arc;

    let cfg = OpenAiCompatConfig::new(
        env("ATOMCODE_LIVE_API_KEY"),
        env("ATOMCODE_LIVE_BASE_URL"),
        env("ATOMCODE_LIVE_MODEL"),
    );
    let provider = Arc::new(OpenAiCompatProvider::new(cfg).expect("build provider"));
    let tools = ToolRegistry::new().mount(&[]); // no tools for this demo

    let outcome = Agent::builder()
        .provider(provider)
        .tools(tools)
        .hook(Arc::new(WireLogHooks::stderr())) // general, provider-agnostic wire log
        .max_rounds(3)
        .build()
        .run_to_completion("Reply with exactly the single word: pong", AutoRespond::AllowAll)
        .await;

    eprintln!(
        "[live] outcome: stop={:?} error={:?} text={:?}",
        outcome.stop, outcome.error, outcome.text
    );
    assert!(outcome.error.is_none(), "unexpected error: {:?}", outcome.error);
    assert!(!outcome.text.trim().is_empty(), "expected assistant text from the turn loop");
}

/// Open a real stream WITH a tool and assert a whole ToolCall assembles.
/// Best run against a model that reliably calls tools (e.g. deepseek-chat).
#[tokio::test]
#[ignore = "tool-calling behavior is model-dependent; run explicitly"]
async fn live_smoke_tool_call_assembles() {
    let cfg = OpenAiCompatConfig::new(
        env("ATOMCODE_LIVE_API_KEY"),
        env("ATOMCODE_LIVE_BASE_URL"),
        env("ATOMCODE_LIVE_MODEL"),
    );
    let provider = OpenAiCompatProvider::new(cfg).expect("build provider");

    let tools = vec![atomcode_kernel::tool::ToolDef {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        }),
    }];
    let messages = vec![Message::user("What's the weather in Paris? Use the tool.")];
    let mut stream = provider
        .chat_stream(&messages, &tools, &ChatOptions::default())
        .await
        .expect("open stream");

    let mut calls = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::ToolCall(tc) => calls.push(tc),
            StreamEvent::Error(e) => panic!("[live] stream error: {}", e.message),
            _ => {}
        }
    }
    eprintln!("[live] tool calls: {calls:?}");
    assert!(!calls.is_empty(), "expected at least one tool call");
    // arguments must be complete, parseable JSON (proves delta assembly worked).
    let parsed: serde_json::Value =
        serde_json::from_str(&calls[0].arguments).expect("tool arguments must be valid JSON");
    assert!(parsed.get("city").is_some(), "expected a city argument");
}
