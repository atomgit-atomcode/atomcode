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
    options: Mutex<Vec<ChatOptions>>,
    /// Answer a request asking for more output tokens than this with a 400, as
    /// an endpoint with its own output limit does.
    refuse_output_above: Option<u32>,
}

#[async_trait]
impl LlmProvider for Provider {
    fn model_name(&self) -> &str {
        "requested-alias"
    }
    fn effort_levels(&self) -> Vec<String> {
        vec!["low".into(), "medium".into(), "high".into()]
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.options.lock().unwrap().push(options.clone());
        if let (Some(limit), Some(asked)) = (self.refuse_output_above, options.max_tokens) {
            if asked > limit {
                return Err(ProviderError {
                    retryable: false,
                    message: format!("max_tokens must be less than or equal to {limit}"),
                    http_status: Some(400),
                    ..ProviderError::default()
                });
            }
        }
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
    app_with_driver(root, provider, resume, None).await
}

struct CheckpointDriver {
    answers: Mutex<VecDeque<Value>>,
    asked: Mutex<Vec<Value>>,
}
struct CheckpointRow(Arc<CheckpointDriver>);
#[async_trait]
impl Plugin for CheckpointRow {
    fn name(&self) -> &'static str {
        "checkpoint-fixture"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tool-driver"]
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        struct Driver(Arc<CheckpointDriver>);
        impl atomcode_harness::seams::ToolDriver for Driver {
            fn progress(&self, _: &str, _: &str) -> atomcode_kernel::tool::ProgressSink {
                atomcode_kernel::tool::ProgressSink::noop()
            }
            fn requester(&self, _: &str) -> Option<atomcode_kernel::request::Requester> {
                let state = self.0.clone();
                Some(atomcode_kernel::request::Requester::from_fn(Arc::new(
                    move |kind, payload| {
                        assert_eq!(kind, atomcode_kernel::OUTPUT_TRUNCATION_CHECKPOINT_KIND);
                        state.asked.lock().unwrap().push(payload);
                        let answer = state
                            .answers
                            .lock()
                            .unwrap()
                            .pop_front()
                            .expect("unexpected checkpoint");
                        Box::pin(async move { answer })
                    },
                )))
            }
        }
        ctx.provide::<atomcode_harness::seams::ToolDriverSvc>(Arc::new(Driver(self.0.clone())))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

async fn app_with_driver(
    root: &std::path::Path,
    provider: Arc<Provider>,
    resume: bool,
    driver: Option<Arc<CheckpointDriver>>,
) -> App {
    let path = bundle::toml_string(&root.to_string_lossy());
    let mut patch = format!(
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
    if driver.is_some() {
        patch.push_str("\n[[insert]]\nname = \"checkpoint-fixture\"\n[[patch]]\nid = \"truncation-recovery\"\nconfig = { checkpoint = true }\n[[patch]]\nid = \"reasoning-effort\"\nconfig = { level = \"high\" }\n");
    }
    let mut catalog = plugins::catalog();
    catalog.register(Arc::new(ProviderRow(provider)));
    if let Some(driver) = driver {
        catalog.register(Arc::new(CheckpointRow(driver)));
    }
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
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
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
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
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

/// An endpoint with its own output limit refuses the raised budget; the
/// request is made again as it was, and the turn goes on.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_refused_budget_raise_falls_back_to_the_original_budget() {
    let root = std::env::temp_dir().join(format!("atomcode-budget-refused-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([
            vec![
                StreamEvent::Reasoning("long analysis".into()),
                StreamEvent::Done { truncated: true },
            ],
            vec![
                StreamEvent::TextDelta("finished".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: Some(atomcode_kernel::agent::DEFAULT_MAX_OUTPUT_TOKENS),
    });
    let mut app = app(&root, provider.clone(), false).await;
    let outcome = run_turn(&app, "start").await.unwrap();
    assert_eq!(outcome.text, "finished", "{outcome:?}");
    assert!(outcome.error.is_none(), "{outcome:?}");
    let options = provider.options.lock().unwrap();
    assert_eq!(
        options.len(),
        3,
        "first, raised (refused), retried as before"
    );
    assert!(options[1].max_tokens > options[0].max_tokens);
    assert_eq!(options[2].max_tokens, options[0].max_tokens);
    let refused = options[1].max_tokens.unwrap();
    drop(options);
    // The notice that announced the raise must not stand alone: the person is
    // told the endpoint refused it, so the screen stops claiming that budget.
    let events = app.context().only_session().unwrap().events();
    assert!(
        events.iter().any(|fact| matches!(
            &fact.event,
            atomcode_harness::session::SessionEvent::Notice { detail, retry: None, .. }
                if detail.contains(&refused.to_string()) && detail.contains("退回原预算")
        )),
        "the refused raise is reported: {events:?}"
    );
    app.stop();
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
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
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
    // Reasoning that ran out of budget gets a larger one for the next request only.
    let options = provider.options.lock().unwrap();
    let base = options[0].max_tokens;
    assert!(
        options[1].max_tokens.unwrap()
            > base.unwrap_or(atomcode_kernel::agent::DEFAULT_MAX_OUTPUT_TOKENS),
        "{:?} -> {:?}",
        base,
        options[1].max_tokens
    );
    assert_eq!(
        options[2].max_tokens, base,
        "the raise is for one request only"
    );
    drop(options);
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn consecutive_reasoning_only_truncations_stop_without_an_unbounded_retry() {
    let root = std::env::temp_dir().join(format!("atomcode-truncation-cap-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(
            (0..2)
                .map(|_| {
                    vec![
                        StreamEvent::Reasoning("thinking".into()),
                        StreamEvent::Done { truncated: true },
                    ]
                })
                .collect(),
        ),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let mut app = app(&root, provider.clone(), false).await;
    run_turn(&app, "start").await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let events = app.context().only_session().unwrap().events();
    assert!(events.iter().any(|fact| matches!(&fact.event,
        atomcode_harness::session::SessionEvent::Notice { detail, .. } if detail.contains("尚未生成正文"))));
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

fn reasoning_stop() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Reasoning("long unfinished analysis".into()),
        StreamEvent::Done { truncated: false },
    ]
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn reasoning_only_stops_end_without_an_output_limit_checkpoint() {
    let root = std::env::temp_dir().join(format!(
        "atomcode-reasoning-stop-checkpoint-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new((0..2).map(|_| reasoning_stop()).collect()),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let driver = Arc::new(CheckpointDriver {
        // Any checkpoint is a failure, even if it would have been declined.
        answers: Mutex::new(VecDeque::new()),
        asked: Mutex::new(Vec::new()),
    });
    let mut app = app_with_driver(&root, provider.clone(), false, Some(driver.clone())).await;
    let outcome = run_turn(&app, "start").await.unwrap();
    assert_eq!(outcome.stop, atomcode_harness::seams::StopReason::Stopped);
    assert!(outcome.error.is_none());
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(driver.asked.lock().unwrap().is_empty());
    let events = app.context().only_session().unwrap().events();
    assert!(events.iter().any(|fact| matches!(&fact.event,
        atomcode_harness::session::SessionEvent::Notice { detail, .. }
            if detail.contains("只输出了思考") && detail.contains("本轮已结束"))));
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_suppressed_reasoning_reply_to_an_internal_note_keeps_its_usage() {
    let root = std::env::temp_dir().join(format!(
        "atomcode-internal-reasoning-usage-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let usage = TokenUsage {
        prompt: 100,
        cached: 70,
        completion: 25,
    };
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([vec![
            StreamEvent::Reasoning("Nothing to add to this note".into()),
            StreamEvent::Usage(usage),
            StreamEvent::Done { truncated: false },
        ]])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let mut app = app(&root, provider.clone(), false).await;
    let agent = atomcode_harness::create_agent(&app).await.unwrap();
    agent.send_from(
        "verify if needed",
        atomcode_harness::agent::MessageOrigin::Internal,
    );
    let outcome = atomcode_harness::drive(&app, &agent).await.unwrap();
    assert_eq!(outcome.stop, atomcode_harness::seams::StopReason::Stopped);
    assert!(outcome.error.is_none());
    assert!(outcome.text.is_empty());
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let events = app.context().only_session().unwrap().events();
    let receipts: Vec<_> = events
        .iter()
        .filter_map(|fact| match &fact.event {
            atomcode_harness::session::SessionEvent::Usage { usage, .. } => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(
        receipts,
        vec![usage],
        "exactly one receipt survives suppression"
    );
    assert!(!events.iter().any(|fact| matches!(
        &fact.event,
        atomcode_harness::session::SessionEvent::AssistantMessage { .. }
    )));
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

/// A gateway that closes a long request at its own time limit reports a plain
/// stop with reasoning and nothing else. Replaying that request reasons the
/// same way again; the model is asked to act on what it already worked out.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_reasoning_only_stop_is_nudged_to_act_instead_of_replayed() {
    let root = std::env::temp_dir().join(format!(
        "atomcode-reasoning-stop-nudge-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([
            reasoning_stop(),
            vec![
                StreamEvent::TextDelta("acted".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let mut app = app(&root, provider.clone(), false).await;
    let outcome = run_turn(&app, "start").await.unwrap();
    assert_eq!(outcome.text, "acted", "{outcome:?}");
    assert!(outcome.error.is_none(), "{outcome:?}");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].iter().any(|m| m
        .text
        .contains("ended after reasoning, with no answer and no tool call")));
    drop(requests);
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn repeated_reasoning_only_stops_end_the_turn_without_a_provider_error() {
    let root = std::env::temp_dir().join(format!(
        "atomcode-reasoning-stop-cap-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new((0..2).map(|_| reasoning_stop()).collect()),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let mut app = app(&root, provider.clone(), false).await;
    let outcome = run_turn(&app, "start").await.unwrap();
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        2,
        "one nudge, never a blind replay of the same request"
    );
    assert_eq!(
        outcome.stop,
        atomcode_harness::seams::StopReason::Stopped,
        "{outcome:?}"
    );
    assert!(outcome.error.is_none(), "{outcome:?}");
    let events = app.context().only_session().unwrap().events();
    assert!(events.iter().any(|fact| matches!(&fact.event,
        atomcode_harness::session::SessionEvent::Notice { detail, .. } if detail.contains("只输出了思考"))));
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
            options: Mutex::new(Vec::new()),
            refuse_output_above: None,
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

fn reasoning_cut() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Reasoning("unfinished reasoning".into()),
        StreamEvent::Done { truncated: true },
    ]
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn lowering_effort_is_explicit_and_applies_to_only_one_recovery_request() {
    use atomcode_kernel::provider::ReasoningEffort::{High, Medium};
    let root = std::env::temp_dir().join(format!("atomcode-effort-once-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([
            reasoning_cut(),
            reasoning_cut(),
            vec![
                StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                    id: "progress".into(),
                    name: "fixture-missing-tool".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("finished".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("next turn".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let driver = Arc::new(CheckpointDriver {
        answers: Mutex::new(VecDeque::from([
            serde_json::json!({"continue":true,"lower_effort":true}),
        ])),
        asked: Mutex::new(Vec::new()),
    });
    let mut app = app_with_driver(&root, provider.clone(), false, Some(driver.clone())).await;
    assert_eq!(run_turn(&app, "start").await.unwrap().text, "finished");
    assert_eq!(run_turn(&app, "next").await.unwrap().text, "next turn");
    let levels: Vec<_> = provider
        .options
        .lock()
        .unwrap()
        .iter()
        .map(|o| o.reasoning_effort)
        .collect();
    assert_eq!(
        levels,
        vec![Some(High), Some(High), Some(Medium), Some(High), Some(High)]
    );
    let asks = driver.asked.lock().unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0]["attempts"], 1);
    assert_eq!(asks[0]["max_attempts"], 1);
    assert_eq!(asks[0]["lower_effort"], "medium");
    drop(asks);
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn continuing_at_the_reasoning_checkpoint_grants_one_request_without_resetting_the_budget() {
    let root =
        std::env::temp_dir().join(format!("atomcode-effort-continue-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([
            reasoning_cut(),
            reasoning_cut(),
            reasoning_cut(),
        ])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let driver = Arc::new(CheckpointDriver {
        answers: Mutex::new(VecDeque::from([
            serde_json::json!({"continue":true}),
            Value::Null,
        ])),
        asked: Mutex::new(Vec::new()),
    });
    let mut app = app_with_driver(&root, provider.clone(), false, Some(driver.clone())).await;
    run_turn(&app, "start").await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert_eq!(driver.asked.lock().unwrap().len(), 2);
    assert!(provider
        .options
        .lock()
        .unwrap()
        .iter()
        .all(|o| o.reasoning_effort == Some(atomcode_kernel::provider::ReasoningEffort::High)));
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_forged_lower_effort_choice_cannot_continue_when_no_lower_level_is_available() {
    let root = std::env::temp_dir().join(format!(
        "atomcode-effort-unavailable-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([reasoning_cut(), reasoning_cut()])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let driver = Arc::new(CheckpointDriver {
        answers: Mutex::new(VecDeque::from([
            serde_json::json!({"continue":true,"lower_effort":true}),
        ])),
        asked: Mutex::new(Vec::new()),
    });
    let mut app = app_with_driver(&root, provider.clone(), false, Some(driver.clone())).await;
    app.patch(
        &Layer::from_toml("[[patch]]\nid = \"reasoning-effort\"\nconfig = { level = \"low\" }")
            .unwrap(),
    )
    .await
    .unwrap();
    run_turn(&app, "start").await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(driver.asked.lock().unwrap()[0]["lower_effort"].is_null());
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_malformed_lower_effort_choice_cannot_silently_retry_at_the_original_effort() {
    let root =
        std::env::temp_dir().join(format!("atomcode-effort-malformed-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([reasoning_cut(), reasoning_cut()])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let driver = Arc::new(CheckpointDriver {
        answers: Mutex::new(VecDeque::from([
            serde_json::json!({"continue":true,"lower_effort":"medium"}),
        ])),
        asked: Mutex::new(Vec::new()),
    });
    let mut app = app_with_driver(&root, provider.clone(), false, Some(driver.clone())).await;
    run_turn(&app, "start").await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(driver.asked.lock().unwrap()[0]["lower_effort"], "medium");
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_projection_keeps_real_numbers_and_reason_and_degrades_old_notices_to_warnings() {
    use atomcode_harness::session::{NoticeKind, SessionEvent};
    use atomcode_kernel::{event::AgentEvent, session::RetryAttempt};
    let events = vec![
        SessionEvent::Notice {
            turn: 1,
            notice: NoticeKind::OutputTruncated,
            detail: "reasoning budget exhausted".into(),
            retry: Some(RetryAttempt {
                attempt: 1,
                max_attempts: 1,
                backoff_secs: 0,
            }),
        },
        SessionEvent::Notice {
            turn: 1,
            notice: NoticeKind::OutputTruncated,
            detail: "historical notice".into(),
            retry: None,
        },
    ];
    let projected = plugins::handle::replay(&events, 1000000);
    assert!(
        matches!(&projected[0], AgentEvent::OutputTruncationRecovery {attempt:1,max_attempts:1,reason:Some(reason)} if reason == "reasoning budget exhausted")
    );
    assert!(matches!(&projected[1], AgentEvent::Warning(reason) if reason == "historical notice"));
    let mut old = serde_json::to_value(&projected[0]).unwrap();
    old["OutputTruncationRecovery"]
        .as_object_mut()
        .unwrap()
        .remove("reason");
    assert!(matches!(
        serde_json::from_value::<AgentEvent>(old).unwrap(),
        AgentEvent::OutputTruncationRecovery { reason: None, .. }
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn incomplete_tool_calls_cannot_bypass_the_no_progress_recovery_limit() {
    let root =
        std::env::temp_dir().join(format!("atomcode-incomplete-limit-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let partial = vec![
        StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
            id: "incomplete".into(),
            name: "write_file".into(),
            arguments: format!(
                "{{\"file_path\":\"{}\",\"content\":\"unfinished",
                root.join("unsafe.txt").display()
            ),
        }),
        StreamEvent::Done { truncated: true },
    ];
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([partial.clone(), partial])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let mut app = app(&root, provider.clone(), false).await;
    let outcome = run_turn(&app, "start").await.unwrap();
    assert_eq!(outcome.stop, atomcode_harness::seams::StopReason::Stopped);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(!root.join("unsafe.txt").exists());
    app.stop();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn remounting_jsonl_keeps_persisting_the_live_session() {
    let root = std::env::temp_dir().join(format!("atomcode-jsonl-remount-{}", std::process::id()));
    let new_root = root.join("new-store");
    let provider = Arc::new(Provider {
        streams: Mutex::new(VecDeque::from([
            vec![
                StreamEvent::TextDelta("before remount".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("saved after remount".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])),
        requests: Mutex::new(Vec::new()),
        options: Mutex::new(Vec::new()),
        refuse_output_above: None,
    });
    let mut app = app(&root, provider, false).await;
    run_turn(&app, "open the session").await.unwrap();
    let id = app.context().only_session().unwrap().id().to_string();
    let path = bundle::toml_string(&new_root.to_string_lossy());
    let patch = Layer::from_toml(&format!(
        "[[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {path}, project_root = {path} }}"
    ))
    .unwrap();
    app.patch(&patch).await.unwrap();

    assert_eq!(app.context().only_session().unwrap().id(), id);
    let store = app
        .context()
        .service::<atomcode_harness::seams::SessionPersistenceSvc>()
        .unwrap();
    run_turn(&app, "persist this").await.unwrap();
    drop(app);
    let events = store.load(&id).await.unwrap();
    let messages = atomcode_kernel::session::derive_messages_with_meta(&events);
    assert!(messages.iter().any(|m| m.text == "saved after remount"));
    std::fs::remove_dir_all(root).unwrap();
}
