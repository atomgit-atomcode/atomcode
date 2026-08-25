//! CLAIM 15: the provider stream is FALLIBLE. A failed open, a mid-stream error,
//! reasoning content, and a `finish_reason=length` truncation are all first-class
//! — none silently degrades into an empty SUCCESSFUL turn.

use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::testkit::{MockProvider, RecorderHook, ScriptedProvider};
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
    assert!(
        completed,
        "the turn must still complete (cleanly fail), not hang"
    );
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
    assert_eq!(
        complete_count, 1,
        "the failed turn ends with exactly one TurnComplete"
    );
    assert!(
        log.lock().unwrap().contains(&"on_error".to_string()),
        "the on_error hook must fire on a mid-stream error"
    );
    // A mid-stream failure must NOT fall through to a normal-success completion:
    // on_model_response (the success path) must NOT have run.
    assert!(
        !log.lock()
            .unwrap()
            .contains(&"on_model_response".to_string()),
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

// A Done { truncated: true } with no tool call (output cut off at the token
// limit) auto-continues: inject a synthetic "resume" nudge and make a follow-up
// LLM call rather than silently ending the turn (v1 parity —
// atomcode-core/src/agent/mod.rs:3064). When that recovery happens, the scary
// "response truncated" warning is SUPPRESSED — the work is being finished, so a
// red alarm would be misleading. (The warning is reserved for the unrecoverable
// case; see `repeated_truncation_is_bounded`.)
#[tokio::test]
async fn recovered_truncation_continues_without_warning() {
    let reg = ToolRegistry::new();
    let provider = Arc::new(MockProvider::new(vec![
        // round 1: output is cut off at the token limit
        vec![
            StreamEvent::TextDelta("a long cut-off answer".into()),
            StreamEvent::Done { truncated: true },
        ],
        // round 2: the model wraps up cleanly after the resume nudge
        vec![
            StreamEvent::TextDelta("the rest, finished".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let received = provider.received.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .build()
        .spawn();
    handle
        .commands
        .send(send("translate the whole file"))
        .unwrap();

    let mut warning: Option<String> = None;
    let mut recovery_progress = Vec::new();
    let mut completed = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warning = Some(w),
            AgentEvent::OutputTruncationRecovery {
                attempt,
                max_attempts,
            } => recovery_progress.push((attempt, max_attempts)),
            AgentEvent::TurnComplete { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        warning.is_none(),
        "a truncation that auto-recovers must NOT surface the scary warning; got {warning:?}"
    );
    assert_eq!(recovery_progress, vec![(1, 4)]);
    assert!(completed, "the turn still completes after the continuation");
    handle.commands.send(AgentCommand::Snapshot).unwrap();
    let snapshot = loop {
        match handle.events.recv().await {
            Some(AgentEvent::Snapshot { snapshot }) => break snapshot.messages,
            Some(_) => continue,
            None => panic!("never received a Snapshot reply"),
        }
    };
    assert!(
        snapshot.iter().any(|m| {
            m.role == Role::User && m.synthetic && m.text.to_lowercase().contains("output limit")
        }),
        "the stored truncation resume nudge must be a synthetic user message: {snapshot:?}"
    );
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    let calls = received.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "truncation must drive a continuation round (a 2nd LLM call); got {} calls",
        calls.len()
    );
    // The follow-up call's LAST message is the synthetic resume nudge.
    let nudge = &calls[1].last().unwrap().1;
    let n = nudge.to_lowercase();
    assert!(
        n.contains("output limit") || n.contains("resume") || n.contains("cut off"),
        "the 2nd call must carry a resume nudge; got {nudge:?}"
    );
}

// The truncation auto-continuation MUST be bounded: a model that truncates on
// every round (never wrapping up) cannot livelock the turn. After the bound the
// turn terminates instead of re-prompting forever.
#[tokio::test]
async fn repeated_truncation_is_bounded() {
    let reg = ToolRegistry::new();
    // Every scripted round truncates; the queue is deep enough that the STOP must
    // come from the kernel's own bound, not from the provider running dry.
    let turns: Vec<Vec<StreamEvent>> = (0..20)
        .map(|_| {
            vec![
                StreamEvent::TextDelta("cut".into()),
                StreamEvent::Done { truncated: true },
            ]
        })
        .collect();
    let provider = Arc::new(MockProvider::new(turns));
    let received = provider.received.clone();

    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let mut completed = false;
    let mut warning: Option<String> = None;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warning = Some(w),
            AgentEvent::TurnComplete { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert!(
        completed,
        "the turn must terminate despite endless truncation"
    );
    let calls = received.lock().unwrap();
    // Exactly 1 initial call + MAX_TRUNCATION_CONTINUATIONS (4) resume rounds = 5.
    // Pinned with == (not <=) so the bound stays load-bearing if the constant moves.
    assert_eq!(
        calls.len(),
        5,
        "truncation continuations must be tightly bounded"
    );
    // UNRECOVERABLE truncation (budget exhausted, turn actually stops) MUST warn —
    // this is the one case the user needs to see, and the only case that should.
    assert!(
        warning
            .as_deref()
            .is_some_and(|w| w.contains("上限") && w.contains("继续")),
        "an exhausted, turn-ending truncation must surface the warning; got {warning:?}"
    );
}

#[tokio::test]
async fn interactive_truncation_checkpoint_can_continue_after_auto_budget() {
    let reg = ToolRegistry::new();
    // The auto-continuation budget is MAX_TRUNCATION_CONTINUATIONS (4): calls 1-4
    // auto-recover, call 5 truncates with the budget exhausted → the checkpoint
    // fires. After the driver says "continue", call 6 wraps up cleanly.
    let mut turns: Vec<Vec<StreamEvent>> = (1..=5)
        .map(|i| {
            vec![
                StreamEvent::TextDelta(format!("part {i}")),
                StreamEvent::Done { truncated: true },
            ]
        })
        .collect();
    turns.push(vec![
        StreamEvent::TextDelta("finished".into()),
        StreamEvent::Done { truncated: false },
    ]);
    let provider = Arc::new(MockProvider::new(turns));
    let received = provider.received.clone();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let mut checkpoint = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Request { id, kind, .. }
                if kind == atomcode_kernel::event::OUTPUT_TRUNCATION_CHECKPOINT_KIND =>
            {
                checkpoint = Some(id);
                handle
                    .commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "continue": true }),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert!(
        checkpoint.is_some(),
        "the exhausted budget must ask the driver"
    );
    // 5 truncated rounds (1 initial + 4 auto-continuations) + 1 clean finish.
    assert_eq!(received.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn interactive_truncation_checkpoint_stop_does_not_repeat_warning() {
    let reg = ToolRegistry::new();
    // 5 truncated rounds (1 initial + MAX_TRUNCATION_CONTINUATIONS=4 auto-recoveries)
    // exhaust the auto budget, so the 5th round reaches the interactive checkpoint.
    let turns = (0..5)
        .map(|_| {
            vec![
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::Done { truncated: true },
            ]
        })
        .collect();
    let provider = Arc::new(MockProvider::new(turns));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let mut warning = None;
    let mut checkpoint_fired = false;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Request { id, kind, .. }
                if kind == atomcode_kernel::event::OUTPUT_TRUNCATION_CHECKPOINT_KIND =>
            {
                checkpoint_fired = true;
                handle
                    .commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "continue": false }),
                    })
                    .unwrap();
            }
            AgentEvent::Warning(message) => warning = Some(message),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    // Pin the intent: the budget must actually be exhausted so the interactive
    // checkpoint fires (otherwise `warning.is_none()` passes vacuously).
    assert!(checkpoint_fired, "the exhausted budget must reach the checkpoint");
    assert!(warning.is_none(), "the choice panel already explained the stop");
}

#[tokio::test]
async fn interactive_truncation_checkpoint_cancel_uses_cancelled_terminal() {
    let reg = ToolRegistry::new();
    // 5 truncated rounds (1 initial + MAX_TRUNCATION_CONTINUATIONS=4 auto-recoveries)
    // exhaust the auto budget, so the 5th round reaches the interactive checkpoint.
    let turns = (0..5)
        .map(|_| {
            vec![
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::Done { truncated: true },
            ]
        })
        .collect();
    let provider = Arc::new(MockProvider::new(turns));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&[] as &[&str]))
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle.commands.send(send("go")).unwrap();

    let mut stop = None;
    let mut checkpoint_fired = false;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Request { kind, .. }
                if kind == atomcode_kernel::event::OUTPUT_TRUNCATION_CHECKPOINT_KIND =>
            {
                checkpoint_fired = true;
                handle.commands.send(AgentCommand::Cancel).unwrap();
            }
            AgentEvent::TurnComplete { reason } => {
                stop = Some(reason);
                break;
            }
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    // Pin the intent: the Cancel must be delivered AT the truncation checkpoint,
    // not from some unrelated request kind.
    assert!(checkpoint_fired, "the exhausted budget must reach the checkpoint");

    assert_eq!(stop, Some(atomcode_kernel::event::StopReason::Cancelled));
}

// --- local helpers (keep each test terse; mirror spike_claims.rs style) ---

use atomcode_kernel::agent::{Agent, AgentHandle};
use atomcode_kernel::provider::LlmProvider;

fn send(text: &str) -> AgentCommand {
    AgentCommand::SendMessage {
        text: text.into(),
        images: vec![],
    }
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
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::tool::ToolDef;
use futures::stream::BoxStream;
use std::sync::atomic::{AtomicU32, Ordering};

/// A provider whose open FAILS the first `fail_first` times (returning a clone of
/// `err`), then SUCCEEDS with a content-bearing response (`TextDelta`+`Done`).
/// Counts total opens so a test can assert how many re-opens the agent loop drove.
/// (The success must carry text: a bare `Done` is now treated as a transient
/// empty-200 and retried, which would mask the open-retry behaviour under test.)
struct FlakyProvider {
    fail_first: u32,
    err: ProviderError,
    calls: AtomicU32,
}

impl FlakyProvider {
    fn new(fail_first: u32, err: ProviderError) -> Self {
        Self {
            fail_first,
            err,
            calls: AtomicU32::new(0),
        }
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
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("recovered".into()),
            StreamEvent::Done { truncated: false },
        ])))
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
    assert!(
        !errored,
        "a recovered turn must NOT surface a terminal Error"
    );
    assert_eq!(
        stop.as_deref(),
        Some("Stopped"),
        "the turn must end successfully"
    );
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

