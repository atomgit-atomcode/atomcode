//! CLAIM 15: the provider stream is FALLIBLE. A failed open, a mid-stream error,
//! reasoning content, and a `finish_reason=length` truncation are all first-class
//! — none silently degrades into an empty SUCCESSFUL turn.

use atomcode_kernel::event::AgentEvent;
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::testkit::{RecorderHook, ScriptedProvider};
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

// A `chat_stream` that fails to OPEN (Err) must surface an Error event and a
// TurnComplete, fire the on_error hook, and NOT fabricate a bogus assistant
// message or an empty-success illusion.
#[tokio::test]
async fn provider_open_error_surfaces_and_fails_turn() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(ScriptedProvider::open_error(ProviderError {
        retryable: false,
        message: "auth failed (401)".into(),
        ..Default::default()
    }));
    let recorder = Arc::new(RecorderHook::new());
    let log = recorder.log.clone();

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut error_msg: Option<String> = None;
    let mut completed = false;
    let mut got_text = false;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Error { message, .. } => error_msg = Some(message),
            AgentEvent::TextDelta(_) => got_text = true,
            AgentEvent::TurnComplete { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        error_msg.as_deref(),
        Some("auth failed (401)"),
        "an open failure must surface as AgentEvent::Error"
    );
    assert!(completed, "the turn must still complete (cleanly fail), not hang");
    assert!(!got_text, "no bogus assistant text on an open failure");
    assert!(
        log.lock().unwrap().contains(&"on_error".to_string()),
        "the on_error hook must fire on an open failure"
    );
}

// A stream that emits some TextDelta then a mid-stream Error must surface the
// error, fire on_error, and end the turn WITHOUT a fake empty-success.
#[tokio::test]
async fn mid_stream_error_surfaces_and_fails_turn() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(ScriptedProvider::events(vec![
        StreamEvent::TextDelta("partial".into()),
        StreamEvent::Error(ProviderError {
            retryable: true,
            message: "upstream 503".into(),
            ..Default::default()
        }),
        // A trailing Done is scripted but must NEVER be reached — the error ends
        // the turn first.
        StreamEvent::Done { truncated: false },
    ]));
    let recorder = Arc::new(RecorderHook::new());
    let log = recorder.log.clone();

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut error_msg: Option<String> = None;
    let mut complete_count = 0;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Error { message, .. } => error_msg = Some(message),
            AgentEvent::TurnComplete { .. } => {
                complete_count += 1;
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        error_msg.as_deref(),
        Some("upstream 503"),
        "a mid-stream error must surface as AgentEvent::Error"
    );
    assert_eq!(complete_count, 1, "the failed turn ends with exactly one TurnComplete");
    assert!(
        log.lock().unwrap().contains(&"on_error".to_string()),
        "the on_error hook must fire on a mid-stream error"
    );
    // A mid-stream failure must NOT fall through to a normal-success completion:
    // on_model_response (the success path) must NOT have run.
    assert!(
        !log.lock().unwrap().contains(&"on_model_response".to_string()),
        "a failed turn must NOT run the success path (no bogus assistant message)"
    );
}

// A Reasoning stream event must surface to the driver as AgentEvent::Reasoning.
#[tokio::test]
async fn reasoning_is_emitted() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(ScriptedProvider::events(vec![
        StreamEvent::Reasoning("let me think...".into()),
        StreamEvent::TextDelta("answer".into()),
        StreamEvent::Done { truncated: false },
    ]));
    let recorder = Arc::new(RecorderHook::new());

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut reasoning: Option<String> = None;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Reasoning(t) => reasoning = Some(t),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    assert_eq!(
        reasoning.as_deref(),
        Some("let me think..."),
        "a Reasoning stream event must surface as AgentEvent::Reasoning"
    );
}

// A Done { truncated: true } must surface a Warning, and the turn still finishes
// the round normally (no continuation — that is a separate follow-up task).
#[tokio::test]
async fn truncated_done_emits_warning() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(ScriptedProvider::events(vec![
        StreamEvent::TextDelta("a long cut-off answer".into()),
        StreamEvent::Done { truncated: true },
    ]));
    let recorder = Arc::new(RecorderHook::new());

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut warning: Option<String> = None;
    let mut completed = false;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warning = Some(w),
            AgentEvent::TurnComplete { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        warning.as_deref().is_some_and(|w| w.contains("truncated")),
        "a truncated Done must surface an AgentEvent::Warning; got {warning:?}"
    );
    assert!(completed, "a truncated response still finishes the round normally");
}

// --- local helpers (keep each test terse; mirror spike_claims.rs style) ---

