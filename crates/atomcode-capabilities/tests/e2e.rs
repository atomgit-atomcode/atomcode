//! END-TO-END tests against a REAL provider (network). SEPARATE from the default
//! suite: the whole file is gated behind the `e2e` cargo feature, so it neither
//! compiles nor runs on a plain `cargo test`. Run ON DEMAND:
//!
//!   ATOMCODE_LIVE_API_KEY=sk-... \
//!   ATOMCODE_LIVE_BASE_URL=https://api.deepseek.com \
//!   ATOMCODE_LIVE_MODEL=deepseek-v4-flash \
//!   cargo test -p atomcode-capabilities --features e2e --test e2e -- --nocapture
//!
//! GLM example: BASE_URL=https://open.bigmodel.cn/api/paas/v4  MODEL=glm-4-flash
//!
//! (Deterministic, network-free integration tests live in tests/http_mock.rs and DO
//! run by default — those are NOT e2e.)
#![cfg(feature = "e2e")]

use atomcode_capabilities::provider::{
    AnthropicConfig, AnthropicProvider, OllamaConfig, OllamaProvider, OpenAiCompatConfig,
    OpenAiCompatProvider,
};
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
            StreamEvent::ToolCallDelta { .. } => {}
            StreamEvent::ReasoningSignature { .. } => {}
            StreamEvent::ResponseId(id) => eprintln!("[live] provider response_id={id}"),
            StreamEvent::Error(e) => panic!("[live] stream error: {}", e.message),
        }
    }

    eprintln!("[live] text={text:?}");
    if !reasoning.is_empty() {
        eprintln!("[live] reasoning_len={}", reasoning.len());
    }
    if let Some(u) = usage {
        eprintln!(
            "[live] usage prompt={} completion={} cached={}",
            u.prompt, u.completion, u.cached
        );
    }
    assert!(saw_done, "stream must reach Done");
    assert!(
        !text.trim().is_empty(),
        "expected some text content, got empty"
    );
}

/// Open a REAL Anthropic Messages stream, consume it, assert text + clean Done. Run:
///
///   ATOMCODE_ANTHROPIC_KEY=sk-ant-... \
///   cargo test -p atomcode-capabilities --features e2e --test e2e \
///     live_anthropic_smoke -- --nocapture --ignored
///
/// Optional: ATOMCODE_ANTHROPIC_BASE_URL (default https://api.anthropic.com),
/// ATOMCODE_ANTHROPIC_MODEL (default claude-haiku-4-5).
#[tokio::test]
#[ignore = "hits the real Anthropic API; run explicitly with a key"]
async fn live_anthropic_smoke_streams_text_and_done() {
    let base = std::env::var("ATOMCODE_ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = std::env::var("ATOMCODE_ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5".to_string());
    let cfg = AnthropicConfig::new(env("ATOMCODE_ANTHROPIC_KEY"), base, model);
    let provider = AnthropicProvider::new(cfg).expect("build provider");

    let messages = vec![Message::user("Reply with exactly the single word: pong")];
    let mut stream = provider
        .chat_stream(&messages, &[], &ChatOptions::default())
        .await
        .expect("open stream");

    let mut text = String::new();
    let mut usage = None;
    let mut saw_done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Usage(u) => usage = Some(u),
            StreamEvent::Done { truncated } => {
                saw_done = true;
                eprintln!("[live-anthropic] Done(truncated={truncated})");
            }
            StreamEvent::ResponseId(id) => eprintln!("[live-anthropic] response_id={id}"),
            StreamEvent::Error(e) => panic!("[live-anthropic] stream error: {}", e.message),
            _ => {}
        }
    }
    eprintln!("[live-anthropic] text={text:?}");
    if let Some(u) = usage {
        eprintln!(
            "[live-anthropic] usage prompt={} completion={} cached={}",
            u.prompt, u.completion, u.cached
        );
    }
    assert!(saw_done, "stream must reach Done");
    assert!(
        !text.trim().is_empty(),
        "expected some text content, got empty"
    );
}

/// Open a REAL Ollama `/api/chat` stream against a LOCAL daemon, assert text + Done. Run:
///
///   cargo test -p atomcode-capabilities --features e2e --test e2e \
///     live_ollama_smoke -- --nocapture --ignored
///
/// Optional: ATOMCODE_OLLAMA_BASE_URL (default http://localhost:11434),
/// ATOMCODE_OLLAMA_MODEL (default llama3.2). Requires `ollama pull <model>` first.
#[tokio::test]
#[ignore = "needs a local Ollama daemon with the model pulled; run explicitly"]
async fn live_ollama_smoke_streams_text_and_done() {
    let base = std::env::var("ATOMCODE_OLLAMA_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434".to_string());
    let model = std::env::var("ATOMCODE_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".to_string());
    let provider = OllamaProvider::new(OllamaConfig::new(base, model)).expect("build provider");

    let messages = vec![Message::user("Reply with exactly the single word: pong")];
    let mut stream = provider
        .chat_stream(&messages, &[], &ChatOptions::default())
        .await
        .expect("open stream");

    let mut text = String::new();
    let mut usage = None;
    let mut saw_done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Usage(u) => usage = Some(u),
            StreamEvent::Done { truncated } => {
                saw_done = true;
                eprintln!("[live-ollama] Done(truncated={truncated})");
            }
            StreamEvent::Error(e) => panic!("[live-ollama] stream error: {}", e.message),
            _ => {}
        }
    }
    eprintln!("[live-ollama] text={text:?}");
    if let Some(u) = usage {
        eprintln!(
            "[live-ollama] usage prompt={} completion={}",
            u.prompt, u.completion
        );
    }
    assert!(saw_done, "stream must reach Done");
    assert!(
        !text.trim().is_empty(),
        "expected some text content, got empty"
    );
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

    // Log to a file if ATOMCODE_WIRE_LOG_FILE is set, else to stderr.
    let log_hook: Arc<dyn atomcode_kernel::hook::LifecycleHooks> =
        match std::env::var("ATOMCODE_WIRE_LOG_FILE") {
            Ok(p) => {
                eprintln!("[live] wire log → file: {p}");
                Arc::new(WireLogHooks::to_file(&p).expect("open wire log file"))
            }
            Err(_) => Arc::new(WireLogHooks::stderr()),
        };

    let outcome = Agent::builder()
        .provider(provider)
        .tools(tools)
        .hook(log_hook) // general, provider-agnostic wire log
        .max_rounds(3)
        .build()
        .run_to_completion(
            "Reply with exactly the single word: pong",
            AutoRespond::AllowAll,
        )
        .await;

    eprintln!(
        "[live] outcome: stop={:?} error={:?} text={:?}",
        outcome.stop, outcome.error, outcome.text
    );
    assert!(
        outcome.error.is_none(),
        "unexpected error: {:?}",
        outcome.error
    );
    assert!(
        !outcome.text.trim().is_empty(),
        "expected assistant text from the turn loop"
    );
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

