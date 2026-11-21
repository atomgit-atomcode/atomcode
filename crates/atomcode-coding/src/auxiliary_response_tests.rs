//! Auxiliary readers must not turn partial/unsafe streams into user actions or verdicts.
use crate::controllers::{evaluate_goal, GoalResult};
use crate::next_prompt_suggestion::{
    generate_next_prompt_suggestion, sample_side_call, why_nothing, NoSuggestion,
};
use crate::recap::generate_recap;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::{ToolCall, ToolDef};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn unsafe_streams(text: &str) -> Vec<Vec<StreamEvent>> {
    let prefix = || StreamEvent::TextDelta(text.into());
    let end = || StreamEvent::Done { truncated: false };
    vec![
        vec![prefix()],
        vec![prefix(), StreamEvent::Done { truncated: true }],
        vec![prefix(), StreamEvent::Malformed, end()],
        vec![
            prefix(),
            StreamEvent::ToolCall(ToolCall {
                id: "never-run".into(),
                name: "write_file".into(),
                arguments: "{}".into(),
            }),
            end(),
        ],
        vec![
            prefix(),
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: Some("write_file".into()),
                arguments: "{".into(),
            },
            end(),
        ],
        vec![
            StreamEvent::TextDelta(format!("]<]minimax[>[<tool_call>\n{text}")),
            end(),
        ],
        vec![prefix(), StreamEvent::Error(ProviderError::default())],
    ]
}
fn history() -> Vec<Message> {
    vec![
        Message::user("修复错误"),
        Message::assistant("修改完成", vec![]),
    ]
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn incomplete_or_unsafe_side_output_is_neither_a_suggestion_nor_a_recap() {
    for (i, events) in unsafe_streams("继续处理").into_iter().enumerate() {
        let provider = Arc::new(RecordingProvider::new(vec![events.clone()]));
        assert!(
            generate_next_prompt_suggestion(provider.clone(), &history())
                .await
                .is_none(),
            "suggestion case {i}"
        );
        assert_eq!(provider.recorded().len(), 1);
        let provider = Arc::new(RecordingProvider::new(vec![events]));
        assert!(
            generate_recap(Some(provider.clone()), &history(), Ok(None))
                .await
                .is_none(),
            "recap case {i}"
        );
        assert_eq!(provider.recorded().len(), 1);
    }
    assert_eq!(why_nothing("partial", true), NoSuggestion::CutOff);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn malformed_or_tool_streams_cannot_confirm_a_goal() {
    for (i, mut events) in unsafe_streams("Verdict: yes completed")
        .into_iter()
        .enumerate()
    {
        events.insert(
            0,
            StreamEvent::Usage(TokenUsage {
                prompt: 100,
                completion: 5,
                cached: 20,
            }),
        );
        let provider = Arc::new(RecordingProvider::new(vec![events]));
        let outcome = evaluate_goal(
            7,
            11,
            13,
            provider.clone(),
            "finish".into(),
            "work".into(),
            String::new(),
            CancellationToken::new(),
            Duration::from_secs(15),
        )
        .await;
        assert!(!matches!(outcome.result, GoalResult::Met(_)), "case {i}");
        assert_eq!(
            (
                outcome.generation,
                outcome.controller_id,
                outcome.async_epoch
            ),
            (7, 11, 13)
        );
        assert_eq!(outcome.requests, 1);
        assert!(!outcome.evidence_expanded);
        assert_eq!(
            outcome.usage,
            Some(TokenUsage {
                prompt: 100,
                completion: 5,
                cached: 20
            })
        );
        assert_eq!(provider.recorded().len(), 1);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn side_output_memory_is_bounded_without_a_reasoning_token_cap() {
    for (events, accepted) in [
        (
            vec![
                StreamEvent::TextDelta("x".repeat(32768)),
                StreamEvent::Done { truncated: false },
            ],
            true,
        ),
        (
            vec![
                StreamEvent::TextDelta("x".repeat(32769)),
                StreamEvent::Done { truncated: false },
            ],
            false,
        ),
        (
            vec![
                StreamEvent::TextDelta("x".repeat(32767)),
                StreamEvent::TextDelta("🙂".into()),
                StreamEvent::Done { truncated: false },
            ],
            false,
        ),
    ] {
        let provider = Arc::new(RecordingProvider::new(vec![events]));
        assert_eq!(
            sample_side_call(
                provider.clone(),
                "sample".into(),
                Duration::from_secs(15),
                "test"
            )
            .await
            .is_some(),
            accepted
        );
        assert!(provider.recorded_options()[0].max_tokens.is_none());
        assert_eq!(provider.recorded().len(), 1);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn complete_auxiliary_responses_still_work() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::Reasoning("thinking".into()),
        StreamEvent::TextDelta("继续处理".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    assert_eq!(
        generate_next_prompt_suggestion(provider, &history())
            .await
            .as_deref(),
        Some("继续处理")
    );
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("正在修复。下一步运行测试。".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    assert_eq!(
        generate_recap(Some(provider), &history(), Ok(None))
            .await
            .as_deref(),
        Some("正在修复。下一步运行测试。")
    );
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("Verdict: yes verified".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    assert!(
        matches!(evaluate_goal(1,2,3,provider,"finish".into(),"work".into(),String::new(),CancellationToken::new(),Duration::from_secs(15)).await.result,GoalResult::Met(reason) if reason=="verified")
    );
}

struct Flood {
    opened: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl LlmProvider for Flood {
    fn model_name(&self) -> &str {
        "always-ready-metadata"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        self.opened.notify_one();
        Ok(Box::pin(futures::stream::repeat(StreamEvent::Usage(
            Default::default(),
        ))))
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn continuous_metadata_cannot_starve_the_sampler_deadline() {
    let opened = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(Flood {
        opened: opened.clone(),
    });
    let clock = async {
        opened.notified().await;
        tokio::time::advance(Duration::from_secs(2)).await;
    };
    let began = tokio::time::Instant::now();
    let (result, ()) = tokio::join!(
        sample_side_call(provider, "sample".into(), Duration::from_secs(1), "test"),
        clock
    );
    assert!(result.is_none());
    assert_eq!(began.elapsed(), Duration::from_secs(2));
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn continuous_metadata_cannot_starve_evaluator_timeout_or_owner_cancellation() {
    for cancel_it in [false, true] {
        let opened = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(Flood {
            opened: opened.clone(),
        });
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        let controller = async {
            opened.notified().await;
            if cancel_it {
                signal.cancel();
            } else {
                tokio::time::advance(Duration::from_secs(2)).await;
            }
        };
        let (outcome, ()) = tokio::join!(
            evaluate_goal(
                7,
                11,
                13,
                provider,
                "finish".into(),
                "work".into(),
                String::new(),
                cancel,
                Duration::from_secs(1)
            ),
            controller
        );
        if cancel_it {
            assert!(
                matches!(outcome.result,GoalResult::Error(reason) if reason.contains("cancelled"))
            );
        } else {
            assert!(matches!(outcome.result, GoalResult::RetryableError { .. }));
        }
        assert_eq!(
            (
                outcome.generation,
                outcome.controller_id,
                outcome.async_epoch
            ),
            (7, 11, 13)
        );
        assert_eq!(outcome.requests, 1);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn provider_error_diagnostics_do_not_dump_untrusted_error_payloads() {
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<String>>>);
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    let _ = write!(self.0, "{}={:?} ", field.name(), value);
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    struct Failing(bool, u16);
    #[async_trait::async_trait]
    impl LlmProvider for Failing {
        fn model_name(&self) -> &str {
            "failing"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[ToolDef],
            _: &ChatOptions,
        ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
            let error = ProviderError {
                message: "PRIVATE_ERROR_PAYLOAD".into(),
                code: Some("PRIVATE_UPSTREAM_CODE".into()),
                http_status: Some(self.1),
                retryable: self.1 != 401,
                retry_after_secs: Some(17),
                ..Default::default()
            };
            if self.0 {
                Err(error)
            } else {
                Ok(Box::pin(futures::stream::iter([StreamEvent::Error(error)])))
            }
        }
    }
    let capture = Capture::default();
    let guard = tracing::subscriber::set_default(capture.clone());
    for open_error in [true, false] {
        assert!(sample_side_call(
            Arc::new(Failing(open_error, 401)),
            "PRIVATE_PROMPT".into(),
            Duration::from_secs(1),
            "test"
        )
        .await
        .is_none());
    }
    for status in [401, 429, 503] {
        for open_error in [true, false] {
            let outcome = evaluate_goal(
                7,
                11,
                13,
                Arc::new(Failing(open_error, status)),
                "PRIVATE_PROMPT".into(),
                "work".into(),
                String::new(),
                CancellationToken::new(),
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(
                (
                    outcome.generation,
                    outcome.controller_id,
                    outcome.async_epoch
                ),
                (7, 11, 13)
            );
            assert_eq!(outcome.requests, 1);
            assert!(!outcome.evidence_expanded && !outcome.evidence_exhausted);
            let message = match outcome.result {
                GoalResult::RetryableError {
                    message,
                    retry_after_secs,
                } => {
                    assert_ne!(status, 401);
                    assert_eq!(retry_after_secs, Some(17));
                    message
                }
                GoalResult::Error(message) => {
                    assert_eq!(status, 401);
                    message
                }
                _ => panic!("provider failure must remain a failure"),
            };
            assert!(message.contains(&status.to_string()), "{message}");
            assert!(!message.contains("PRIVATE_"), "{message}");
        }
    }
    drop(guard);
    let log = capture.0.lock().unwrap().join("\n");
    assert!(log.contains("http_status") && log.contains("401"), "{log}");
    assert!(log.contains("has_error_code"), "{log}");
    assert!(!log.contains("PRIVATE_"), "{log}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_native_unknown_verdict_cannot_trigger_evidence_expansion() {
    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta(
            "]<]minimax[>[<tool_call>\nVerdict: unknown need more evidence".into(),
        ),
        StreamEvent::Done { truncated: false },
    ]]));
    let result = crate::controllers::evaluate_goal_with_evidence(
        7,
        11,
        13,
        provider.clone(),
        "finish".into(),
        "work".into(),
        String::new(),
        CancellationToken::new(),
        Duration::from_secs(15),
        Some("bounded evidence".into()),
        false,
    )
    .await;
    assert!(matches!(result.result, GoalResult::Inconclusive(_)));
    assert_eq!(result.requests, 1);
    assert!(!result.evidence_expanded && !result.evidence_exhausted);
    assert_eq!(provider.recorded().len(), 1);
}
