//! 调用说明的两半，端到端：模型被**要求**填，工具**收不到**它。
//!
//! 两半互为反向，少任何一半这个设计都不成立：
//!
//! - 光有注入：说明会留在参数里，一路进闸门（一句"为了看 .env"就能把普通读变成
//!   敏感路径审批）、进 `bash` 的「允许所有」判定、被 MCP 转发给第三方服务器。
//! - 光有剥离：模型压根不知道要填，屏幕上什么都不会多出来。
//!
//! 判据打在两个可观测点上，都不依赖实现细节：模型请求里那个 `ToolDef`，以及工具
//! 实际收到的参数。

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};

use atomcode_coding::{prepare, CodingAgentConfig};
use atomcode_harness::agent::OnlySession;
use atomcode_harness::session::{LoggedEvent, SessionEvent};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use support::{allow, mount_parts, quiet_options, turn};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// What the model was shown and asked to do, recorded as the request goes out.
#[derive(Default)]
struct Seen {
    /// `(tool name, whether its schema offered `intent`)`, in request order.
    offered: Vec<(String, bool)>,
}

/// A scripted model that answers its first step with the given calls — exactly
/// what a model that read the guide emits — then plain text.
struct ScriptedModel {
    /// `(tool name, raw arguments)` for step one. More than one is what puts a
    /// round through `tool-exec-parallel` instead of the serial terminal.
    calls: Vec<(String, String)>,
    seen: Arc<Mutex<Seen>>,
    turn: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedModel {
    fn model_name(&self) -> &str {
        "scripted"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        tools: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        {
            let mut seen = self.seen.lock().expect("seen poisoned");
            seen.offered = tools
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        t.parameters
                            .get("properties")
                            .and_then(|p| p.get("intent"))
                            .is_some(),
                    )
                })
                .collect();
        }
        let step = self.turn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = if step == 0 {
            let mut events: Vec<StreamEvent> = self
                .calls
                .iter()
                .enumerate()
                .map(|(i, (name, arguments))| {
                    StreamEvent::ToolCall(ToolCall {
                        id: format!("c{i}"),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    })
                })
                .collect();
            events.push(StreamEvent::Done { truncated: false });
            events
        } else {
            vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::Done { truncated: false },
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Mount the product, run one turn that writes `content` with a reason attached,
/// and hand back what the model was shown plus the session's log.
async fn one_annotated_write(
    dir: &Path,
    content: &str,
) -> (Seen, Vec<LoggedEvent>, std::path::PathBuf) {
    let file = dir.join("written.txt");
    let args = serde_json::json!({
        "file_path": file.to_string_lossy(),
        "content": content,
        "intent": "creating the fixture this test reads back",
    })
    .to_string();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let model = Arc::new(ScriptedModel {
        calls: vec![("write_file".to_string(), args)],
        seen: seen.clone(),
        turn: std::sync::atomic::AtomicUsize::new(0),
    });

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "scripted", dir);
    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();
    let mut mounted = mount_parts(&parts, &cfg, &opts, model).await;
    let outcome = turn(&mut mounted.handle, "write the file", allow()).await;
    assert_eq!(
        outcome.tool_results.len(),
        1,
        "the call must have run: {outcome:?}"
    );
    assert!(
        !outcome.tool_results[0].is_error,
        "a call carrying a reason must still run: {:?}",
        outcome.tool_results[0]
    );
    let events = mounted
        .context()
        .only_session()
        .expect("the turn's session")
        .events();
    mounted.shutdown().await;
    let offered = std::mem::take(&mut seen.lock().expect("seen poisoned").offered);
    (Seen { offered }, events, file)
}

/// **Half one.** Every tool the model may call is offered the `intent` argument.
///
/// Deliberately not asserting a particular wording or that it is required: the
/// contract is soft (an old session, a weak model and a non-object MCP schema all
/// get by without one), and what this pins is that the OFFER reaches every tool
/// rather than the handful someone remembered to edit.
#[tokio::test]
async fn every_tool_is_offered_somewhere_to_say_why() {
    let project = tempfile::tempdir().unwrap();
    let (seen, _, _) = one_annotated_write(project.path(), "hello").await;
    assert!(
        !seen.offered.is_empty(),
        "no tools were advertised, so this would pass for the wrong reason"
    );
    let missing: Vec<&String> = seen
        .offered
        .iter()
        .filter(|(_, offers)| !*offers)
        .map(|(name, _)| name)
        .collect();
    assert!(
        missing.is_empty(),
        "these tools cannot be told why they are called: {missing:?} (of {} advertised)",
        seen.offered.len()
    );
}