use atomcode_kernel::agent::{Agent, AgentHandle};
use atomcode_kernel::event::AgentCommand;
use atomcode_kernel::provider::LlmProvider;

fn send(text: &str) -> AgentCommand {
    AgentCommand::SendMessage { text: text.into(), images: vec![] }
}

fn spawn_agent(
    provider: Arc<dyn LlmProvider>,
    reg: ToolRegistry,
    recorder: Arc<RecorderHook>,
) -> AgentHandle {
    Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .hooks(recorder)
        .build()
        .spawn()
}

// --- agent-layer visible retry (second tier above the transport retry) ---

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::tool::ToolDef;
use futures::stream::BoxStream;
use std::sync::atomic::{AtomicU32, Ordering};

/// A provider whose open FAILS the first `fail_first` times (returning a clone of
/// `err`), then SUCCEEDS with a bare `Done`. Counts total opens so a test can
/// assert how many re-opens the agent loop drove.
struct FlakyProvider {
    fail_first: u32,
    err: ProviderError,
    calls: AtomicU32,
}

impl FlakyProvider {
    fn new(fail_first: u32, err: ProviderError) -> Self {
        Self { fail_first, err, calls: AtomicU32::new(0) }
    }
}

#[async_trait]
impl LlmProvider for FlakyProvider {
    fn model_name(&self) -> &str {
        "flaky"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_first {
            return Err(self.err.clone());
        }
        Ok(Box::pin(futures::stream::iter(vec![StreamEvent::Done { truncated: false }])))
    }
}

// A RETRYABLE open failure must NOT hard-fail the turn: the agent loop re-opens
// the same round, emits a VISIBLE Warning ("…秒后重试…") per attempt, and the turn
// SUCCEEDS once the provider recovers. `start_paused` advances the 3/6/9s backoff
// on virtual time so the test is instant.
#[tokio::test(start_paused = true)]
async fn retryable_open_failure_retries_visibly_then_succeeds() {
    let reg = ToolRegistry::new();
    // Fail twice (→ two retries, 3s + 6s), then succeed on the third open.
    let provider = Arc::new(FlakyProvider::new(
        2,
        ProviderError {
            retryable: true,
            message: "open failed: dns error".into(),
            ..Default::default()
        },
    ));
    let calls_seen = provider.clone();
    let recorder = Arc::new(RecorderHook::new());

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut warnings: Vec<String> = Vec::new();
    let mut errored = false;
    let mut stop: Option<String> = None;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warnings.push(w),
            AgentEvent::Error { .. } => errored = true,
            AgentEvent::TurnComplete { reason } => {
                stop = Some(format!("{reason:?}"));
                break;
            }
            _ => {}
        }
    }

    let retry_notes: Vec<&String> = warnings.iter().filter(|w| w.contains("秒后重试")).collect();
    assert_eq!(
        retry_notes.len(),
        2,
        "two retryable failures must emit two visible retry notices; got {warnings:?}"
    );
    assert!(
        retry_notes[0].contains("(1/3)") && retry_notes[1].contains("(2/3)"),
        "retry notices must be numbered 1/3 then 2/3; got {retry_notes:?}"
    );
    assert!(!errored, "a recovered turn must NOT surface a terminal Error");
    assert_eq!(stop.as_deref(), Some("Stopped"), "the turn must end successfully");
    assert_eq!(
        calls_seen.calls.load(Ordering::SeqCst),
        3,
        "two failed opens + one successful open = three chat_stream calls"
    );
}

// A NON-retryable open failure (auth/400) must FAIL FAST: no retry notice, exactly
// one open, terminal Error — never spin the 18s retry budget on an error that
// cannot recover.
#[tokio::test(start_paused = true)]
async fn non_retryable_open_failure_fails_fast() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(FlakyProvider::new(
        99, // would fail forever, but classification must stop it at the first open
        ProviderError {
            retryable: false,
            message: "auth failed (401)".into(),
            http_status: Some(401),
            ..Default::default()
        },
    ));
    let calls_seen = provider.clone();
    let recorder = Arc::new(RecorderHook::new());

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut warnings: Vec<String> = Vec::new();
    let mut error_msg: Option<String> = None;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warnings.push(w),
            AgentEvent::Error { message, .. } => error_msg = Some(message),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }

    assert!(
        !warnings.iter().any(|w| w.contains("秒后重试")),
        "a non-retryable error must emit NO retry notice; got {warnings:?}"
    );
    assert_eq!(
        error_msg.as_deref(),
        Some("auth failed (401)"),
        "the terminal error must surface verbatim"
    );
    assert_eq!(
        calls_seen.calls.load(Ordering::SeqCst),
        1,
        "a non-retryable error must open exactly once (fail fast)"
    );
}