/// REAL multi-round reasoning round-trip against a THINKING model (DeepSeek-V4 family).
///
/// Proves end-to-end against the LIVE API that when a V4 model returns reasoning + a
/// tool call in round 1, the kernel stores the reasoning and the adapter ECHOES it back
/// as `reasoning_content` in round 2 — and the API ACCEPTS it (no HTTP 400 "the
/// reasoning_content in the thinking mode must be passed back"). Surviving a >=2-round
/// loop with no error IS the proof the round-trip held; a missing/wrong echo would 400
/// the second round.
///
/// Requires ATOMCODE_LIVE_MODEL to be a `deepseek-v4*` model (Include policy).
#[tokio::test]
async fn e2e_multi_round_reasoning_roundtrip_does_not_400() {
    use atomcode_capabilities::hooks::WireLogHooks;
    use atomcode_kernel::agent::{Agent, AutoRespond};
    use atomcode_kernel::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
    use std::sync::{Arc, Mutex};

    struct GetTimeTool;
    #[async_trait::async_trait]
    impl Tool for GetTimeTool {
        fn name(&self) -> &str {
            "get_time"
        }
        fn description(&self) -> &str {
            "Get the current time in a given city."
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string", "description": "City name" } },
                "required": ["city"]
            })
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: "12:00 (noon)".into(),
                is_error: false,
                images: vec![],
            }
        }
    }

    let cfg = OpenAiCompatConfig::new(
        env("ATOMCODE_LIVE_API_KEY"),
        env("ATOMCODE_LIVE_BASE_URL"),
        env("ATOMCODE_LIVE_MODEL"),
    );
    let provider = Arc::new(OpenAiCompatProvider::new(cfg).expect("build provider"));

    // Count rounds via the general hook so we KNOW the loop went multi-round, AND tee
    // the full wire log to a file when ATOMCODE_WIRE_LOG_FILE is set (else stderr only).
    let rounds = Arc::new(Mutex::new(0usize));
    let counter = rounds.clone();
    let log_file = std::env::var("ATOMCODE_WIRE_LOG_FILE").ok().map(|p| {
        eprintln!("[e2e] wire log → file: {p}");
        Arc::new(Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .expect("open wire log file"),
        ))
    });
    let lf = log_file.clone();
    let hooks = WireLogHooks::with_sink(Arc::new(move |s: &str| {
        eprintln!("{s}");
        if s.contains("[wire] request") {
            *counter.lock().unwrap() += 1;
        }
        if let Some(f) = &lf {
            use std::io::Write;
            let _ = writeln!(f.lock().unwrap(), "{s}\n");
        }
    }));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(GetTimeTool));
    let tools = registry.mount(&["get_time"]);

    let outcome = Agent::builder()
        .provider(provider)
        .session_id("sess-e2e-demo")
        .tools(tools)
        .hook(Arc::new(hooks))
        .max_rounds(6)
        .build()
        .run_to_completion(
            "Call the get_time tool for the city Paris, then tell me the time in one short sentence.",
            AutoRespond::AllowAll,
        )
        .await;

    let n = *rounds.lock().unwrap();
    eprintln!(
        "[e2e] rounds={n} stop={:?} error={:?} text={:?}",
        outcome.stop, outcome.error, outcome.text
    );

    // CORE invariants (ALWAYS): no 400/error — including the echoed reasoning_content
    // NOT being rejected in round 2 — and a real final answer.
    assert!(
        outcome.error.is_none(),
        "reasoning round-trip likely 400'd in round 2: {:?}",
        outcome.error
    );
    assert!(!outcome.text.trim().is_empty(), "expected a final answer");

    // Going multi-round is MODEL-DEPENDENT (V4 may answer directly without the tool),
    // so we do NOT hard-fail on a single round — that would make this gated test flake
    // on the model's mood, not on our code. When it DID go multi-round, that LIVE-proves
    // the reasoning round-trip survived round 2 with no 400. The DETERMINISTIC, always-2-
    // rounds proof (byte-exact reasoning echo) lives in tests/http_mock.rs.
    if n >= 2 {
        eprintln!("[e2e] ✓ multi-round reasoning round-trip held over {n} rounds (no 400)");
    } else {
        eprintln!(
            "[e2e] NOTE: model answered in {n} round (no tool call) — multi-round NOT \
             exercised this run; see tests/http_mock.rs for the deterministic proof"
        );
    }
}