// ── EMPTY-RESPONSE FAST RETRY (an empty 200 must be retried, not a silent stop) ─
//
// Some OpenAI-compatible gateways (the atomgit→DeepSeek path) occasionally return a
// 200 with a COMPLETELY empty completion: the stream opens, yields no text, no tool
// calls and no reasoning, then ends. That is a transient upstream hiccup that
// recovers on an immediate resend — NOT the model choosing to stop (a real stop
// carries text). The kernel must re-issue the SAME round with a VISIBLE notice and
// SUCCEED once the provider recovers, instead of finishing as a silent Stopped
// (which the user perceives as the agent mysteriously giving up mid-task).
// `start_paused` advances the backoff on virtual time so the test is instant.
#[tokio::test(start_paused = true)]
async fn empty_response_is_retried_then_succeeds() {
    let reg = ToolRegistry::new();
    // Call 1: an empty 200 (bare Done, no content). Call 2: a real recovery.
    let provider = Arc::new(MockProvider::new(vec![
        vec![StreamEvent::Done { truncated: false }],
        vec![
            StreamEvent::TextDelta("hello".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let received = provider.received.clone();
    let recorder = Arc::new(RecorderHook::new());

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut warnings: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut errored = false;
    let mut stop: Option<String> = None;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warnings.push(w),
            AgentEvent::TextDelta(t) => text.push_str(&t),
            AgentEvent::Error { .. } => errored = true,
            AgentEvent::TurnComplete { reason } => {
                stop = Some(format!("{reason:?}"));
                break;
            }
            _ => {}
        }
    }

    let calls = received.lock().unwrap().len();
    assert_eq!(
        calls, 2,
        "the empty 200 must be RETRIED → a second chat_stream call; got {calls}"
    );
    assert_eq!(
        text, "hello",
        "the recovered response text must reach the driver"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("空响应") && w.contains("重试")),
        "an empty response must emit a VISIBLE retry notice; got {warnings:?}"
    );
    assert!(
        !errored,
        "a recovered empty response must NOT surface a terminal Error"
    );
    assert_eq!(
        stop.as_deref(),
        Some("Stopped"),
        "the recovered turn must end successfully (Stopped)"
    );
}

// ── EMPTY-RESPONSE EXHAUSTION (a run of empty 200s fails VISIBLY, not silently) ──
//
// If every resend is also empty, the dedicated budget is exhausted and the turn
// ends with StopReason::ProviderError and a clear Error — NOT a silent Stopped that
// looks like a normal (empty) completion.
#[tokio::test(start_paused = true)]
async fn empty_response_exhaustion_fails_visibly() {
    let reg = ToolRegistry::new();
    // An empty turns queue makes MockProvider yield a bare Done forever → every open
    // is an empty 200, so the retry budget is exhausted.
    let provider = Arc::new(MockProvider::new(vec![]));
    let received = provider.received.clone();
    let recorder = Arc::new(RecorderHook::new());

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut retry_notices = 0usize;
    let mut error_msg: Option<String> = None;
    let mut stop: Option<String> = None;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Warning(w) if w.contains("空响应") => retry_notices += 1,
            AgentEvent::Error { message, .. } => error_msg = Some(message),
            AgentEvent::TurnComplete { reason } => {
                stop = Some(format!("{reason:?}"));
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        retry_notices, 5,
        "the dedicated budget is 5 visible retries"
    );
    let calls = received.lock().unwrap().len();
    assert_eq!(
        calls, 6,
        "1 initial empty open + 5 retries = 6 chat_stream calls; got {calls}"
    );
    assert!(
        error_msg.as_deref().is_some_and(|m| m.contains("空响应")),
        "exhaustion must surface a clear empty-response Error; got {error_msg:?}"
    );
    assert_eq!(
        stop.as_deref(),
        Some("ProviderError"),
        "exhausted empty retries must fail the turn with ProviderError, not a silent Stopped"
    );
}

// ── MALFORMED response is retried with a DISTINCT notice (格式异常, not 空响应) ──
//
// When the adapter dropped an unparseable chunk (StreamEvent::Malformed) and the
// round carries no content, the kernel still retries (same as an empty 200) but the
// notice says the response was MALFORMED ("响应格式异常") rather than empty — the two
// are different upstream faults and the wording should not conflate them.
#[tokio::test(start_paused = true)]
async fn malformed_response_retried_with_distinct_notice() {
    let reg = ToolRegistry::new();
    // Call 1: a malformed-only round (adapter dropped a garbage chunk, no content).
    // Call 2: a clean recovery.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::Malformed,
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let received = provider.received.clone();
    let recorder = Arc::new(RecorderHook::new());

    let handle = spawn_agent(provider, reg, recorder);
    handle.commands.send(send("go")).unwrap();

    let mut events = handle.events;
    let mut warnings: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut stop: Option<String> = None;
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warnings.push(w),
            AgentEvent::TextDelta(t) => text.push_str(&t),
            AgentEvent::TurnComplete { reason } => {
                stop = Some(format!("{reason:?}"));
                break;
            }
            _ => {}
        }
    }

    let calls = received.lock().unwrap().len();
    assert_eq!(
        calls, 2,
        "a malformed (content-free) response must be RETRIED; got {calls}"
    );
    assert_eq!(text, "ok", "the recovered response must reach the driver");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("格式异常") && w.contains("重试")),
        "a malformed response must emit a格式异常 retry notice; got {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w.contains("空响应")),
        "a malformed response must NOT use the empty-response (空响应) wording; got {warnings:?}"
    );
    assert_eq!(
        stop.as_deref(),
        Some("Stopped"),
        "the recovered turn must succeed"
    );
}
