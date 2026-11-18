use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use atomcode_kernel::event::PolicyIntervention;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolDef, ToolResult};
use futures::stream::{BoxStream, Stream};
use tokio::time::Instant;
use tracing::{Instrument, Span};

#[derive(Clone)]
pub(crate) struct ReviewTiming {
    span: Span,
}

impl ReviewTiming {
    pub(crate) fn new(source_id: Option<&str>) -> Self {
        static NEXT_REVIEW: AtomicU64 = AtomicU64::new(1);
        let review_id = NEXT_REVIEW.fetch_add(1, Ordering::Relaxed);
        Self {
            span: tracing::info_span!(
                "code_review",
                process_id = std::process::id(),
                review_id,
                source_call_id = source_id.unwrap_or_default()
            ),
        }
    }

    pub(crate) fn pass(&self, stage: &str, candidate: Option<usize>) -> Self {
        Self {
            span: tracing::info_span!(parent: &self.span, "review_pass", stage, candidate),
        }
    }

    pub(crate) fn start(&self) -> Timer {
        Timer::start(self.span.clone(), "review pass")
    }

    pub(crate) fn prepared(&self, elapsed_ms: u64, files: usize, diff_bytes: usize) {
        tracing::info!(parent: &self.span, elapsed_ms, files, diff_bytes, "review input prepared");
    }

    pub(crate) fn provider(&self, inner: Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
        Arc::new(TimedProvider {
            parent: self.span.clone(),
            next_request: AtomicU64::new(1),
            inner,
        })
    }

    pub(crate) fn tool(&self, inner: Arc<dyn Tool>) -> Arc<dyn Tool> {
        Arc::new(TimedTool {
            parent: self.span.clone(),
            inner,
        })
    }
}

pub(crate) struct Timer {
    span: Span,
    kind: &'static str,
    start: Instant,
    outcome: &'static str,
}

impl Timer {
    fn start(span: Span, kind: &'static str) -> Self {
        Self {
            span,
            kind,
            start: Instant::now(),
            outcome: "dropped",
        }
    }

    pub(crate) fn finish(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        tracing::info!(
            parent: &self.span,
            operation = self.kind,
            elapsed_ms = self.elapsed_ms(),
            outcome = self.outcome,
            "review timing"
        );
    }
}

struct TimedProvider {
    parent: Span,
    next_request: AtomicU64,
    inner: Arc<dyn LlmProvider>,
}

#[async_trait]
impl LlmProvider for TimedProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    fn effort_levels(&self) -> Vec<String> {
        self.inner.effort_levels()
    }

    fn replays_reasoning(&self) -> bool {
        self.inner.replays_reasoning()
    }

    fn bind_session_id(&self, session_id: &str) {
        self.inner.bind_session_id(session_id)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let request = self.next_request.fetch_add(1, Ordering::Relaxed);
        let span = tracing::info_span!(
            parent: &self.parent, "review_model", request, messages = messages.len()
        );
        let mut timer = Timer::start(span.clone(), "model request");
        match self
            .inner
            .chat_stream(messages, tools, options)
            .instrument(span)
            .await
        {
            Ok(stream) => Ok(Box::pin(TimedStream {
                stream: Some(stream),
                timer,
                saw_content: false,
            })),
            Err(err) => {
                timer.finish("open_error");
                Err(err)
            }
        }
    }
}

struct TimedStream {
    stream: Option<BoxStream<'static, StreamEvent>>,
    timer: Timer,
    saw_content: bool,
}

impl Stream for TimedStream {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        let span = self.timer.span.clone();
        let poll = span.in_scope(|| {
            self.stream
                .as_mut()
                .map_or(Poll::Ready(None), |stream| stream.as_mut().poll_next(cx))
        });
        match &poll {
            Poll::Ready(Some(StreamEvent::Done { truncated })) => {
                self.timer
                    .finish(if *truncated { "truncated" } else { "completed" });
            }
            Poll::Ready(Some(StreamEvent::Error(_))) => self.timer.finish("stream_error"),
            Poll::Ready(None) if self.timer.outcome == "dropped" => self.timer.finish("eof"),
            Poll::Ready(Some(event)) if !self.saw_content => {
                let content = match event {
                    StreamEvent::TextDelta(text) | StreamEvent::Reasoning(text) => !text.is_empty(),
                    StreamEvent::ToolCall(_) | StreamEvent::ToolCallDelta { .. } => true,
                    _ => false,
                };
                if content {
                    self.saw_content = true;
                    tracing::info!(parent: &span, elapsed_ms = self.timer.elapsed_ms(), "review model first content");
                }
            }
            _ => {}
        }
        poll
    }
}

impl Drop for TimedStream {
    fn drop(&mut self) {
        // Adapter cleanup may emit the upstream request's terminal timing.
        self.timer.span.in_scope(|| drop(self.stream.take()));
    }
}

struct TimedTool {
    parent: Span,
    inner: Arc<dyn Tool>,
}

#[async_trait]
impl Tool for TimedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    fn risk(&self, args: &str) -> RiskLevel {
        self.inner.risk(args)
    }

    fn read_only_hint(&self) -> bool {
        self.inner.read_only_hint()
    }

    fn self_bounds_output(&self) -> bool {
        self.inner.self_bounds_output()
    }

    fn parallel_safe(&self, args: &str) -> bool {
        self.inner.parallel_safe(args)
    }

    fn always_grant_scope(&self, args: &str) -> String {
        self.inner.always_grant_scope(args)
    }

    fn allow_all_group(&self, args: &str) -> Option<String> {
        self.inner.allow_all_group(args)
    }

    fn take_policy_intervention(&self, result: &mut ToolResult) -> Option<PolicyIntervention> {
        self.inner.take_policy_intervention(result)
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let span = tracing::info_span!(
            parent: &self.parent, "review_tool",
            name = self.name(), call_id = ctx.progress.source_id().unwrap_or_default()
        );
        let mut timer = Timer::start(span.clone(), "tool execution");
        let result = self.inner.execute(args, ctx).instrument(span).await;
        timer.finish(if result.is_error {
            "error"
        } else {
            "completed"
        });
        result
    }
}