/// **Half two.** The reason is gone before the tool runs — while the session log
/// still carries it, because that is what the screen reads.
///
/// The two facts together are the whole design: same call, two readers, and each
/// sees the shape it needs. Asserting only the first would pass for a build that
/// never told the model anything; only the second, for one that let the reason
/// reach every gate.
#[tokio::test]
async fn the_tool_never_sees_the_reason_but_the_log_keeps_it() {
    let project = tempfile::tempdir().unwrap();
    let (_, events, file) = one_annotated_write(project.path(), "hello").await;

    // What the tool was handed: no `intent`. The file proves it also ran
    // correctly — a stripped call that failed would satisfy the parse alone.
    assert_eq!(
        std::fs::read_to_string(&file).expect("the tool wrote its file"),
        "hello"
    );

    let started = events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::ToolStarted { call, .. } => Some(call.clone()),
            _ => None,
        })
        .expect("the call started");
    let executed: serde_json::Value =
        serde_json::from_str(&started.arguments).expect("arguments stay valid JSON");
    assert!(
        executed.get("intent").is_none(),
        "a gate, a grant scope or an MCP server would have seen the reason: {}",
        started.arguments
    );
    assert_eq!(executed["content"], "hello", "the rest is untouched");

    // What the screen reads: the model's own call, reason and all.
    let asked = events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::AssistantMessage { tool_calls, .. } => tool_calls.first().cloned(),
            _ => None,
        })
        .expect("the model's message");
    let logged: serde_json::Value =
        serde_json::from_str(&asked.arguments).expect("logged arguments parse");
    assert_eq!(
        logged["intent"], "creating the fixture this test reads back",
        "the log is what the transcript draws from; losing it here loses the line"
    );
}

/// The negative control for the strip: a call with no reason reaches the tool
/// with the exact bytes the model wrote.
///
/// Without this, a strip that reformatted or reordered every call would pass the
/// two criteria above — and every existing session's transcript would quietly
/// change shape.
#[tokio::test]
async fn a_call_without_a_reason_is_passed_through_byte_for_byte() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("written.txt");
    let written = serde_json::json!({
        "file_path": file.to_string_lossy(),
        "content": "unchanged",
    })
    .to_string();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let model = Arc::new(ScriptedModel {
        calls: vec![("write_file".to_string(), written.clone())],
        seen,
        turn: std::sync::atomic::AtomicUsize::new(0),
    });
    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "scripted", project.path());
    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();
    let mut mounted = mount_parts(&parts, &cfg, &opts, model).await;
    let outcome = turn(&mut mounted.handle, "write the file", allow()).await;
    assert_eq!(outcome.tool_results.len(), 1, "{outcome:?}");
    let events = mounted
        .context()
        .only_session()
        .expect("the turn's session")
        .events();
    mounted.shutdown().await;

    let started = events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::ToolStarted { call, .. } => Some(call.arguments.clone()),
            _ => None,
        })
        .expect("the call started");
    assert_eq!(
        started, written,
        "a call the model did not annotate must reach the tool as it was written"
    );
    assert_eq!(
        std::fs::read_to_string(&file).expect("the tool wrote its file"),
        "unchanged"
    );
}

/// A round with two calls never reaches the serial terminal: `tool-exec-parallel`
/// takes it and calls `execute_one` itself.
///
/// That is a different route to the same bytes, and one nothing above exercises —
/// one call short-circuits to `next.run`. If the strip were registered anywhere
/// but outermost on the batch, this is the shape it would miss: two calls, both
/// arriving at their tools with the reason still attached.
#[tokio::test]
async fn a_parallel_round_strips_every_call_in_it() {
    let project = tempfile::tempdir().unwrap();
    let first = project.path().join("one.txt");
    let second = project.path().join("two.txt");
    let args = |file: &Path, why: &str| {
        serde_json::json!({
            "file_path": file.to_string_lossy(),
            "content": "written",
            "intent": why,
        })
        .to_string()
    };
    let seen = Arc::new(Mutex::new(Seen::default()));
    let model = Arc::new(ScriptedModel {
        calls: vec![
            (
                "write_file".to_string(),
                args(&first, "writing the first fixture"),
            ),
            (
                "write_file".to_string(),
                args(&second, "writing the second fixture"),
            ),
        ],
        seen,
        turn: std::sync::atomic::AtomicUsize::new(0),
    });
    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "scripted", project.path());
    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();
    let mut mounted = mount_parts(&parts, &cfg, &opts, model).await;
    let outcome = turn(&mut mounted.handle, "write both", allow()).await;
    assert_eq!(
        outcome.tool_results.len(),
        2,
        "both calls must have run: {outcome:?}"
    );
    assert!(
        outcome.tool_results.iter().all(|r| !r.is_error),
        "neither may fail: {:?}",
        outcome.tool_results
    );
    let events = mounted
        .context()
        .only_session()
        .expect("the turn's session")
        .events();
    mounted.shutdown().await;

    let started: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::ToolStarted { call, .. } => Some(call.arguments.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 2, "both calls started: {started:?}");
    for arguments in &started {
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).expect("arguments stay valid JSON");
        assert!(
            parsed.get("intent").is_none(),
            "a parallel call kept its reason: {arguments}"
        );
    }
}
