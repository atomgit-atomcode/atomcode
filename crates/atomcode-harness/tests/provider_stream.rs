//! Provider facts must survive the active loop, persistence and the next request.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::OnlySession;
use atomcode_harness::seams::LlmSvc;
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::ToolDef;
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use serde_json::Value;

#[ctor::ctor]
fn isolate_home() {
    atomcode_kernel::test_support::isolate_home();
}

struct Provider {
    streams: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl LlmProvider for Provider {
    fn model_name(&self) -> &str {
        "requested-alias"
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let events = self
            .streams
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider call");
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct ProviderRow(Arc<Provider>);
#[async_trait]
impl Plugin for ProviderRow {
    fn name(&self) -> &'static str {
        "provider-fixture"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        ctx.provide::<LlmSvc>(self.0.clone())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

async fn app(root: &std::path::Path, provider: Arc<Provider>, resume: bool) -> App {
    let path = bundle::toml_string(&root.to_string_lossy());
    let patch = format!(
        r#"
[[patch]]
id = "llm"
name = "provider-fixture"
config = {{}}
[[patch]]
id = "session"
config = {{ id = "provider-facts", resume = {resume} }}
[[patch]]
id = "session-persistence-jsonl"
config = {{ root = {path}, project_root = {path} }}
"#
    );
    let mut catalog = plugins::catalog();
    catalog.register(Arc::new(ProviderRow(provider)));
    let tree =
        ConfigTree::from_layers([bundle::infra().unwrap(), Layer::from_toml(&patch).unwrap()])
            .unwrap();
    let mut app = App::new(catalog, tree);
    app.start().await.unwrap();
    app
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn signed_blocks_and_response_identity_survive_resume_and_reach_the_next_request() {
    let root =
        std::env::temp_dir().join(format!("atomcode-provider-stream-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([
            vec![
                StreamEvent::ResponseId("upstream-1".into()),
                StreamEvent::ResponseModel("reported-model".into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 100,
                    cached: 70,
                    completion: 0,
                }),
                StreamEvent::Reasoning("first block".into()),
                StreamEvent::ReasoningSignature {
                    opaque: "signature-1".into(),
                    provider: "signed-provider".into(),
                },
                StreamEvent::ReasoningSignature {
                    opaque: "redacted".into(),
                    provider: "signed-provider".into(),
                },
                StreamEvent::Reasoning("second block".into()),
                StreamEvent::ReasoningSignature {
                    opaque: "signature-2".into(),
                    provider: "signed-provider".into(),
                },
                StreamEvent::TextDelta("first answer".into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 0,
                    cached: 0,
                    completion: 9,
                }),
                StreamEvent::Usage(TokenUsage {
                    prompt: 0,
                    cached: 0,
                    completion: 9,
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("second answer".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let first = app(&root, provider.clone(), false).await;
    assert_eq!(
        run_turn(&first, "start").await.unwrap().text,
        "first answer"
    );
    let messages = atomcode_kernel::session::derive_messages_with_meta(
        &first.context().only_session().unwrap().events(),
    );
    let saved = messages
        .iter()
        .find(|m| m.role == Role::Assistant && m.text == "first answer")
        .unwrap()
        .clone();
    assert_eq!(saved.reasoning_blocks.len(), 3);
    assert_eq!(saved.reasoning_blocks[0].text, "first block");
    assert_eq!(saved.reasoning_blocks[1].text, "");
    assert_eq!(
        saved.reasoning_blocks[1].opaque.as_deref(),
        Some("redacted")
    );
    assert_eq!(saved.reasoning_blocks[2].text, "second block");
    let meta = saved.meta.as_ref().unwrap();
    assert_eq!(meta.provider_model.as_deref(), Some("reported-model"));
    assert_eq!(meta.provider_response_id.as_deref(), Some("upstream-1"));
    assert_eq!(
        (
            meta.tokens.prompt,
            meta.tokens.cached,
            meta.tokens.completion
        ),
        (100, 70, 9)
    );
    drop(first);

    let resumed = app(&root, provider.clone(), true).await;
    assert_eq!(
        run_turn(&resumed, "continue").await.unwrap().text,
        "second answer"
    );
    let requests = provider.requests.lock().unwrap();
    let replayed = requests[1]
        .iter()
        .find(|m| m.role == Role::Assistant && m.text == "first answer")
        .unwrap();
    assert_eq!(replayed.reasoning_blocks, saved.reasoning_blocks);
    drop(requests);
    let messages = atomcode_kernel::session::derive_messages_with_meta(
        &resumed.context().only_session().unwrap().events(),
    );
    let second = messages.iter().find(|m| m.text == "second answer").unwrap();
    assert!(
        second.meta.as_ref().unwrap().provider_response_id.is_none(),
        "response identities cannot leak to the next request"
    );
    drop(resumed);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_recovered_attempt_keeps_its_own_signatures_and_identity() {
    let root =
        std::env::temp_dir().join(format!("atomcode-provider-recovery-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([
            vec![
                StreamEvent::ResponseId("failed-attempt".into()),
                StreamEvent::ResponseModel("reported-model".into()),
                StreamEvent::Reasoning("completed block".into()),
                StreamEvent::ReasoningSignature {
                    opaque: "partial-signature".into(),
                    provider: "signed-provider".into(),
                },
                StreamEvent::TextDelta("partial answer".into()),
                StreamEvent::Error(ProviderError {
                    message: "interrupted".into(),
                    code: Some(atomcode_kernel::stream::STREAM_TRANSPORT_INTERRUPTED_CODE.into()),
                    ..Default::default()
                }),
            ],
            vec![
                StreamEvent::TextDelta("recovered answer".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])),
        requests: Mutex::new(Vec::new()),
    });
    let app = app(&root, provider.clone(), false).await;
    let outcome = run_turn(&app, "start").await.unwrap();
    assert_eq!(outcome.error, None);
    let messages = atomcode_kernel::session::derive_messages_with_meta(
        &app.context().only_session().unwrap().events(),
    );
    let partial = messages
        .iter()
        .find(|m| m.text == "partial answer")
        .unwrap();
    assert_eq!(
        partial.reasoning_blocks[0].opaque.as_deref(),
        Some("partial-signature")
    );
    let meta = partial.meta.as_ref().unwrap();
    assert_eq!(meta.provider_response_id.as_deref(), Some("failed-attempt"));
    assert_eq!(meta.finish_reason, "error");
    let final_message = messages
        .iter()
        .find(|m| m.text == "recovered answer")
        .unwrap();
    assert!(final_message.reasoning_blocks.is_empty());
    let final_meta = final_message.meta.as_ref().unwrap();
    assert!(final_meta.provider_response_id.is_none());
    assert!(final_meta.request_id > meta.request_id);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let replayed = requests[1]
        .iter()
        .find(|m| m.text == "partial answer")
        .unwrap();
    assert_eq!(replayed.reasoning_blocks, partial.reasoning_blocks);
    drop(requests);
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn successful_tool_rounds_break_the_truncation_streak() {
    let root =
        std::env::temp_dir().join(format!("atomcode-truncation-streak-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut streams = VecDeque::new();
    for i in 0..6 {
        streams.push_back(vec![
            StreamEvent::Reasoning(format!("analysis {i}")),
            StreamEvent::Done { truncated: true },
        ]);
        streams.push_back(vec![
            StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                id: format!("call-{i}"),
                name: format!("unavailable-fixture-tool-{i}"),
                arguments: format!(r#"{{"step":{i}}}"#),
            }),
            StreamEvent::Done { truncated: false },
        ]);
    }
    streams.push_back(vec![
        StreamEvent::TextDelta("finished".into()),
        StreamEvent::Done { truncated: false },
    ]);
    let provider = Arc::new(Provider {
        streams: Mutex::new(streams),
        requests: Mutex::new(Vec::new()),
    });
    let mut app = app(&root, provider.clone(), false).await;
    let outcome = run_turn(&app, "start").await.unwrap();
    assert_eq!(outcome.text, "finished", "{outcome:?}");
    assert_eq!(provider.requests.lock().unwrap().len(), 13);
    let requests = provider.requests.lock().unwrap();
    assert!(requests[1].iter().any(|m| m
        .text
        .contains("exhausted its output budget during reasoning")));
    assert!(!requests[1].iter().any(|m| m
        .text
        .contains("writing the remaining content INCREMENTALLY")));
    drop(requests);
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn consecutive_reasoning_only_truncations_stop_without_an_unbounded_retry() {
    let root = std::env::temp_dir().join(format!("atomcode-truncation-cap-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(
            (0..5)
                .map(|_| {
                    vec![
                        StreamEvent::Reasoning("thinking".into()),
                        StreamEvent::Done { truncated: true },
                    ]
                })
                .collect(),
        ),
        requests: Mutex::new(Vec::new()),
    });
    let mut app = app(&root, provider.clone(), false).await;
    run_turn(&app, "start").await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 5);
    let events = app.context().only_session().unwrap().events();
    assert!(events.iter().any(|fact| matches!(&fact.event,
        atomcode_harness::session::SessionEvent::Notice { detail, .. } if detail.contains("尚未生成正文"))));
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn repeated_text_is_only_a_redump_within_a_consecutive_truncation_streak() {
    for break_streak in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "atomcode-truncation-redump-{}-{break_streak}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let text = "unfinished section ".repeat(40);
        let cut = vec![
            StreamEvent::TextDelta(text.clone()),
            StreamEvent::Done { truncated: true },
        ];
        let mut streams = VecDeque::from([cut.clone()]);
        if break_streak {
            streams.push_back(vec![
                StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                    id: "intervening-call".into(),
                    name: "unavailable-fixture-tool".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ]);
        }
        streams.push_back(cut);
        if break_streak {
            streams.push_back(vec![
                StreamEvent::TextDelta("finished".into()),
                StreamEvent::Done { truncated: false },
            ]);
        }
        let provider = Arc::new(Provider {
            streams: Mutex::new(streams),
            requests: Mutex::new(Vec::new()),
        });
        let mut app = app(&root, provider.clone(), false).await;
        let outcome = run_turn(&app, "start").await.unwrap();
        assert_eq!(outcome.text, if break_streak { "finished" } else { &text });
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), if break_streak { 4 } else { 2 });
        assert!(requests[1]
            .iter()
            .any(|m| m.text.contains("resume where you left off")));
        drop(requests);
        app.stop();
        std::fs::remove_dir_all(root).unwrap();
    }
}
