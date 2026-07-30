//! CLAIM: 429 responses are routed to the host's on_rate_limit verdict.
//!
//! Two behavioural proofs:
//!   1. Pause  → emits AgentEvent::RateLimited (NOT Error) + TurnComplete{RateLimited}
//!   2. WaitAndRetry{secs:0} → sleeps 0 s, re-issues the round, turn succeeds with text

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AgentHandle};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::hook::{LifecycleHooks, RateLimitDecision};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::testkit::ScriptedRateLimitHook;
use atomcode_kernel::tool::{ToolDef, ToolRegistry};
use futures::stream::BoxStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

// ── shared helpers ────────────────────────────────────────────────────────────

fn send(text: &str) -> AgentCommand {
    AgentCommand::SendMessage {
        text: text.into(),
        images: vec![],
    }
}

fn spawn_agent(provider: Arc<dyn LlmProvider>, hook: Arc<dyn LifecycleHooks>) -> AgentHandle {
    let reg = ToolRegistry::new();
    Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hooks(hook)
        .build()
        .spawn()
}

// ── test-local provider: first call -> 429, then -> normal success ─────────

struct Once429Provider {
    calls: AtomicU32,
}

impl Once429Provider {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for Once429Provider {
    fn model_name(&self) -> &str {
        "once-429"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(ProviderError {
                retryable: true,
                http_status: Some(429),
                message: "rate limited".into(),
                ..Default::default()
            });
        }
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

// ── test 1: Pause decision → RateLimited event, no Error, TurnComplete{RateLimited}

#[tokio::test]
async fn rate_limit_pause_emits_ratelimited_not_error() {
    let provider = Arc::new(Once429Provider::new());
    let hook = Arc::new(ScriptedRateLimitHook::new(RateLimitDecision::Pause {
        reset_at_display: "18:09".into(),
        reset_label: "5h".into(),
        secs_until_reset: Some(7200),
    }));

    let handle = spawn_agent(provider, hook);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut collected: Vec<AgentEvent> = Vec::new();
    while let Some(ev) = events.recv().await {
        let done = matches!(ev, AgentEvent::TurnComplete { .. });
        collected.push(ev);
        if done {
            break;
        }
    }

    assert!(
        collected
            .iter()
            .any(|e| matches!(e, AgentEvent::RateLimited { .. })),
        "must emit RateLimited: {collected:?}"
    );
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { .. })),
        "must NOT emit Error on pause: {collected:?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            AgentEvent::TurnComplete {
                reason: StopReason::RateLimited
            }
        )),
        "TurnComplete must carry RateLimited reason: {collected:?}"
    );
}

// ── mid-stream 429 provider: open succeeds, stream emits partial text then Error(429) ──

struct MidStream429Provider {
    calls: AtomicU32,
}

/// Opens successfully, then emits a 429 before any model content. This is safe
/// to replay, but repeated occurrences must still consume the turn-owned fuse.
struct EmptyMidStream429Provider {
    calls: AtomicU32,
    succeed_after: u32,
}

#[async_trait]
impl LlmProvider for EmptyMidStream429Provider {
    fn model_name(&self) -> &str {
        "empty-mid-stream-429"
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call <= self.succeed_after {
            return Ok(Box::pin(futures::stream::iter(vec![StreamEvent::Error(
                ProviderError {
                    retryable: true,
                    http_status: Some(429),
                    message: "rate limited before first delta".into(),
                    ..Default::default()
                },
            )])));
        }
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

impl MidStream429Provider {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for MidStream429Provider {
    fn model_name(&self) -> &str {
        "mid-stream-429"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // First call: stream some text then 429 mid-stream
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::Error(ProviderError {
                    retryable: true,
                    http_status: Some(429),
                    message: "rate limited mid-stream".into(),
                    ..Default::default()
                }),
            ])))
        } else {
            // Second call: normal completion continuing from committed partial
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta(" done".into()),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }
}

// ── test 3: a 429 after visible output must not replay the provider request ─

#[tokio::test]
async fn mid_stream_429_after_output_pauses_without_replay() {
    let provider = Arc::new(MidStream429Provider::new());
    let hook = Arc::new(ScriptedRateLimitHook::new(
        RateLimitDecision::WaitAndRetry { secs: 0 },
    ));

    let handle = spawn_agent(provider.clone(), hook);
    handle.commands.send(send("go")).unwrap();

    let commands = handle.commands.clone();
    let mut events = handle.events;
    let mut collected: Vec<AgentEvent> = Vec::new();
    while let Some(ev) = events.recv().await {
        let done = matches!(ev, AgentEvent::TurnComplete { .. });
        collected.push(ev);
        if done {
            break;
        }
    }

    assert!(
        collected
            .iter()
            .any(|e| matches!(e, AgentEvent::RateLimited { .. })),
        "must emit RateLimited on mid-stream 429: {collected:?}"
    );
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { .. })),
        "partial-stream 429 must remain a clean RateLimited terminal: {collected:?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            AgentEvent::TurnComplete {
                reason: StopReason::RateLimited
            }
        )),
        "turn must pause after a mid-stream 429 with visible output: {collected:?}"
    );
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "replaying after visible output can duplicate text"
    );

    commands.send(AgentCommand::Snapshot).unwrap();
    let snapshot = loop {
        match events.recv().await {
            Some(AgentEvent::Snapshot { snapshot }) => break snapshot,
            Some(_) => continue,
            None => panic!("agent stopped before returning its snapshot"),
        }
    };
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.text == "partial"),
        "visible partial output must survive snapshot/resume: {snapshot:?}"
    );
}

#[tokio::test]
async fn empty_mid_stream_429_uses_one_turn_owned_fuse() {
    let provider = Arc::new(EmptyMidStream429Provider {
        calls: AtomicU32::new(0),
        // A broken implementation that resets the incident count on every HTTP 200
        // would reach call 11 and succeed. The correct implementation stops on call 6.
        succeed_after: 10,
    });
    let hook = Arc::new(ScriptedRateLimitHook::new(
        RateLimitDecision::WaitAndRetry { secs: 0 },
    ));
    let handle = spawn_agent(provider.clone(), hook);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut terminal = None;
    while let Some(event) = events.recv().await {
        if let AgentEvent::TurnComplete { reason } = event {
            terminal = Some(reason);
            break;
        }
    }

    assert_eq!(terminal, Some(StopReason::RateLimited));
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        6,
        "five waits plus the fuse-triggering 429 must be the complete incident"
    );
}

// ── test 2: WaitAndRetry{secs:0} → re-issues round, turn succeeds with "ok" ─

#[tokio::test]
async fn rate_limit_wait_then_resumes_turn() {
    let provider = Arc::new(Once429Provider::new());
    let hook = Arc::new(ScriptedRateLimitHook::new(
        RateLimitDecision::WaitAndRetry { secs: 0 },
    ));

    let handle = spawn_agent(provider, hook);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut collected: Vec<AgentEvent> = Vec::new();
    while let Some(ev) = events.recv().await {
        let done = matches!(ev, AgentEvent::TurnComplete { .. });
        collected.push(ev);
        if done {
            break;
        }
    }

    assert!(
        collected
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("ok"))),
        "turn must resume and produce content: {collected:?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            AgentEvent::TurnComplete {
                reason: StopReason::Stopped
            }
        )),
        "turn must end with Stopped after successful retry: {collected:?}"
    );
}
