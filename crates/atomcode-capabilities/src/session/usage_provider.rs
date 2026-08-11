use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::ToolDef;
use futures::stream::{BoxStream, StreamExt};

use super::{DetachedUsageRecorder, TokenBreakdown};

/// Records usage for an out-of-loop provider call without changing the stream
/// observed by its consumer. Persistence is best-effort: a metadata I/O failure
/// must not turn an otherwise successful model response into a provider failure.
pub struct UsageRecordingProvider {
    inner: Arc<dyn LlmProvider>,
    recorder: DetachedUsageRecorder,
}

impl UsageRecordingProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, recorder: DetachedUsageRecorder) -> Self {
        Self { inner, recorder }
    }
}

#[async_trait]
impl LlmProvider for UsageRecordingProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }

    fn bind_session_id(&self, session_id: &str) {
        self.inner.bind_session_id(session_id);
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let inner = self.inner.chat_stream(messages, tools, options).await?;
        let recorder = self.recorder.clone();
        let stream = futures::stream::unfold(
            (inner, TokenUsage::default(), Some(recorder)),
            |(mut stream, mut usage, mut recorder)| async move {
                match stream.next().await {
                    Some(event) => {
                        if let StreamEvent::Usage(next) = &event {
                            usage.merge_max(*next);
                        }
                        if matches!(event, StreamEvent::Done { .. }) {
                            if let Some(recorder) = recorder.take() {
                                let cached = usage.cached.min(usage.prompt);
                                let _ = recorder.record(TokenBreakdown {
                                    input: u64::from(usage.prompt.saturating_sub(cached)),
                                    output: u64::from(usage.completion),
                                    cached_input: u64::from(cached),
                                });
                            }
                        }
                        Some((event, (stream, usage, recorder)))
                    }
                    None => {
                        if let Some(recorder) = recorder.take() {
                            let cached = usage.cached.min(usage.prompt);
                            let _ = recorder.record(TokenBreakdown {
                                input: u64::from(usage.prompt.saturating_sub(cached)),
                                output: u64::from(usage.completion),
                                cached_input: u64::from(cached),
                            });
                        }
                        None
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::snapshot::SnapshotPersistenceStatus;
    use crate::session::{aggregate_session_cost, SessionManager, SessionMeta};
    use futures::StreamExt;

    struct Canned;

    #[async_trait]
    impl LlmProvider for Canned {
        fn model_name(&self) -> &str {
            "child-model"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::Usage(TokenUsage {
                    prompt: 10,
                    completion: 4,
                    cached: 3,
                }),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }

    #[tokio::test]
    async fn records_out_of_loop_usage_in_session_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(dir.path()));
        manager
            .write_meta(&SessionMeta::new("s1", "/p", 1))
            .unwrap();
        let provider = UsageRecordingProvider::new(
            Arc::new(Canned),
            DetachedUsageRecorder::new(manager.clone(), "s1", "fast", "child-model"),
        );

        let mut stream = provider
            .chat_stream(&[], &[], &ChatOptions::default())
            .await
            .unwrap();
        while stream.next().await.is_some() {}
        let mut second = provider
            .chat_stream(&[], &[], &ChatOptions::default())
            .await
            .unwrap();
        while second.next().await.is_some() {}

        let meta = manager.read_meta("s1").unwrap();
        assert_eq!(meta.turn_count, 0);
        assert_eq!(meta.detached_model_usage.len(), 1);
        let report = aggregate_session_cost(&meta);
        assert_eq!(report.models[0].tokens.input, 14);
        assert_eq!(report.models[0].tokens.cached_input, 6);
        assert_eq!(report.models[0].tokens.output, 8);
    }

    #[test]
    fn persistence_failure_is_reported_without_changing_provider_result_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(dir.path()));
        let status = SnapshotPersistenceStatus::default();
        let recorder = DetachedUsageRecorder::new(manager, "missing", "fast", "child-model")
            .with_persistence_status(status.clone());

        assert!(recorder
            .record(TokenBreakdown {
                input: 1,
                ..TokenBreakdown::default()
            })
            .is_err());
        assert!(status
            .take_cost_warning()
            .is_some_and(|warning| warning.contains("/cost may be incomplete")));
    }
}
