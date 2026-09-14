//! The two engines, side by side.
//!
//! The question this exists to answer is not "does it compile" but "does it
//! *mean* the same". `atomcode-coding`'s 15,787-line runtime talks to its engine
//! through exactly one channel pair — 8 `AgentCommand`s in, 25 `AgentEvent`s out
//! — so putting the harness underneath it is a small interface and a large
//! behavioural risk. Interface size is measurable by reading; behaviour is not.
//!
//! So: drive both engines with the same scripted model and the same commands,
//! normalise the two event streams, and diff them. The old engine is a free
//! oracle — the one kind of judge that is genuinely independent of the new
//! implementation, because it was written by someone who had never heard of it.
//!
//! # This is a measurement first and a gate second
//!
//! The divergence count is a **ratchet**: whatever it is today is frozen, and it
//! may only go down. A gate that demanded parity on day one would be red on day
//! one, and a gate that is red on day one gets deleted. What matters is that the
//! number cannot silently grow.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::ToolDef;
use futures::stream::{self, BoxStream};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

// ---- one model, so a divergence can only be the engine ------------------

/// One scripted round.
///
/// Rich enough to reach the paths that actually carry ordering guarantees: a
/// tool call, several at once, a usage report, a mid-stream failure.
#[derive(Clone)]
enum Reply {
    Text(&'static str),
    /// Text plus tool calls in the same round — what a real model does.
    Calls(
        &'static str,
        Vec<(&'static str, &'static str, &'static str)>,
    ),
    /// A mid-stream provider failure.
    Fail(&'static str),
    /// A failure at OPEN the provider marks retryable — a relay's momentary
    /// "no upstream available" 503 — with the server's `Retry-After`, if any.
    OpenFail(&'static str, Option<u64>),
    /// Cut off by `finish_reason=length`.
    Truncated(&'static str),
    /// A round that takes time to answer.
    ///
    /// Needed to measure cancellation at all: with an instant provider, whether
    /// a `Cancel` lands before the round finishes is a race, and a race reports
    /// as an engine difference on one run and not the next. Blocking makes the
    /// cancel deterministically mid-turn, so what is compared is the engines'
    /// behaviour rather than their scheduling luck.
    Slow(u64, &'static str),
}

impl Reply {
    fn call(id: &'static str, name: &'static str, args: &'static str) -> Reply {
        Reply::Calls("", vec![(id, name, args)])
    }
}

/// A provider that replays a fixed script.
///
/// Shared by both engines *by value*, not re-implemented on each side: two
/// scripted providers that drifted would show up as an engine difference, and
/// the whole point is that they cannot.
///
/// Every round reports the same token usage, so "who forwards usage" is a real
/// question about the engines rather than an artefact of one of them inventing
/// numbers.
struct Script {
    replies: Vec<Reply>,
    cursor: AtomicUsize,
    /// What the model claims to be.
    ///
    /// Real behaviour branches on it — `model_needs_firm_execution` is the
    /// reason this field exists — so a fixture that can only ever be called
    /// "script" cannot exercise anything model-gated on either engine.
    model: String,
    /// Every request, as the model received it.
    ///
    /// The event stream and the snapshot both miss an EPHEMERAL request tail:
    /// it is appended to the request and never logged, so no event carries it
    /// and no snapshot shows it. Several coding behaviours are exactly that
    /// shape (the skill-first nudge, the plan-mode reminder), which made them
    /// invisible to this rig until it started keeping the requests.
    seen: std::sync::Mutex<Vec<Vec<Message>>>,
}

impl Script {
    fn new(replies: &[Reply]) -> Arc<Script> {
        Arc::new(Script {
            replies: replies.to_vec(),
            cursor: AtomicUsize::new(0),
            model: "script".into(),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn text(replies: &[&'static str]) -> Arc<Script> {
        Script::new(&replies.iter().map(|t| Reply::Text(t)).collect::<Vec<_>>())
    }
    /// The same script, claiming to be a different model.
    fn as_model(self: Arc<Script>, model: &str) -> Arc<Script> {
        Arc::new(Script {
            replies: self.replies.clone(),
            cursor: AtomicUsize::new(0),
            model: model.into(),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }
    /// Everything the model was shown, request by request, flattened to text.
    fn seen(&self) -> String {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flatten()
            .map(|m| format!("{:?}: {}", m.role, m.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl LlmProvider for Script {
    fn model_name(&self) -> &str {
        &self.model
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(messages.to_vec());
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        // Past the end of the script, stop cleanly rather than failing: a
        // fixture running out is not a provider outage, and one engine taking
        // an extra round must not read as a provider difference.
        let reply = self.replies.get(i).cloned().unwrap_or(Reply::Text("done"));
        let mut events: Vec<StreamEvent> = Vec::new();
        match reply {
            Reply::OpenFail(message, retry_after_secs) => {
                return Err(ProviderError {
                    retryable: true,
                    message: message.into(),
                    http_status: Some(503),
                    code: None,
                    retry_after_secs,
                });
            }
            Reply::Slow(ms, t) => {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                events.push(StreamEvent::TextDelta(t.into()));
            }
            Reply::Truncated(t) => {
                events.push(StreamEvent::TextDelta(t.into()));
                events.push(StreamEvent::Usage(atomcode_kernel::stream::TokenUsage {
                    prompt: 100,
                    completion: 20,
                    cached: 0,
                }));
                events.push(StreamEvent::Done { truncated: true });
                return Ok(Box::pin(stream::iter(events)));
            }
            Reply::Text(t) => events.push(StreamEvent::TextDelta(t.into())),
            Reply::Calls(t, calls) => {
                if !t.is_empty() {
                    events.push(StreamEvent::TextDelta(t.into()));
                }
                for (id, name, args) in calls {
                    events.push(StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments: args.into(),
                    }));
                }
            }
            Reply::Fail(message) => {
                events.push(StreamEvent::Error(ProviderError {
                    retryable: false,
                    message: message.into(),
                    http_status: Some(500),
                    code: None,
                    retry_after_secs: None,
                }));
                return Ok(Box::pin(stream::iter(events)));
            }
        }
        events.push(StreamEvent::Usage(atomcode_kernel::stream::TokenUsage {
            prompt: 100,
            completion: 20,
            cached: 0,
        }));
        events.push(StreamEvent::Done { truncated: false });
        Ok(Box::pin(stream::iter(events)))
    }
}

// ---- what "the same" means ----------------------------------------------

/// One event, with everything volatile removed.
///
/// Ids, durations and token counts legitimately differ between two engines
/// running the same conversation; the *shape* of the stream must not. Comparing
/// raw events would report a difference on every run and mean nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Step {
    kind: &'static str,
    detail: String,
}

fn normalise(event: &AgentEvent) -> Option<Step> {
    let step = |kind, detail: String| Some(Step { kind, detail });
    match event {
        AgentEvent::TurnStarted => step("TurnStarted", String::new()),
        // Deltas are chunking, not meaning: one engine may split a sentence
        // where the other does not. The assembled text arrives in TurnComplete.
        AgentEvent::TextDelta(_) => None,
        AgentEvent::Reasoning(_) => None,
        AgentEvent::ToolCallStreaming { .. } => None,
        AgentEvent::ToolBatchStarted { .. } => step("ToolBatchStarted", String::new()),
        AgentEvent::ToolBatchCompleted { .. } => step("ToolBatchCompleted", String::new()),
        AgentEvent::ToolStarted { call } => step("ToolStarted", call.name.clone()),
        AgentEvent::ToolProgress { .. } => None,
        AgentEvent::ToolResult { result } => {
            step("ToolResult", format!("error={}", result.is_error))
        }
        AgentEvent::PolicyIntervention { .. } => step("PolicyIntervention", String::new()),
        AgentEvent::Request { kind, .. } => step("Request", kind.clone()),
        AgentEvent::Usage(_) => step("Usage", String::new()),
        AgentEvent::Snapshot { snapshot } => {
            // Roles, not just a count: "4 vs 2" says there is a difference,
            // "system,user,assistant,… vs user,assistant" says what it is.
            // Consecutive system messages collapse to one. Not a fudge: the
            // stack already coalesces them on the wire because some providers
            // honour only the first, so "one system message" and "three in a
            // row" are the same conversation. What must not differ is whether
            // there is a system message at all.
            let mut roles: Vec<String> = Vec::new();
            for m in &snapshot.messages {
                let role = format!("{:?}", m.role).to_lowercase();
                if role == "system" && roles.last().map(String::as_str) == Some("system") {
                    continue;
                }
                roles.push(role);
            }
            step("Snapshot", roles.join(","))
        }
        AgentEvent::TurnComplete { reason } => step("TurnComplete", format!("{reason:?}")),
        AgentEvent::Error { message, .. } => step("Error", message.clone()),
        AgentEvent::Cancelled => step("Cancelled", String::new()),
        AgentEvent::Warning(_) => None,
        AgentEvent::StreamRecovery { .. } => None,
        AgentEvent::ProviderRetry { .. } => None,
        // Reported, not ignored: a divergence whose only trace is an extra
        // `Usage` is a mystery, and the rig exists to produce findings rather
        // than puzzles.
        AgentEvent::OutputTruncationRecovery { .. } => step("TruncationRecovery", String::new()),
        AgentEvent::RateLimited { .. } => None,
        AgentEvent::Steered { .. } => step("Steered", String::new()),
        AgentEvent::CompactionStarted { .. } => step("CompactionStarted", String::new()),
        AgentEvent::Compacted { .. } => step("Compacted", String::new()),
        AgentEvent::CompactionFailed { .. } => step("CompactionFailed", String::new()),
        // `AgentEvent` is `#[non_exhaustive]`, so this arm is unavoidable. It
        // deliberately *reports* rather than ignores: a variant added upstream
        // shows up as a divergence with its own name, instead of being dropped
        // on the floor by whichever engine does not emit it.
        other => step("UNKNOWN", format!("{other:?}")),
    }
}

/// As [`drive`], but keep listening past the turn's end until every `also` kind
/// has been seen.
///
/// The first version always stopped at the terminal event, which meant a reply
/// that legitimately arrives after it — a `Snapshot` answer — was cut off by the
/// rig rather than missing from the engine. A measurement that reports its own
/// harness's behaviour is worse than no measurement.
async fn drive_until(handle: AgentHandle, commands: Vec<AgentCommand>, also: &[&str]) -> Vec<Step> {
    drive_with(handle, commands, also, None).await
}

/// As above, but send `late` once the turn has actually started.
///
/// Cancellation only means anything mid-turn. Sending it in the same breath as
/// the message measures which engine drains its command queue faster, which is
/// not a difference anybody cares about.
/// Send one message, wait for its turn to end, then the next.
///
/// A second turn is not a second message on the same turn — the distinction the
/// steering scenario is about — so the rig has to wait, or it would be
/// measuring steering again under a different name.
async fn drive_turns(mut handle: AgentHandle, messages: &[&str]) -> Vec<Step> {
    let mut out = Vec::new();
    for text in messages {
        if handle
            .commands
            .send(AgentCommand::SendMessage {
                text: (*text).into(),
                images: Vec::new(),
            })
            .is_err()
        {
            break;
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                out.push(Step {
                    kind: "TIMEOUT",
                    detail: "a turn never ended".into(),
                });
                return out;
            }
            match tokio::time::timeout(left, handle.events.recv()).await {
                Ok(Some(event)) => {
                    let terminal = matches!(
                        event,
                        AgentEvent::TurnComplete { .. } | AgentEvent::Error { .. }
                    );
                    if let Some(step) = normalise(&event) {
                        out.push(step);
                    }
                    if terminal {
                        break;
                    }
                }
                _ => return out,
            }
        }
    }
    let _ = handle.commands.send(AgentCommand::Shutdown);
    out
}

/// Everything the model was shown, as text, taken from a snapshot.
///
/// For the questions an event stream cannot answer — "did the context actually
/// reach the model" is about message contents, and every event in the world
/// could look right while the answer is no.
async fn transcript(mut handle: AgentHandle, commands: Vec<AgentCommand>) -> String {
    for c in commands {
        if handle.commands.send(c).is_err() {
            break;
        }
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut asked = false;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return String::new();
        }
        match tokio::time::timeout(left, handle.events.recv()).await {
            Ok(Some(AgentEvent::TurnComplete { .. })) | Ok(Some(AgentEvent::Error { .. }))
                if !asked =>
            {
                asked = true;
                let _ = handle.commands.send(AgentCommand::Snapshot);
            }
            Ok(Some(AgentEvent::Snapshot { snapshot })) => {
                let _ = handle.commands.send(AgentCommand::Shutdown);
                return snapshot
                    .messages
                    .iter()
                    .map(|m| format!("{:?}: {}", m.role, m.text))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            Ok(Some(_)) => {}
            _ => return String::new(),
        }
    }
}

/// The wire shape a driver answers an approval with.
///
/// `{"decision":"allow"}` — not `"yes"`. Anything unrecognised parses as deny,
/// on purpose (a crashed driver must not become consent), which is why the
/// first version of these scenarios saw every call refused.
fn allow() -> serde_json::Value {
    serde_json::json!({ "decision": "allow" })
}

fn deny() -> serde_json::Value {
    serde_json::json!({ "decision": "deny" })
}

async fn drive_with(
    handle: AgentHandle,
    commands: Vec<AgentCommand>,
    also: &[&str],
    late: Option<AgentCommand>,
) -> Vec<Step> {
    drive_answering(handle, commands, also, late, allow()).await
}

async fn drive_answering(
    mut handle: AgentHandle,
    commands: Vec<AgentCommand>,
    also: &[&str],
    late: Option<AgentCommand>,
    answer: serde_json::Value,
) -> Vec<Step> {
    for c in commands {
        if handle.commands.send(c).is_err() {
            break;
        }
    }
    /// How long to keep listening after the turn ends.
    const GRACE: std::time::Duration = std::time::Duration::from_millis(250);
    let mut out = Vec::new();
    let mut done = false;
    let mut grace: Option<tokio::time::Instant> = None;
    // A bound, not a hope: an engine that never terminates must fail the test
    // rather than hang it. A hang reports as slowness and gets blamed on the
    // machine.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let until = grace.map_or(deadline, |g| g.min(deadline));
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            if grace.is_some() {
                break; // the trailing window closed; not a failure
            }
            out.push(Step {
                kind: "TIMEOUT",
                detail: "the engine never finished the turn".into(),
            });
            break;
        }
        match tokio::time::timeout(left, handle.events.recv()).await {
            Ok(Some(event)) => {
                if matches!(
                    event,
                    AgentEvent::TurnComplete { .. } | AgentEvent::Error { .. }
                ) {
                    done = true;
                }
                // Answer any request the engine raises. A driver that did not
                // would park the turn, and every approval scenario would
                // measure the rig's silence rather than the engines.
                // `Null` means "this driver does not answer" — for the
                // deadlock scenario, where the way out has to be the cancel.
                if let AgentEvent::Request { id, .. } = &event {
                    if !answer.is_null() {
                        let _ = handle.commands.send(AgentCommand::Respond {
                            id: *id,
                            value: answer.clone(),
                        });
                    }
                }
                if matches!(event, AgentEvent::TurnStarted) {
                    if let Some(cmd) = late.clone() {
                        // Give the round a moment to be genuinely in flight.
                        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                        let _ = handle.commands.send(cmd);
                    }
                }
                if let Some(step) = normalise(&event) {
                    out.push(step);
                }
                // Checked on *every* event, not only terminal ones: a reply
                // that arrives after the turn ended would otherwise never be
                // noticed, and the rig would sit out its whole timeout.
                if done && also.iter().all(|k| out.iter().any(|s| s.kind == *k)) {
                    // A short window for anything that legitimately trails the
                    // turn — a queued compaction, a snapshot answered on the
                    // way out. Breaking at the terminal event made the rig
                    // report "neither engine emits this" when the truth was
                    // "the rig stopped listening". Both sides get the same
                    // window, so what is compared is still the engines.
                    if grace.is_none() {
                        grace = Some(tokio::time::Instant::now() + GRACE);
                    }
                }
                if let Some(until) = grace {
                    if tokio::time::Instant::now() >= until {
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => {
                if grace.is_none() {
                    out.push(Step {
                        kind: "TIMEOUT",
                        detail: "the engine never finished the turn".into(),
                    });
                }
                break;
            }
        }
    }
    let _ = handle.commands.send(AgentCommand::Shutdown);
    out
}

// ---- the two engines -----------------------------------------------------

/// A scratch dir that is NOT under the system temp roots.
///
/// `scratch` below uses `std::env::temp_dir()`, and `write_approval`'s
/// `path_in_temp_dir` deliberately treats anything under a temp root as benign —
/// a throwaway write, not project code. So every "write outside the workspace"
/// scenario built on `scratch` was writing INTO the temp dir and being
/// auto-approved by design: zero approval requests, zero divergence, and nothing
/// measured. Under `target/` the question is a real one again.
fn scratch_outside_temp(tag: &str) -> std::path::PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/diff-scratch")
        .join(format!("{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch outside temp");
    dir
}

fn scratch(tag: &str) -> std::path::PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("diff-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The reference: `atomcode-coding`'s own assembly.
async fn reference(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    reference_until(script, dir, commands, &[]).await
}

async fn reference_cancelling(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    let cfg = atomcode_coding::CodingAgentConfig::new("k", "http://unused.test/v1", "script", dir);
    let agent = atomcode_coding::build_coding_agent_with(&cfg, script);
    drive_with(agent.spawn(), commands, &[], Some(AgentCommand::Cancel)).await
}

fn coding_agent(script: Arc<Script>, dir: &std::path::Path) -> atomcode_kernel::agent::Agent {
    let cfg = atomcode_coding::CodingAgentConfig::new("k", "http://unused.test/v1", "script", dir);
    atomcode_coding::build_coding_agent_with(&cfg, script)
}

/// A second message, sent once the turn is genuinely under way.
async fn reference_late(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    late: AgentCommand,
) -> Vec<Step> {
    drive_with(coding_agent(script, dir).spawn(), commands, &[], Some(late)).await
}

async fn reference_until(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
) -> Vec<Step> {
    let cfg = atomcode_coding::CodingAgentConfig::new("k", "http://unused.test/v1", "script", dir);
    let agent = atomcode_coding::build_coding_agent_with(&cfg, script);
    drive_until(agent.spawn(), commands, also).await
}

// ---- the production reference -------------------------------------------
//
// `build_coding_agent_with` above is, by its own doc comment, the "MINIMAL sync
// path (tools + codeintel only)". Production does not use it: it runs the
// two-phase `prepare` → `assemble`, which mounts roughly fourteen
// `ToolMiddleware`s (the approval gates, datalog, artifact spill, …) that the
// minimal path never sees.
//
// So the parity this file measured until now was parity against a stripped-down
// engine. That is the wrong oracle for a migration whose whole difficulty IS
// that middleware chain — it would report "green" for a swap that drops every
// gate. These run the same scripts through the real assembly, under their own
// ratchet keys so the minimal-path numbers stay readable beside them.
//
// `mcp` / `web` / `review` / `memory` are off: each reaches the network, a
// subprocess or the user's disk, and a rig that needs any of those is not a rig.
// Everything they gate is additive to the chain, so the chain itself is intact.

async fn production_agent(
    script: Arc<Script>,
    dir: &std::path::Path,
) -> atomcode_kernel::agent::Agent {
    let cfg = atomcode_coding::CodingAgentConfig::new("k", "http://unused.test/v1", "script", dir);
    let opts = atomcode_coding::parts::PrepareOptions {
        mcp: false,
        web: false,
        review: false,
        memory: false,
        skill_dirs: Some(Vec::new()),
        ..Default::default()
    };
    let mut parts = atomcode_coding::parts::prepare(&cfg, opts)
        .await
        .expect("the production prepare must succeed in the rig");
    atomcode_coding::parts::assemble(&mut parts, &cfg, script)
        .expect("the production assemble must succeed in the rig")
}

async fn reference_production(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    drive_until(production_agent(script, dir).await.spawn(), commands, &[]).await
}

/// The candidate: a plexus tree providing `agent-handle`.
async fn candidate(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    candidate_until(script, dir, commands, &[]).await
}

async fn candidate_cancelling(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    candidate_inner(script, dir, commands, &[], Some(AgentCommand::Cancel)).await
}

async fn candidate_late(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    late: AgentCommand,
) -> Vec<Step> {
    candidate_answering(
        script,
        dir,
        commands,
        &[],
        Some(late),
        serde_json::json!("yes"),
    )
    .await
}

/// Raise the approval, never answer it, cancel instead.
async fn candidate_never_answering(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    candidate_answering(
        script,
        dir,
        commands,
        &[],
        Some(AgentCommand::Cancel),
        serde_json::Value::Null,
    )
    .await
}

async fn candidate_until(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
) -> Vec<Step> {
    candidate_inner(script, dir, commands, also, None).await
}

async fn candidate_inner(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
    late: Option<AgentCommand>,
) -> Vec<Step> {
    candidate_answering(script, dir, commands, also, late, allow()).await
}

/// A mounted candidate tree and its handle.
///
/// Both, because the tree must outlive the handle: dropping the `App` unloads
/// every row, and the next command goes to a conversation whose services have
/// all been torn down.
async fn candidate_handle(
    script: Arc<Script>,
    dir: &std::path::Path,
) -> (AgentHandle, atomcode_plexus::App) {
    let app = candidate_app(script, dir).await;
    let handle = app
        .context()
        .service::<atomcode_harness::seams::AgentHandleSvc>()
        .expect("agent-handle row must provide a handle")
        .take()
        .expect("the handle, once");
    (handle, app)
}

/// Mount a candidate tree with this script as its model.
async fn candidate_app(script: Arc<Script>, dir: &std::path::Path) -> atomcode_plexus::App {
    use atomcode_plexus::{App, ConfigTree, Layer};

    let empty = dir.join("__no_skills__");
    let _ = std::fs::create_dir_all(&empty);
    let scoped = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"approval\"\ndisabled = true\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {dir:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {dir:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {dir:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {dir:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-script\"\nconfig = {{}}\n\n\
         [[insert]]\nname = \"ui-handle\"\n",
        dir = dir.to_string_lossy(),
        home = empty.to_string_lossy(),
    );
    let tree = ConfigTree::from_layers(vec![
        atomcode_harness::bundle::base().unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ])
    .expect("tree");

    let mut registry = atomcode_harness::plugins::catalog();
    registry.register(Arc::new(InjectScript(script)));
    let mut app = App::new(registry, tree);
    app.start().await.expect("the candidate tree must mount");
    app
}

async fn candidate_answering(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
    late: Option<AgentCommand>,
    answer: serde_json::Value,
) -> Vec<Step> {
    let mut app = candidate_app(script, dir).await;
    let handle = app
        .context()
        .service::<atomcode_harness::seams::AgentHandleSvc>()
        .expect("agent-handle row must provide a handle")
        .take()
        .expect("the handle, once");
    let steps = drive_answering(handle, commands, also, late, answer).await;
    app.stop();
    steps
}

/// Puts the very same provider object into the tree.
struct InjectScript(Arc<Script>);

#[async_trait]
impl atomcode_plexus::Plugin for InjectScript {
    fn name(&self) -> &'static str {
        "llm-script"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "the differential rig's shared scripted model"
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _config: &serde_json::Value,
    ) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- the diff ------------------------------------------------------------

/// Line up the two streams, allowing insertions on either side.
///
/// Position-by-position comparison was wrong in a way that mattered: one extra
/// event near the start shifted everything after it, so a single defect
/// reported as four. The ratchet reads this number, so an inflated one both
/// exaggerates the problem and hides the next real regression underneath it.
///
/// Plain LCS — the streams are tens of events, not thousands.
fn align(a: &[Step], b: &[Step]) -> Vec<(Option<Step>, Option<Step>)> {
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((Some(a[i].clone()), Some(b[j].clone())));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push((Some(a[i].clone()), None));
            i += 1;
        } else {
            out.push((None, Some(b[j].clone())));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|s| (Some(s.clone()), None)));
    out.extend(b[j..].iter().map(|s| (None, Some(s.clone()))));
    out
}

fn render(a: &[Step], b: &[Step]) -> String {
    let mut out = String::from("\n  参考（coding）              候选（harness）\n");
    for (left, right) in align(a, b) {
        let same = left.is_some() && right.is_some();
        let show = |s: Option<Step>| {
            s.map(|s| format!("{} {}", s.kind, s.detail))
                .unwrap_or_else(|| "—".into())
        };
        out.push_str(&format!(
            "  {} {:<28} {}\n",
            if same { ' ' } else { '!' },
            show(left),
            show(right)
        ));
    }
    out
}

/// How many events are unmatched — one insertion counts once.
fn divergences(a: &[Step], b: &[Step]) -> usize {
    align(a, b)
        .into_iter()
        .filter(|(l, r)| l.is_none() || r.is_none())
        .count()
}

/// Freeze today's divergence and let it only shrink.
///
/// Demanding parity now would be red now, and a gate that is red on day one is
/// a gate that gets deleted. What must not happen is the number growing without
/// anyone noticing.
fn ratchet(name: &str, count: usize, report: &str) {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../gates/differential.baseline");
    let _ = std::fs::create_dir_all(path.parent().unwrap());

    // A read-modify-write on a file shared by every scenario, so it needs a lock —
    // and the lock has to be a CROSS-PROCESS one. This used to be a `static Mutex`,
    // which was enough only because `cargo test` runs a binary's tests as threads in
    // one process. `cargo nextest` gives each test its own process, so a process-local
    // lock guards nothing: two scenarios both read the old text, both append their own
    // line, and the second write loses the first. Silently — a missing entry is
    // indistinguishable from a first run, which is exactly the failure this guards.
    //
    // Locking the baseline file itself (rather than a sidecar) keeps the lock and the
    // data inseparable: there is no path where one exists without the other.
    let mut f = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("⊙ {name}: 打不开基线文件({e}),跳过棘轮{report}");
            return;
        }
    };
    if let Err(e) = f.lock_exclusive() {
        eprintln!("⊙ {name}: 锁不上基线文件({e}),跳过棘轮{report}");
        return;
    }
    let mut text = String::new();
    let _ = f.read_to_string(&mut text);

    let base: Option<usize> = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}="))?.trim().parse().ok());

    let rewrite = |f: &mut std::fs::File, body: &str| {
        let _ = f.set_len(0);
        let _ = f.seek(SeekFrom::Start(0));
        let _ = f.write_all(body.as_bytes());
    };

    match base {
        None => {
            let mut all = text;
            all.push_str(&format!("{name}={count}\n"));
            rewrite(&mut f, &all);
            eprintln!("⊙ {name}: 建立基线 {count} 处分歧{report}");
        }
        Some(base) if count > base => {
            // Drop the lock before unwinding so a panicking scenario cannot wedge
            // the others behind a lock held by a dead process's file handle.
            let _ = FileExt::unlock(&f);
            panic!("{name}: 分歧从 {base} 涨到 {count}{report}");
        }
        Some(base) if count < base => {
            let kept: String = text
                .lines()
                .filter(|l| !l.starts_with(&format!("{name}=")))
                .map(|l| format!("{l}\n"))
                .collect();
            rewrite(&mut f, &format!("{kept}{name}={count}\n"));
            eprintln!("✓ {name}: {count} 处分歧（基线从 {base} 降到 {count}）{report}");
        }
        Some(_) => eprintln!("✓ {name}: {count} 处分歧（持平）"),
    }
    let _ = FileExt::unlock(&f);
}

// ---- the measurements ----------------------------------------------------

#[tokio::test]
async fn a_plain_turn() {
    let dir = scratch("plain");
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "say hello".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(Script::text(&["hello"]), &dir, cmds()).await;
    let b = candidate(Script::text(&["hello"]), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("plain_turn", divergences(&a, &b), &report);

    // Whatever else differs, both must end the turn exactly once. A driver that
    // sees two terminals runs two turns; one that sees none waits forever.
    for (who, steps) in [("参考", &a), ("候选", &b)] {
        let terminals = steps
            .iter()
            .filter(|s| matches!(s.kind, "TurnComplete" | "Error" | "Cancelled"))
            .count();
        assert_eq!(terminals, 1, "{who} 的终结事件不是一个{report}");
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who} 没有结束{report}"
        );
    }
}

/// Write a file the scripted tools can act on, so both engines see the same
/// world as well as the same model.
fn seed(dir: &std::path::Path) {
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").expect("seed");
    std::fs::write(dir.join("b.rs"), "fn other() {}\n").expect("seed");
}

#[tokio::test]
async fn one_tool_call() {
    // The first place ordering matters: ToolStarted must precede its
    // ToolResult, and the turn must not end between them.
    let dir = scratch("one-tool");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
            Reply::Text("that is an empty main"),
        ])
    };
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "read a.rs".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("one_tool_call", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        let started = steps.iter().position(|s| s.kind == "ToolStarted");
        let result = steps.iter().position(|s| s.kind == "ToolResult");
        assert!(
            matches!((started, result), (Some(x), Some(y)) if x < y),
            "{who}: 工具必须先开始后有结果{report}"
        );
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnComplete").count(),
            1,
            "{who}: 工具轮之后仍然只有一个终结{report}"
        );
    }
}

#[tokio::test]
async fn two_tool_calls_at_once() {
    // Parallel calls are where a batch protocol earns its keep: whatever else
    // differs, every call that started must have a result, or a driver waits
    // forever on one that never lands.
    let dir = scratch("two-tools");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::Calls(
                "reading both",
                vec![
                    ("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                    ("c2", "read_file", r#"{"file_path":"b.rs"}"#),
                ],
            ),
            Reply::Text("both read"),
        ])
    };
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "read both".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("two_tool_calls", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        let started = steps.iter().filter(|s| s.kind == "ToolStarted").count();
        let results = steps.iter().filter(|s| s.kind == "ToolResult").count();
        assert_eq!(
            started, results,
            "{who}: 每个开始的调用都必须有结果，否则驱动方会永远等那一个{report}"
        );
        assert_eq!(started, 2, "{who}: 两个调用{report}");
    }
}

#[tokio::test]
async fn a_tool_that_does_not_exist() {
    // A model asking for a tool nobody mounted is normal, and it must come back
    // as a failed result rather than as a dead turn.
    let dir = scratch("no-such-tool");
    let script = || {
        Script::new(&[
            Reply::call("c1", "no_such_tool", "{}"),
            Reply::Text("ah, my mistake"),
        ])
    };
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "use it".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("unknown_tool", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 未知工具不能挂死回合{report}"
        );
    }
}

#[tokio::test]
async fn the_provider_fails_mid_stream() {
    let dir = scratch("provider-fails");
    let script = || Script::new(&[Reply::Fail("upstream exploded")]);
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "go".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("provider_error", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        let terminals = steps
            .iter()
            .filter(|s| matches!(s.kind, "TurnComplete" | "Error"))
            .count();
        assert!(terminals >= 1, "{who}: 失败也要终结回合{report}");
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 失败不能表现成挂死{report}"
        );
    }
}

#[tokio::test]
async fn several_rounds() {
    // Continuation: tool, tool, then an answer. Each round must open and close
    // once — an engine that emits TurnStarted per round rather than per turn
    // makes a driver draw three turns.
    let dir = scratch("rounds");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
            Reply::call("c2", "read_file", r#"{"file_path":"b.rs"}"#),
            Reply::Text("read them both"),
        ])
    };
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "read both, one at a time".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("several_rounds", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            1,
            "{who}: 一个回合开一次，不是每轮开一次{report}"
        );
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnComplete").count(),
            1,
            "{who}: 也只终结一次{report}"
        );
    }
}

#[tokio::test]
async fn a_cancel_lands() {
    // Cancellation is the one command whose whole value is timing. What must
    // hold either way: the turn ends, once, and does not hang.
    let dir = scratch("cancel");
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "go".into(),
            images: Vec::new(),
        }]
    };
    // 400ms round, cancelled 60ms in. An engine that honours the cancel
    // promptly finishes well before the round would have; one that waits for
    // the in-flight request finishes after it. That is a difference a person
    // feels, and it does not show up in the event stream at all — the same
    // events arrive, just late.
    let script = || Script::new(&[Reply::Slow(400, "working")]);
    let started = std::time::Instant::now();
    let a = reference_cancelling(script(), &dir, cmds()).await;
    let took_reference = started.elapsed();
    let started = std::time::Instant::now();
    let b = candidate_cancelling(script(), &dir, cmds()).await;
    let took_candidate = started.elapsed();
    let report = render(&a, &b);
    ratchet("cancel", divergences(&a, &b), &report);

    for (who, took) in [("参考", took_reference), ("候选", took_candidate)] {
        assert!(
            took < std::time::Duration::from_millis(380),
            "{who}: 取消用了 {took:?} —— 那一轮本来就要 400ms，说明它在等回合跑完\
             而不是中断它{report}"
        );
    }

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 取消之后回合必须结束{report}"
        );
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s.kind, "TurnComplete" | "Error"))
                .count(),
            1,
            "{who}: 恰好一个终结{report}"
        );
    }
}

/// Approval is a property of the candidate, not a comparison.
///
/// At the `AgentHandle` seam the two sides gate differently *by configuration*:
/// `build_coding_agent_with` registers no approval gate unless permission rules
/// are supplied, so a differential run here measured the config and not the
/// engine — the reference wrote the file without asking at all. Comparing that
/// to a harness that does ask says nothing about whether the harness could sit
/// underneath it.
///
/// So these two judge the candidate on its own, and they judge it against the
/// **disk**: whether a file exists is not something an event stream can be
/// mistaken about.
#[tokio::test]
async fn an_approval_is_asked_once_and_a_yes_lets_the_call_through() {
    let dir = scratch("approval");
    let script = Script::new(&[
        Reply::call(
            "c1",
            "write_file",
            r#"{"file_path":"new.rs","content":"fn x(){}"}"#,
        ),
        Reply::Text("written"),
    ]);
    let cmds = vec![AgentCommand::SendMessage {
        text: "write it".into(),
        images: Vec::new(),
    }];
    let steps = candidate_answering(script, &dir, cmds, &[], None, allow()).await;
    let report = render(&steps, &steps);

    assert_eq!(
        steps.iter().filter(|s| s.kind == "Request").count(),
        1,
        "一次调用问一次，不是零次也不是两次{report}"
    );
    assert_eq!(
        steps.iter().filter(|s| s.kind == "TurnComplete").count(),
        1,
        "审批不会多开一个回合{report}"
    );
    assert!(
        !steps.iter().any(|s| s.kind == "TIMEOUT"),
        "被应答之后回合必须继续{report}"
    );
    assert!(
        dir.join("new.rs").exists(),
        "同意了就必须真的写进去 —— 这条判据看的是磁盘，不是事件{report}"
    );
}

#[tokio::test]
async fn a_no_blocks_the_call_and_the_turn_carries_on() {
    // Saying no is an answer, not a failure: the model is told, and the turn
    // continues so it can try something else.
    let dir = scratch("refused");
    let script = Script::new(&[
        Reply::call(
            "c1",
            "write_file",
            r#"{"file_path":"nope.rs","content":"x"}"#,
        ),
        Reply::Text("understood"),
    ]);
    let cmds = vec![AgentCommand::SendMessage {
        text: "write it".into(),
        images: Vec::new(),
    }];
    let steps = candidate_answering(script, &dir, cmds, &[], None, deny()).await;
    let report = render(&steps, &steps);

    assert!(
        !dir.join("nope.rs").exists(),
        "拒绝之后文件不该存在{report}"
    );
    assert!(
        steps
            .iter()
            .any(|s| s.kind == "ToolResult" && s.detail.contains("error=true")),
        "模型必须被告知它被拒了，否则它不知道要换个做法{report}"
    );
    assert_eq!(
        steps.iter().filter(|s| s.kind == "TurnComplete").count(),
        1,
        "拒绝也要让回合正常走完{report}"
    );
}

#[tokio::test]
async fn a_cancel_while_a_tool_waits_for_approval() {
    // The deadlock this protocol can produce: a tool parked on an unanswered
    // approval, and the only way out is a cancel that the parked task must
    // actually observe. Nobody answers here on purpose.
    //
    // A candidate property rather than a comparison, for the same reason as the
    // other approval scenarios: the reference mounts no gate at this seam.
    let dir = scratch("approval-cancel");
    let script = Script::new(&[
        Reply::call("c1", "write_file", r#"{"file_path":"x.rs","content":"x"}"#),
        Reply::Text("done"),
    ]);
    let cmds = vec![AgentCommand::SendMessage {
        text: "write it".into(),
        images: Vec::new(),
    }];
    let started = std::time::Instant::now();
    let steps = candidate_never_answering(script, &dir, cmds).await;
    let took = started.elapsed();
    let report = render(&steps, &steps);

    assert!(
        !steps.iter().any(|s| s.kind == "TIMEOUT"),
        "等审批的工具被取消后回合必须结束，否则就是死锁{report}"
    );
    assert!(
        took < std::time::Duration::from_secs(5),
        "取消要及时到达那个被 park 住的任务，实际用了 {took:?}{report}"
    );
    assert!(
        !dir.join("x.rs").exists(),
        "没人同意过，文件不该存在{report}"
    );
}

#[tokio::test]
async fn the_provider_fails_while_a_tool_call_is_outstanding() {
    // The dangling-call case. A model asks for a tool, the tool runs, and the
    // next round dies. Every `tool_call` in the history must have a paired
    // result or the next request is malformed and the conversation is stuck —
    // providers reject an assistant message whose calls have no answers.
    let dir = scratch("dangling");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
            Reply::Fail("died holding the bag"),
        ])
    };
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "read it".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("dangling_call", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        let started = steps.iter().filter(|s| s.kind == "ToolStarted").count();
        let results = steps.iter().filter(|s| s.kind == "ToolResult").count();
        assert_eq!(
            started, results,
            "{who}: provider 死掉也不能留下没有结果的调用{report}"
        );
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 必须终结{report}"
        );
    }
}

#[tokio::test]
async fn one_of_two_parallel_tools_fails() {
    // A batch is only closed correctly if it closes on partial failure too.
    let dir = scratch("mixed-batch");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::Calls(
                "both",
                vec![
                    ("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                    ("c2", "read_file", r#"{"file_path":"missing.rs"}"#),
                ],
            ),
            Reply::Text("one worked"),
        ])
    };
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "read both".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("mixed_batch", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "ToolResult").count(),
            2,
            "{who}: 两个调用两个结果，成败无关{report}"
        );
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.kind == "ToolBatchCompleted")
                .count(),
            1,
            "{who}: 部分失败也要合上批次，否则 UI 上那一组永远转圈{report}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.kind == "ToolResult" && s.detail.contains("error=true")),
            "{who}: 失败的那个要被报成失败{report}"
        );
    }
}

#[tokio::test]
async fn a_second_turn_sees_the_first() {
    // Continuity. Two turns, and the second must be answering with the first
    // still in the history — otherwise every turn is a fresh conversation and
    // nothing the person said earlier counts.
    let dir = scratch("two-turns");
    let script = || Script::text(&["first answer", "second answer"]);
    let a = drive_turns(
        coding_agent(script(), &dir).spawn(),
        &["remember the number 41", "what number?"],
    )
    .await;
    let (handle, _app) = candidate_handle(script(), &dir).await;
    let b = drive_turns(handle, &["remember the number 41", "what number?"]).await;
    let report = render(&a, &b);
    ratchet("two_turns", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            2,
            "{who}: 两条消息两个回合{report}"
        );
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnComplete").count(),
            2,
            "{who}: 各自终结{report}"
        );
    }
}

#[tokio::test]
async fn the_context_a_message_carries_actually_reaches_the_model() {
    // An event-stream comparison cannot answer this: every event could be
    // identical while the context was silently dropped. The judge is the
    // snapshot — what the model was actually shown.
    let dir = scratch("context-visible");
    let cmds = || {
        vec![AgentCommand::SendMessageWithContext {
            text: "what is broken?".into(),
            images: Vec::new(),
            context: "the build fails on line 41".into(),
        }]
    };
    let seen_reference =
        transcript(coding_agent(Script::text(&["ok"]), &dir).spawn(), cmds()).await;
    let (handle, _app) = candidate_handle(Script::text(&["ok"]), &dir).await;
    let seen_candidate = transcript(handle, cmds()).await;

    for (who, seen) in [("参考", &seen_reference), ("候选", &seen_candidate)] {
        assert!(
            seen.contains("the build fails on line 41"),
            "{who}: 上下文没到模型面前：\n{seen}"
        );
        assert!(
            seen.contains("what is broken?"),
            "{who}: 提问也得在：\n{seen}"
        );
    }
}

#[tokio::test]
async fn a_truncated_response() {
    // `finish_reason=length`. Whatever each engine does about it, neither may
    // leave the turn open.
    let dir = scratch("truncated");
    let script = || Script::new(&[Reply::Truncated("this got cut off mid-")]);
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "write a long thing".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    // Known and benign: both engines run the recovery round and both report the
    // same two usages; they differ only in whether the first usage is reported
    // before or after the recovery notice. A driver accumulating usage cannot
    // tell. Frozen so it cannot quietly become something else.
    ratchet("truncated", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 截断不能让回合悬着{report}"
        );
    }
}

/// Long enough that two identical truncated rounds read as a re-dump rather
/// than a coincidence (the check ignores anything under 64 chars).
const REDUMPED: &str =
    "第 1 节:游戏概述。玩家可选性别,只画脸,滚动条调肤色;多点触控时其余脸随机肤色,\
来回判定加分。第 2 节:关卡设计。每关三十秒,失败三次结束。第 3 节:美术风格。";

#[tokio::test]
async fn a_truncation_the_model_answers_by_redumping() {
    // The first cut gets the resume nudge. The model answers it by sending the
    // same text from the top and getting cut again. Nudging again would only
    // spend the budget on the same answer; both engines must stop asking and
    // end the turn on what they have.
    let dir = scratch("truncated-redump");
    let script = || Script::new(&[Reply::Truncated(REDUMPED), Reply::Truncated(REDUMPED)]);
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "write a long thing".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("truncated_redump", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 重灌不能把回合挂死{report}"
        );
        let recoveries = steps
            .iter()
            .filter(|s| s.kind == "TruncationRecovery")
            .count();
        assert_eq!(recoveries, 1, "{who}: 检测到重灌后不能再盲续{report}");
    }
}

#[tokio::test]
async fn a_transient_open_failure_with_a_retry_after() {
    // A relay that cannot reach its upstream for a moment says so with a 503
    // and a `Retry-After`. Both engines must wait the server's word, re-issue
    // the round, and finish the turn as if nothing happened — no error, no
    // second turn.
    let dir = scratch("retry-after");
    let script = || {
        Script::new(&[
            Reply::OpenFail("no upstream available", Some(1)),
            Reply::Text("recovered"),
        ])
    };
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "go".into(),
            images: Vec::new(),
        }]
    };
    let a = reference(script(), &dir, cmds()).await;
    let b = candidate(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("retry_after", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "Error"),
            "{who}: 认了 Retry-After 的瞬时 503 不该以错误收场{report}"
        );
        assert!(
            steps.iter().any(|s| s.kind == "TurnComplete"),
            "{who}: 重试后回合要正常结束{report}"
        );
    }
}

#[tokio::test]
async fn a_manual_compaction() {
    let dir = scratch("compact");
    let cmds = || {
        vec![
            AgentCommand::SendMessage {
                text: "hi".into(),
                images: Vec::new(),
            },
            AgentCommand::Compact { focus: None },
        ]
    };
    let a = reference_until(Script::text(&["ok"]), &dir, cmds(), &[]).await;
    let b = candidate_until(Script::text(&["ok"]), &dir, cmds(), &[]).await;
    let report = render(&a, &b);
    // Known and benign: the candidate emits `CompactionStarted` where the
    // reference goes straight to `Compacted`. The event exists precisely so a
    // driver can show "compacting…" before a possibly multi-second summary, and
    // an engine cannot know in advance that a compaction will be quick — so
    // announcing it is the documented behaviour, not a defect. Frozen at 1 so
    // it cannot quietly become 2.
    ratchet("compact", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 压缩请求不能挂住{report}"
        );
    }
}

#[tokio::test]
async fn a_second_message_steers_the_running_turn() {
    // Typing while the model is answering folds into the turn already running.
    // The invariant a driver depends on: still one turn, not two.
    let dir = scratch("steer");
    let script = || Script::new(&[Reply::Slow(300, "first"), Reply::Text("second")]);
    let cmds = || {
        vec![AgentCommand::SendMessage {
            text: "start".into(),
            images: Vec::new(),
        }]
    };
    let later = || AgentCommand::SendMessage {
        text: "and also this".into(),
        images: Vec::new(),
    };
    let a = reference_late(script(), &dir, cmds(), later()).await;
    let b = candidate_late(script(), &dir, cmds(), later()).await;
    let report = render(&a, &b);
    ratchet("steering", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            1,
            "{who}: 插话折进正在跑的回合，不另开一个{report}"
        );
    }
}

#[tokio::test]
async fn a_message_carrying_context() {
    // Context is model-visible but is not something the user said. Both engines
    // must run exactly one turn for the pair.
    let dir = scratch("context");
    let cmds = || {
        vec![AgentCommand::SendMessageWithContext {
            text: "given that, what next?".into(),
            images: Vec::new(),
            context: "the build is broken".into(),
        }]
    };
    let a = reference(Script::text(&["fix it"]), &dir, cmds()).await;
    let b = candidate(Script::text(&["fix it"]), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("with_context", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            1,
            "{who}: 一条命令一个回合{report}"
        );
    }
}

#[tokio::test]
async fn a_synthetic_message() {
    // The harness speaking on its own initiative — a continuation nudge. Same
    // shape as a user message from the engine's point of view.
    let dir = scratch("synthetic");
    let cmds = || {
        vec![AgentCommand::SendSyntheticMessage {
            text: "keep going".into(),
        }]
    };
    let a = reference(Script::text(&["carrying on"]), &dir, cmds()).await;
    let b = candidate(Script::text(&["carrying on"]), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("synthetic", divergences(&a, &b), &report);

    for (who, steps) in [("参考", &a), ("候选", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 合成消息也要跑完一个回合{report}"
        );
    }
}

#[tokio::test]
async fn a_snapshot_round_trip() {
    // The one I called a structural blocker without measuring it. A driver asks
    // for a snapshot and waits; if the harness cannot answer, everything above
    // it stalls.
    let dir = scratch("snapshot");
    let cmds = || {
        vec![
            AgentCommand::SendMessage {
                text: "hi".into(),
                images: Vec::new(),
            },
            AgentCommand::Snapshot,
        ]
    };
    let a = reference_until(Script::text(&["ok"]), &dir, cmds(), &["Snapshot"]).await;
    let b = candidate_until(Script::text(&["ok"]), &dir, cmds(), &["Snapshot"]).await;
    let report = render(&a, &b);
    ratchet("snapshot", divergences(&a, &b), &report);
}

// ---- the same scenarios, against the PRODUCTION assembly -----------------
//
// Their own ratchet keys (`*_prod`) so the minimal-path numbers stay readable
// beside them: the two measure different engines, and collapsing them would
// hide which one moved.

/// One scenario through the production assembly and the candidate tree.
async fn prod_vs_candidate(
    key: &str,
    dir: &std::path::Path,
    script: impl Fn() -> Arc<Script>,
    cmds: impl Fn() -> Vec<AgentCommand>,
) -> (Vec<Step>, Vec<Step>, String) {
    let a = reference_production(script(), dir, cmds()).await;
    let b = candidate(script(), dir, cmds()).await;
    let report = render(&a, &b);
    ratchet(key, divergences(&a, &b), &report);
    (a, b, report)
}

fn say(text: &str) -> Vec<AgentCommand> {
    vec![AgentCommand::SendMessage {
        text: text.into(),
        images: Vec::new(),
    }]
}

#[tokio::test]
async fn a_plain_turn_in_production() {
    let dir = scratch("plain-prod");
    let (a, b, report) = prod_vs_candidate(
        "plain_turn_prod",
        &dir,
        || Script::text(&["hello"]),
        || say("say hello"),
    )
    .await;
    for (who, steps) in [("参考（生产）", &a), ("候选", &b)] {
        let terminals = steps
            .iter()
            .filter(|s| matches!(s.kind, "TurnComplete" | "Error" | "Cancelled"))
            .count();
        assert_eq!(terminals, 1, "{who} 的终结事件不是一个{report}");
    }
}

#[tokio::test]
async fn one_tool_call_in_production() {
    // The first scenario that actually crosses the production middleware chain:
    // argument repair, plan mode, the workspace and credential gates, approval,
    // datalog, artifact spill. The minimal path mounts almost none of them.
    let dir = scratch("one-tool-prod");
    seed(&dir);
    let (a, b, report) = prod_vs_candidate(
        "one_tool_call_prod",
        &dir,
        || {
            Script::new(&[
                Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                Reply::Text("that is an empty main"),
            ])
        },
        || say("read a.rs"),
    )
    .await;
    for (who, steps) in [("参考（生产）", &a), ("候选", &b)] {
        let started = steps.iter().position(|s| s.kind == "ToolStarted");
        let result = steps.iter().position(|s| s.kind == "ToolResult");
        assert!(
            matches!((started, result), (Some(x), Some(y)) if x < y),
            "{who}: 工具必须先开始后有结果{report}"
        );
    }
}

#[tokio::test]
async fn two_tool_calls_at_once_in_production() {
    let dir = scratch("two-tools-prod");
    seed(&dir);
    let (a, b, report) = prod_vs_candidate(
        "two_tool_calls_prod",
        &dir,
        || {
            Script::new(&[
                Reply::Calls(
                    "reading both",
                    vec![
                        ("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                        ("c2", "read_file", r#"{"file_path":"b.rs"}"#),
                    ],
                ),
                Reply::Text("both read"),
            ])
        },
        || say("read both"),
    )
    .await;
    // Every call that started must land, or a driver waits on one that never does.
    for (who, steps) in [("参考（生产）", &a), ("候选", &b)] {
        let started = steps.iter().filter(|s| s.kind == "ToolStarted").count();
        let landed = steps.iter().filter(|s| s.kind == "ToolResult").count();
        assert_eq!(started, landed, "{who}: 开始与落地的工具数不等{report}");
    }
}

#[tokio::test]
async fn several_rounds_in_production() {
    let dir = scratch("rounds-prod");
    seed(&dir);
    let (_a, _b, _report) = prod_vs_candidate(
        "several_rounds_prod",
        &dir,
        || {
            Script::new(&[
                Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                Reply::call("c2", "read_file", r#"{"file_path":"b.rs"}"#),
                Reply::Text("read them both"),
            ])
        },
        || say("read both, one at a time"),
    )
    .await;
}

#[tokio::test]
async fn one_of_two_parallel_tools_fails_in_production() {
    let dir = scratch("mixed-prod");
    seed(&dir);
    let (a, b, report) = prod_vs_candidate(
        "mixed_batch_prod",
        &dir,
        || {
            Script::new(&[
                Reply::Calls(
                    "both",
                    vec![
                        ("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                        ("c2", "read_file", r#"{"file_path":"missing.rs"}"#),
                    ],
                ),
                Reply::Text("one worked"),
            ])
        },
        || say("read both"),
    )
    .await;
    for (who, steps) in [("参考（生产）", &a), ("候选", &b)] {
        let started = steps.iter().filter(|s| s.kind == "ToolStarted").count();
        let landed = steps.iter().filter(|s| s.kind == "ToolResult").count();
        assert_eq!(started, landed, "{who}: 一个失败不该让另一个悬空{report}");
    }
}

#[tokio::test]
async fn the_provider_fails_mid_stream_in_production() {
    let dir = scratch("provider-error-prod");
    let (_a, _b, _report) = prod_vs_candidate(
        "provider_error_prod",
        &dir,
        || Script::new(&[Reply::Fail("upstream exploded")]),
        || say("go"),
    )
    .await;
}

// ---- coding, assembled ON the harness ------------------------------------
//
// The third engine, and the one this whole file exists for. `reference_production`
// is coding's own chain; this is the SAME product assembled as a row list
// (`on_harness::mount`). If the two agree, the chain can be replaced by the list
// without anything above noticing — which is the swap, stated as a measurement.

/// The hermetic scoping every on-harness scenario shares.
///
/// Nothing may reach the network, a subprocess, or the developer's own disk.
fn quiet_rows(dir: &std::path::Path) -> String {
    let empty = dir.join("__no_skills__");
    let _ = std::fs::create_dir_all(&empty);
    format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {dir:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {dir:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {dir:?}, home = {home:?} }}\n",
        dir = dir.to_string_lossy(),
        home = empty.to_string_lossy(),
    )
}

/// A mounted coding-on-harness tree and its handle.
///
/// Both, for the same reason `candidate_handle` returns both: dropping the `App`
/// unloads every row, and the next command would go to a conversation whose
/// services have all been torn down.
async fn on_harness_handle(
    script: Arc<Script>,
    dir: &std::path::Path,
) -> (AgentHandle, atomcode_plexus::App) {
    let quiet = quiet_rows(dir);
    atomcode_coding::on_harness::mount(
        dir,
        atomcode_coding::on_harness::Presence::Attended,
        script,
        &[quiet.as_str()],
    )
    .await
    .expect("the coding-on-harness tree must mount")
}

async fn on_harness_inner(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
    late: Option<AgentCommand>,
    answer: serde_json::Value,
) -> Vec<Step> {
    let (handle, mut app) = on_harness_handle(script, dir).await;
    let steps = drive_answering(handle, commands, also, late, answer).await;
    app.stop();
    steps
}

async fn on_harness(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    on_harness_inner(script, dir, commands, &[], None, allow()).await
}

async fn on_harness_until(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
) -> Vec<Step> {
    on_harness_inner(script, dir, commands, also, None, allow()).await
}

async fn on_harness_late(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    late: AgentCommand,
) -> Vec<Step> {
    on_harness_inner(script, dir, commands, &[], Some(late), allow()).await
}

// The production side needs the same three drivers, or half the scenario list
// below could only be run against one of the two engines — which is not a
// differential at all.

async fn reference_production_until(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
) -> Vec<Step> {
    drive_until(production_agent(script, dir).await.spawn(), commands, also).await
}

async fn reference_production_late(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    late: AgentCommand,
) -> Vec<Step> {
    drive_with(
        production_agent(script, dir).await.spawn(),
        commands,
        &[],
        Some(late),
    )
    .await
}

/// Render and ratchet a pair of runs the caller drove itself.
///
/// `chain_vs_rows` covers the scenarios whose two sides are driven identically;
/// the ones that cancel, steer, or wait for a late reply are not, and inventing
/// a closure shape general enough for all of them costs more than this line.
fn judge(key: &str, a: &[Step], b: &[Step]) -> String {
    let report = render(a, b);
    ratchet(key, divergences(a, b), &report);
    report
}

/// One scenario through coding's own chain and the same product as a row list.
async fn chain_vs_rows(
    key: &str,
    dir: &std::path::Path,
    script: impl Fn() -> Arc<Script>,
    cmds: impl Fn() -> Vec<AgentCommand>,
) -> (Vec<Step>, Vec<Step>, String) {
    let a = reference_production(script(), dir, cmds()).await;
    let b = on_harness(script(), dir, cmds()).await;
    let report = render(&a, &b);
    ratchet(key, divergences(&a, &b), &report);
    (a, b, report)
}

#[tokio::test]
async fn a_plain_turn_on_the_harness() {
    let dir = scratch("plain-onharness");
    let (a, b, report) = chain_vs_rows(
        "plain_turn_rows",
        &dir,
        || Script::text(&["hello"]),
        || say("say hello"),
    )
    .await;
    for (who, steps) in [("链式", &a), ("行式", &b)] {
        let terminals = steps
            .iter()
            .filter(|s| matches!(s.kind, "TurnComplete" | "Error" | "Cancelled"))
            .count();
        assert_eq!(terminals, 1, "{who} 的终结事件不是一个{report}");
    }
}

#[tokio::test]
async fn one_tool_call_on_the_harness() {
    let dir = scratch("one-tool-onharness");
    seed(&dir);
    let (a, b, report) = chain_vs_rows(
        "one_tool_call_rows",
        &dir,
        || {
            Script::new(&[
                Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                Reply::Text("that is an empty main"),
            ])
        },
        || say("read a.rs"),
    )
    .await;
    for (who, steps) in [("链式", &a), ("行式", &b)] {
        let started = steps.iter().position(|s| s.kind == "ToolStarted");
        let result = steps.iter().position(|s| s.kind == "ToolResult");
        assert!(
            matches!((started, result), (Some(x), Some(y)) if x < y),
            "{who}: 工具必须先开始后有结果{report}"
        );
    }
}

#[tokio::test]
async fn several_rounds_on_the_harness() {
    let dir = scratch("rounds-onharness");
    seed(&dir);
    let _ = chain_vs_rows(
        "several_rounds_rows",
        &dir,
        || {
            Script::new(&[
                Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                Reply::call("c2", "read_file", r#"{"file_path":"b.rs"}"#),
                Reply::Text("read them both"),
            ])
        },
        || say("read both, one at a time"),
    )
    .await;
}

// ---- the approval line, on both engines ----------------------------------
//
// The gap this closes. Until now approval was the one thing the rig could not
// compare: the candidate tree mounted `approval` disabled, and the three
// approval scenarios above compare the candidate to ITSELF (`render(&steps,
// &steps)`). So the five gates that were moved onto the harness — the whole
// approval gradient — had no independent judge at all.
//
// They CAN be compared, because both engines ask through the same channel in the
// end: coding's `ApprovalMiddleware` round-trips the driver via `RequestCtx`, and
// the harness's `approval-interactive` asks `user-questions`, which the
// `ui-handle` row fills with an `Asker` that round-trips the driver too. Two
// different insides, one observable: `AgentEvent::Request`, answered with
// `AgentCommand::Respond`.

async fn reference_production_answering(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    answer: serde_json::Value,
) -> Vec<Step> {
    let agent = production_agent(script, dir).await;
    drive_answering(agent.spawn(), commands, &[], None, answer).await
}

/// The same tree, with the driver answering approvals.
///
/// Disabling the `approval` row is what ENABLES asking here, which reads
/// backwards until you see who fills the seam: that row is the `deny-risky`
/// policy (refuse, never ask), and turning it off lets `ui-handle` claim
/// `approval` and round-trip the driver instead. `ui-handle` PROVIDES the
/// `approval` seam itself — behind the handle protocol the driver IS the
/// person, so a call that needs authorization round-trips as
/// `AgentEvent::Request`, the same observable coding's `ApprovalMiddleware`
/// produces. Two earlier attempts here are worth recording:
/// `bundle::INTERACTIVE` also enables `user-questions-unattended`, which takes
/// the `user-questions` seam away from `ui-handle`; and enabling
/// `approval-interactive` collides with `ui-handle` over `approval` itself.
/// Both errors were the tree saying the front end had already answered this
/// question.
async fn on_harness_answering(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    answer: serde_json::Value,
) -> Vec<Step> {
    on_harness_inner(script, dir, commands, &[], None, answer).await
}

/// One approval scenario through both engines.
async fn ask_chain_vs_rows(
    key: &str,
    dir: &std::path::Path,
    script: impl Fn() -> Arc<Script>,
    cmds: impl Fn() -> Vec<AgentCommand>,
    answer: serde_json::Value,
) -> (Vec<Step>, Vec<Step>, String) {
    let a = reference_production_answering(script(), dir, cmds(), answer.clone()).await;
    let b = on_harness_answering(script(), dir, cmds(), answer).await;
    let report = render(&a, &b);
    ratchet(key, divergences(&a, &b), &report);
    (a, b, report)
}

/// A write to a path OUTSIDE the workspace: risky on both engines, and the first
/// thing either of them should want a person for.
///
/// A LITERAL relative path, resolved against the working dir — `../x` from the
/// scratch dir lands in its parent, outside the workspace. Literal because
/// `Reply::call` takes `&'static str`, and relative because that is also the
/// shape a model actually produces.
fn write_outside(rel: &'static str) -> Arc<Script> {
    let args: &'static str = match rel {
        "../outside-yes.txt" => r#"{"file_path":"../outside-yes.txt","content":"x"}"#,
        "../outside-no.txt" => r#"{"file_path":"../outside-no.txt","content":"x"}"#,
        other => panic!("unknown fixture path {other}"),
    };
    Script::new(&[
        Reply::call("c1", "write_file", args),
        Reply::Text("written"),
    ])
}

#[tokio::test]
async fn a_write_outside_the_workspace_diverges_and_here_is_why() {
    // This scenario has been wrong twice, and both times it read as GREEN.
    //
    // First it described a divergence the `Presence` rule had already closed.
    // Then — found while mapping what is left before the default path can
    // switch — it turned out neither engine was ASKING at all, and the reason
    // was the rig: `scratch()` hands out a directory under the system temp
    // root, `write_approval::path_in_temp_dir` deliberately treats anything
    // under a temp root as a benign throwaway write, and so `../x.txt` from a
    // temp scratch dir is auto-approved by design on BOTH engines. Zero
    // requests, zero divergence, nothing measured. The `allow()` answer was
    // never consumed.
    //
    // `scratch_outside_temp` makes the question real again, and with a real
    // question both engines ask — which is the good news. What they disagree
    // about is WHEN:
    //
    //   链式  Request approval → ToolStarted → ToolResult
    //   行式  ToolStarted → Request approval → ToolResult
    //
    // The same `ui-handle` property that
    // `a_refused_call_still_reads_as_started_on_the_harness` owns, and the
    // fourth scenario to carry it — but the worst-looking instance: a person
    // watching sees the write announced as started and is THEN asked whether to
    // allow it. Frozen at 2 (one difference, reported twice because the
    // alignment cannot pair events that moved past each other).
    let dir = scratch_outside_temp("ask-yes");
    let outside = dir.parent().unwrap().join("outside-yes.txt");
    let _ = std::fs::remove_file(&outside);
    let (a, b, report) = ask_chain_vs_rows(
        "approval_write_outside_rows",
        &dir,
        || write_outside("../outside-yes.txt"),
        || say("write it"),
        allow(),
    )
    .await;
    // Whatever else differs, neither engine may hang or run two turns: a driver
    // that sees no terminal waits forever, and one that sees two runs twice.
    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnComplete").count(),
            1,
            "{who}: 仍然只有一个终结{report}"
        );
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 回合必须结束{report}"
        );
    }
    let _ = std::fs::remove_file(&outside);
}

#[tokio::test]
async fn a_refusal_is_the_call_not_the_turn_on_both_engines() {
    // The one scenario here that genuinely round-trips the approval seam.
    //
    // Its first version reused the write-outside script and answered `deny`.
    // That was vacuous: neither engine ASKS about that write, so the `deny`
    // was never consumed and the test asserted "a refusal ends the call" while
    // watching a successful write. It passed for a year of reasons that had
    // nothing to do with refusal.
    //
    // A recursive `rm` is a call both engines really do stop for, which is what
    // makes the answer mean something: `Request approval` on both, refused on
    // both. The claim is not which engine refuses — it is that a refusal ends
    // the CALL and the turn carries on. An engine that ended the turn instead
    // would strand a driver mid-conversation, and that failure is invisible in a
    // green test that only checks the file.
    let dir = scratch("ask-no");
    let (a, b, report) = ask_chain_vs_rows(
        "approval_refusal_rows",
        &dir,
        || {
            Script::new(&[
                Reply::call(
                    "c1",
                    "bash",
                    r#"{"command":"rm -rf /tmp/nope-does-not-exist"}"#,
                ),
                Reply::Text("stopped"),
            ])
        },
        || say("clean it up"),
        deny(),
    )
    .await;
    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "Request").count(),
            1,
            "{who}: 这一条的全部意义就是真的问了人一次{report}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.kind == "ToolResult" && s.detail.contains("error=true")),
            "{who}: 答了 deny,调用就得失败{report}"
        );
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnComplete").count(),
            1,
            "{who}: 拒绝结束的是调用,不是回合{report}"
        );
    }
}

#[tokio::test]
async fn headless_refuses_what_attended_would_ask_about() {
    // The other half of the rule, and its negative control. `Attended` above now
    // agrees with coding to the event — which on its own would also be true of an
    // assembly that simply never fenced anything. What makes the rule a rule is
    // that the SAME call, with nobody to ask, is refused.
    //
    // A prompt nobody answers is an auto-approval wearing a question mark, and a
    // subagent is exactly where that would go unnoticed.
    let dir = scratch("headless");
    let outside = dir.parent().unwrap().join("outside-headless.txt");
    let _ = std::fs::remove_file(&outside);
    let empty = dir.join("__no_skills_hl__");
    let _ = std::fs::create_dir_all(&empty);
    let quiet = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"mcp\"\ndisabled = true\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {dir:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {dir:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {dir:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {dir:?}, home = {home:?} }}\n",
        dir = dir.to_string_lossy(),
        home = empty.to_string_lossy(),
    );
    let args = r#"{"file_path":"../outside-headless.txt","content":"x"}"#;
    let script = Script::new(&[
        Reply::call("c1", "write_file", args),
        Reply::Text("written"),
    ]);
    let (handle, mut app) = atomcode_coding::on_harness::mount(
        &dir,
        atomcode_coding::on_harness::Presence::Headless,
        script,
        &[quiet.as_str()],
    )
    .await
    .expect("the headless tree must mount");
    let steps = drive_answering(handle, say("write it"), &[], None, allow()).await;
    app.stop();
    let report = render(&steps, &steps);

    assert!(
        steps
            .iter()
            .any(|s| s.kind == "ToolResult" && s.detail.contains("error=true")),
        "工作区外的写必须被拒{report}"
    );
    assert_eq!(
        steps.iter().filter(|s| s.kind == "Request").count(),
        0,
        "没人在,就不该假装问{report}"
    );
    assert!(!outside.exists(), "文件不该被写出来{report}");
    assert_eq!(
        steps.iter().filter(|s| s.kind == "TurnComplete").count(),
        1,
        "拒绝结束的是调用,不是回合{report}"
    );
}

// ---- the rest of the scenario list, on the harness -----------------------
//
// Until now the chain-vs-rows comparison covered three happy paths: a plain
// turn, one tool call, several rounds. Everything that makes an engine swap
// frightening — a cancel landing mid-round, a second message folding into a
// running turn, a truncated response, a 503 with a `Retry-After`, a compaction,
// a snapshot round trip — was measured only against the MINIMAL path, an engine
// nobody ships. So the rig was quietest exactly where the risk lives.
//
// These are the same scripts and the same invariants as the minimal-path
// scenarios above, with the production assembly on one side and the row list on
// the other, under `*_rows` keys.

#[tokio::test]
async fn two_tool_calls_at_once_on_the_harness() {
    let dir = scratch("two-tools-onharness");
    seed(&dir);
    let (a, b, report) = chain_vs_rows(
        "two_tool_calls_rows",
        &dir,
        || {
            Script::new(&[
                Reply::Calls(
                    "reading both",
                    vec![
                        ("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                        ("c2", "read_file", r#"{"file_path":"b.rs"}"#),
                    ],
                ),
                Reply::Text("both read"),
            ])
        },
        || say("read both"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        let started = steps.iter().filter(|s| s.kind == "ToolStarted").count();
        let results = steps.iter().filter(|s| s.kind == "ToolResult").count();
        assert_eq!(
            started, results,
            "{who}: 每个开始的调用都必须有结果，否则驱动方会永远等那一个{report}"
        );
        assert_eq!(started, 2, "{who}: 两个调用{report}");
    }
}

#[tokio::test]
async fn one_of_two_parallel_tools_fails_on_the_harness() {
    let dir = scratch("mixed-batch-onharness");
    seed(&dir);
    let (a, b, report) = chain_vs_rows(
        "mixed_batch_rows",
        &dir,
        || {
            Script::new(&[
                Reply::Calls(
                    "both",
                    vec![
                        ("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                        ("c2", "read_file", r#"{"file_path":"missing.rs"}"#),
                    ],
                ),
                Reply::Text("one worked"),
            ])
        },
        || say("read both"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "ToolResult").count(),
            2,
            "{who}: 两个调用两个结果，成败无关{report}"
        );
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.kind == "ToolBatchCompleted")
                .count(),
            1,
            "{who}: 部分失败也要合上批次，否则 UI 上那一组永远转圈{report}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.kind == "ToolResult" && s.detail.contains("error=true")),
            "{who}: 失败的那个要被报成失败{report}"
        );
    }
}

#[tokio::test]
async fn a_tool_that_does_not_exist_on_the_harness() {
    let dir = scratch("no-such-tool-onharness");
    let (a, b, report) = chain_vs_rows(
        "unknown_tool_rows",
        &dir,
        || {
            Script::new(&[
                Reply::call("c1", "no_such_tool", "{}"),
                Reply::Text("ah, my mistake"),
            ])
        },
        || say("use it"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 未知工具不能挂死回合{report}"
        );
    }
}

#[tokio::test]
async fn the_provider_fails_mid_stream_on_the_harness() {
    let dir = scratch("provider-fails-onharness");
    let (a, b, report) = chain_vs_rows(
        "provider_error_rows",
        &dir,
        || Script::new(&[Reply::Fail("upstream exploded")]),
        || say("go"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        let terminals = steps
            .iter()
            .filter(|s| matches!(s.kind, "TurnComplete" | "Error"))
            .count();
        assert!(terminals >= 1, "{who}: 失败也要终结回合{report}");
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 失败不能表现成挂死{report}"
        );
    }
}

#[tokio::test]
async fn the_provider_fails_while_a_tool_call_is_outstanding_on_the_harness() {
    // The dangling-call case: every `tool_call` in the history must have a
    // paired result, or the next request is malformed and the conversation is
    // stuck. A row list that drops the pairing would look fine event-for-event
    // right up until the next turn.
    let dir = scratch("dangling-onharness");
    seed(&dir);
    let (a, b, report) = chain_vs_rows(
        "dangling_call_rows",
        &dir,
        || {
            Script::new(&[
                Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
                Reply::Fail("died holding the bag"),
            ])
        },
        || say("read it"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        let started = steps.iter().filter(|s| s.kind == "ToolStarted").count();
        let results = steps.iter().filter(|s| s.kind == "ToolResult").count();
        assert_eq!(
            started, results,
            "{who}: provider 死掉也不能留下没有结果的调用{report}"
        );
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 必须终结{report}"
        );
    }
}

#[tokio::test]
async fn a_cancel_lands_on_the_harness() {
    // The timing half of this does not show up in the event stream at all: the
    // same events arrive either way, just late. A row list that queued the
    // cancel behind the in-flight request would pass every event comparison and
    // still feel broken to a person holding ctrl-c.
    let dir = scratch("cancel-onharness");
    let script = || Script::new(&[Reply::Slow(400, "working")]);

    // Both engines are built BEFORE either clock starts. The first version of
    // this timed `production_agent()` too, and `prepare()` is a real async
    // setup step — it read 416ms for a 400ms round and blamed the cancel. It
    // would also have been unfair the other way: mounting a plexus tree is not
    // free either, and neither cost is what this scenario is about.
    let chain = production_agent(script(), &dir).await;
    let started = std::time::Instant::now();
    let a = drive_with(chain.spawn(), say("go"), &[], Some(AgentCommand::Cancel)).await;
    let took_chain = started.elapsed();

    let (handle, mut app) = on_harness_handle(script(), &dir).await;
    let started = std::time::Instant::now();
    let b = drive_answering(handle, say("go"), &[], Some(AgentCommand::Cancel), allow()).await;
    let took_rows = started.elapsed();
    app.stop();
    let report = judge("cancel_rows", &a, &b);

    for (who, took) in [("链式", took_chain), ("行式", took_rows)] {
        assert!(
            took < std::time::Duration::from_millis(380),
            "{who}: 取消用了 {took:?} —— 那一轮本来就要 400ms，说明它在等回合跑完\
             而不是中断它{report}"
        );
    }
    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 取消之后回合必须结束{report}"
        );
        // `Cancelled` is a notification, not a terminal: it arrives BESIDE the
        // `TurnComplete` that closes the turn. Counting it here read 2 on both
        // engines — the assertion was wrong, not the engines.
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s.kind, "TurnComplete" | "Error"))
                .count(),
            1,
            "{who}: 恰好一个终结{report}"
        );
    }
}

#[tokio::test]
async fn a_second_message_steers_the_running_turn_on_the_harness() {
    let dir = scratch("steer-onharness");
    let script = || Script::new(&[Reply::Slow(300, "first"), Reply::Text("second")]);
    let later = || AgentCommand::SendMessage {
        text: "and also this".into(),
        images: Vec::new(),
    };
    let a = reference_production_late(script(), &dir, say("start"), later()).await;
    let b = on_harness_late(script(), &dir, say("start"), later()).await;
    let report = judge("steering_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            1,
            "{who}: 插话折进正在跑的回合，不另开一个{report}"
        );
    }
}

#[tokio::test]
async fn a_truncated_response_on_the_harness() {
    // Frozen at 2, the same number the minimal path carries: both engines run
    // the recovery round and both report the same two usages, differing only in
    // whether the first is reported before or after the recovery notice. A
    // driver accumulating usage cannot tell. What matters for the swap is that
    // the production chain adds NO new divergence on top of it.
    let dir = scratch("truncated-onharness");
    let (a, b, report) = chain_vs_rows(
        "truncated_rows",
        &dir,
        || Script::new(&[Reply::Truncated("this got cut off mid-")]),
        || say("write a long thing"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 截断不能让回合悬着{report}"
        );
    }
}

#[tokio::test]
async fn a_truncation_the_model_answers_by_redumping_on_the_harness() {
    // Frozen at 2 for the same reason as the single truncation above.
    let dir = scratch("truncated-redump-onharness");
    let (a, b, report) = chain_vs_rows(
        "truncated_redump_rows",
        &dir,
        || Script::new(&[Reply::Truncated(REDUMPED), Reply::Truncated(REDUMPED)]),
        || say("write a long thing"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 重灌不能把回合挂死{report}"
        );
        let recoveries = steps
            .iter()
            .filter(|s| s.kind == "TruncationRecovery")
            .count();
        assert_eq!(recoveries, 1, "{who}: 检测到重灌后不能再盲续{report}");
    }
}

#[tokio::test]
async fn a_transient_open_failure_with_a_retry_after_on_the_harness() {
    let dir = scratch("retry-after-onharness");
    let (a, b, report) = chain_vs_rows(
        "retry_after_rows",
        &dir,
        || {
            Script::new(&[
                Reply::OpenFail("no upstream available", Some(1)),
                Reply::Text("recovered"),
            ])
        },
        || say("go"),
    )
    .await;

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "Error"),
            "{who}: 认了 Retry-After 的瞬时 503 不该以错误收场{report}"
        );
        assert!(
            steps.iter().any(|s| s.kind == "TurnComplete"),
            "{who}: 重试后回合要正常结束{report}"
        );
    }
}

#[tokio::test]
async fn a_manual_compaction_on_the_harness() {
    let dir = scratch("compact-onharness");
    let cmds = || {
        vec![
            AgentCommand::SendMessage {
                text: "hi".into(),
                images: Vec::new(),
            },
            AgentCommand::Compact { focus: None },
        ]
    };
    let a = reference_production_until(Script::text(&["ok"]), &dir, cmds(), &[]).await;
    let b = on_harness_until(Script::text(&["ok"]), &dir, cmds(), &[]).await;
    // Frozen at 1, the same as the minimal path: the row list emits
    // `CompactionStarted` where the chain goes straight to `Compacted`. That
    // event exists precisely so a driver can show "compacting…" before a
    // possibly multi-second summary, and an engine cannot know in advance that
    // a compaction will be quick — announcing it is the documented behaviour.
    let report = judge("compact_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 压缩请求不能挂住{report}"
        );
    }
}

#[tokio::test]
async fn a_snapshot_round_trip_on_the_harness() {
    // A driver asks for a snapshot and waits. If the row list cannot answer,
    // everything above it stalls — and the stall would be invisible to every
    // other scenario here, because none of them ask.
    let dir = scratch("snapshot-onharness");
    let cmds = || {
        vec![
            AgentCommand::SendMessage {
                text: "hi".into(),
                images: Vec::new(),
            },
            AgentCommand::Snapshot,
        ]
    };
    let a = reference_production_until(Script::text(&["ok"]), &dir, cmds(), &["Snapshot"]).await;
    let b = on_harness_until(Script::text(&["ok"]), &dir, cmds(), &["Snapshot"]).await;
    let report = judge("snapshot_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            steps.iter().any(|s| s.kind == "Snapshot"),
            "{who}: 问了快照就得答{report}"
        );
    }
}

#[tokio::test]
async fn a_message_carrying_context_on_the_harness() {
    let dir = scratch("context-onharness");
    let cmds = || {
        vec![AgentCommand::SendMessageWithContext {
            text: "given that, what next?".into(),
            images: Vec::new(),
            context: "the build is broken".into(),
        }]
    };
    let a = reference_production(Script::text(&["fix it"]), &dir, cmds()).await;
    let b = on_harness(Script::text(&["fix it"]), &dir, cmds()).await;
    let report = judge("with_context_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            1,
            "{who}: 一条命令一个回合{report}"
        );
    }
}

#[tokio::test]
async fn a_synthetic_message_on_the_harness() {
    let dir = scratch("synthetic-onharness");
    let cmds = || {
        vec![AgentCommand::SendSyntheticMessage {
            text: "keep going".into(),
        }]
    };
    let a = reference_production(Script::text(&["carrying on"]), &dir, cmds()).await;
    let b = on_harness(Script::text(&["carrying on"]), &dir, cmds()).await;
    let report = judge("synthetic_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            !steps.iter().any(|s| s.kind == "TIMEOUT"),
            "{who}: 合成消息也要跑完一个回合{report}"
        );
    }
}

#[tokio::test]
async fn a_second_turn_sees_the_first_on_the_harness() {
    // Continuity across turns. The row list holds the conversation in a service
    // rather than in a middleware chain's captured state, so "does turn two see
    // turn one" is a question the swap genuinely reopens.
    let dir = scratch("two-turns-onharness");
    let script = || Script::text(&["first answer", "second answer"]);
    let asked = ["remember the number 41", "what number?"];

    let a = drive_turns(production_agent(script(), &dir).await.spawn(), &asked).await;
    let (handle, mut app) = on_harness_handle(script(), &dir).await;
    let b = drive_turns(handle, &asked).await;
    app.stop();
    let report = judge("two_turns_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            2,
            "{who}: 两条消息两个回合{report}"
        );
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnComplete").count(),
            2,
            "{who}: 各自终结{report}"
        );
    }
}

#[tokio::test]
async fn the_context_a_message_carries_reaches_the_model_on_the_harness() {
    // Not a diff: an event-stream comparison cannot answer this. Every event
    // could match while the context was silently dropped on the way to the
    // provider. The judge is the snapshot — what the model was actually shown.
    let dir = scratch("context-visible-onharness");
    let cmds = || {
        vec![AgentCommand::SendMessageWithContext {
            text: "what is broken?".into(),
            images: Vec::new(),
            context: "the build fails on line 41".into(),
        }]
    };
    let seen_chain = transcript(
        production_agent(Script::text(&["ok"]), &dir).await.spawn(),
        cmds(),
    )
    .await;
    let (handle, mut app) = on_harness_handle(Script::text(&["ok"]), &dir).await;
    let seen_rows = transcript(handle, cmds()).await;
    app.stop();

    for (who, seen) in [("链式", &seen_chain), ("行式", &seen_rows)] {
        assert!(
            seen.contains("the build fails on line 41"),
            "{who}: 上下文没到模型面前：\n{seen}"
        );
        assert!(
            seen.contains("what is broken?"),
            "{who}: 提问也得在：\n{seen}"
        );
    }
}

// ---- the self-correction loop, on the harness ---------------------------
//
// One of the three things this crate says it owns (lib.rs: assembly, persona,
// discipline). It was the one thing nothing here measured: no other script
// edits a file and then walks away, which is the exact shape the cadence
// exists for. Asked directly, the rig answered immediately — the chain ran a
// third model call and the row list stopped at two.
//
// Both sides are UNATTENDED here, which is what makes it a comparison:
// `CodingAgentConfig::new` leaves `interactive: false`, so the chain's cadence
// is armed, and `Presence::Headless` is the row list's way of saying the same
// thing. An attended pair is the negative control below.

/// A mounted headless tree and its handle.
async fn on_harness_headless(
    script: Arc<Script>,
    dir: &std::path::Path,
) -> (AgentHandle, atomcode_plexus::App) {
    let quiet = quiet_rows(dir);
    atomcode_coding::on_harness::mount(
        dir,
        atomcode_coding::on_harness::Presence::Headless,
        script,
        &[quiet.as_str()],
    )
    .await
    .expect("the headless coding-on-harness tree must mount")
}

/// An in-workspace code edit the model walks away from without checking.
fn edits_and_stops() -> Arc<Script> {
    Script::new(&[
        Reply::call(
            "c1",
            "write_file",
            r#"{"file_path":"a.rs","content":"fn main() { let x: i32 = 1; }"}"#,
        ),
        Reply::Text("done"),
        Reply::Text("checked, it compiles"),
    ])
}

/// Model calls in a run, one `Usage` apiece.
///
/// The observable the cadence moves: a continuation is one more round, and a
/// round is one more request. Nothing else in the stream distinguishes "the
/// turn ended" from "the turn ended after being asked to check its work".
fn model_calls(steps: &[Step]) -> usize {
    steps.iter().filter(|s| s.kind == "Usage").count()
}

#[tokio::test]
async fn an_edit_that_was_never_verified_is_asked_about_on_both_engines() {
    let dir = scratch("verify-cadence-onharness");
    seed(&dir);
    let a = reference_production(edits_and_stops(), &dir, say("fix a.rs")).await;
    let (handle, mut app) = on_harness_headless(edits_and_stops(), &dir).await;
    let b = drive_answering(handle, say("fix a.rs"), &[], None, allow()).await;
    app.stop();
    let report = judge("verify_cadence_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            model_calls(steps),
            3,
            "{who}: 改了代码就走,必须多问一轮 —— 两轮说明根本没问{report}"
        );
        assert_eq!(
            steps.iter().filter(|s| s.kind == "TurnStarted").count(),
            1,
            "{who}: 续问折进同一个回合,不另开一个{report}"
        );
    }
}

#[tokio::test]
async fn an_attended_run_does_not_force_the_check() {
    // The negative control, and the half that makes it a rule rather than a
    // behaviour. `Attended` agreeing with an armed chain would also be true of
    // a row list that simply never nudged — what makes it a rule is that the
    // SAME edit, with a person watching, is left alone on both engines.
    //
    // A present human sees the edit and can ask for the check; forcing it
    // spends a round on something they were about to decide themselves.
    let dir = scratch("verify-cadence-attended");
    seed(&dir);
    let mut cfg =
        atomcode_coding::CodingAgentConfig::new("k", "http://unused.test/v1", "script", &dir);
    cfg.interactive = true;
    let opts = atomcode_coding::parts::PrepareOptions {
        mcp: false,
        web: false,
        review: false,
        memory: false,
        skill_dirs: Some(Vec::new()),
        ..Default::default()
    };
    let mut parts = atomcode_coding::parts::prepare(&cfg, opts)
        .await
        .expect("prepare");
    let chain =
        atomcode_coding::parts::assemble(&mut parts, &cfg, edits_and_stops()).expect("assemble");
    let a = drive_until(chain.spawn(), say("fix a.rs"), &[]).await;
    let b = on_harness(edits_and_stops(), &dir, say("fix a.rs")).await;
    let report = judge("verify_cadence_attended_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            model_calls(steps),
            2,
            "{who}: 有人看着就别强制查 —— 多出来的那轮是抢了人的活{report}"
        );
    }
}

#[tokio::test]
async fn a_user_who_forbade_the_shell_is_not_nudged_into_using_it() {
    // The cadence must not override the person it stands in for. Headless on
    // both sides — the mode where the nudge IS forced — so the only thing that
    // can hold it back is the user's own instruction.
    //
    // This is the gap the first version of the row shipped with: it asked
    // `unverified_edit` and nothing else, so a user who said "no shell
    // commands" would have been nudged to run one anyway.
    let dir = scratch("verify-cadence-forbidden");
    seed(&dir);
    let asked = || say("fix a.rs, and do not run any command");
    let a = reference_production(edits_and_stops(), &dir, asked()).await;
    let (handle, mut app) = on_harness_headless(edits_and_stops(), &dir).await;
    let b = drive_answering(handle, asked(), &[], None, allow()).await;
    app.stop();
    let report = judge("verify_cadence_forbidden_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert_eq!(
            model_calls(steps),
            2,
            "{who}: 人说了不许跑命令,就不能再被催着去跑{report}"
        );
    }
}

// ---- the per-turn execution boundary ------------------------------------
//
// `TurnExecutionPolicy`, which the production chain registers BEFORE every
// middleware that can `Allow` — because an `Allow` from an approval gate
// short-circuits everything downstream, and a boundary the user set must not be
// something a gate can wave through.
//
// It is a boundary the person states in plain language mid-conversation ("do
// not run any command"), so nothing in the tree configuration expresses it: it
// has to be re-read from the messages every round.

#[tokio::test]
async fn a_command_the_user_forbade_is_refused_on_both_engines() {
    let dir = scratch("exec-policy-onharness");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::call("c1", "bash", r#"{"command":"echo hi"}"#),
            Reply::Text("ran it"),
        ])
    };
    let asked = || say("check the build, and do not run any command");
    let a = reference_production(script(), &dir, asked()).await;
    let (handle, mut app) = on_harness_headless(script(), &dir).await;
    let b = drive_answering(handle, asked(), &[], None, allow()).await;
    app.stop();
    let report = judge("exec_policy_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            steps
                .iter()
                .any(|s| s.kind == "ToolResult" && s.detail.contains("error=true")),
            "{who}: 人这一回合禁掉的命令必须被拒 —— 审批闸门的 Allow 不该能放行它{report}"
        );
    }
}

#[tokio::test]
async fn a_refused_call_still_reads_as_started_on_the_harness() {
    // A general property of the two engines, given its own name so it is found
    // once rather than rediscovered as a bug in whichever row happens to refuse
    // something next. It cost exactly that: the `execution-policy` row's first
    // theory was that it had introduced this, and it had not.
    //
    //   链式  Request approval → ToolResult error=true
    //   行式  ToolStarted bash → Request approval → ToolResult error=true
    //
    // `ui-handle` synthesises `ToolStarted` for every MOUNTED tool the moment
    // the assistant message is logged — earlier than `tools/execute-batch`,
    // earlier than `tools/execute`, earlier than anything that could refuse.
    // Coding's chain announces a call only after its middleware has let it
    // through, so a refused call is never announced at all.
    //
    // Consequence for a driver: every refused call flashes as a tool that
    // started and instantly failed. `handle.rs` already guards the neighbouring
    // case — a tool nobody mounted is not announced, with a comment saying a
    // differential run found it — so the shape of the fix is known and it
    // belongs to `ui-handle`, not to any product row. Frozen at 1 until then.
    //
    // Two scenarios carry this divergence (`approval_refusal_rows` and
    // `exec_policy_rows`); this one states it.
    let dir = scratch("started-gap");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::call(
                "c1",
                "bash",
                r#"{"command":"rm -rf /tmp/nope-does-not-exist"}"#,
            ),
            Reply::Text("stopped"),
        ])
    };
    let a = reference_production_answering(script(), &dir, say("clean it"), deny()).await;
    let b = on_harness_answering(script(), &dir, say("clean it"), deny()).await;
    let report = judge("refused_call_started_rows", &a, &b);

    let started = |steps: &[Step]| steps.iter().filter(|s| s.kind == "ToolStarted").count();
    assert_eq!(started(&a), 0, "链式:被拒的调用不该被宣告开始过{report}");
    assert_eq!(
        started(&b),
        1,
        "行式:这正是本条记录的差异 —— 若它变成 0,说明 ui-handle 修好了,\
         把本条连同两处基线一起降下来{report}"
    );
}

// ---- the skill-first nudge ----------------------------------------------
//
// A whole class this rig could not see until now: an EPHEMERAL request tail.
// It is appended to the request and never logged, so no event carries it and no
// snapshot shows it — `transcript` above asks for a snapshot and would report
// "identical" for two engines that sent the model completely different things.
//
// The judge here is what the provider was handed. `Script::seen()`.

/// One skill, where both engines look for skills.
fn seed_skill(dir: &std::path::Path) {
    let skill = dir.join(".claude/skills/tidy-imports");
    std::fs::create_dir_all(&skill).expect("skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: tidy-imports\ndescription: Reorder and dedupe imports in a file\n---\n\n\
         Read the file, sort the imports, write it back.\n",
    )
    .expect("skill");
}

/// The production assembly, told it is a model that needs firm steering, with
/// skills actually loaded.
async fn reference_production_as(
    script: Arc<Script>,
    dir: &std::path::Path,
    model: &str,
    commands: Vec<AgentCommand>,
) -> Vec<Step> {
    let cfg = atomcode_coding::CodingAgentConfig::new("k", "http://unused.test/v1", model, dir);
    let opts = atomcode_coding::parts::PrepareOptions {
        mcp: false,
        web: false,
        review: false,
        memory: false,
        // NOT `Some(vec![])` here, unlike every other scenario: this one is
        // about the skill catalog, and an empty catalog makes the behaviour a
        // no-op by design on both engines.
        skill_dirs: None,
        ..Default::default()
    };
    let mut parts = atomcode_coding::parts::prepare(&cfg, opts)
        .await
        .expect("prepare");
    let agent = atomcode_coding::parts::assemble(&mut parts, &cfg, script).expect("assemble");
    drive_until(agent.spawn(), commands, &[]).await
}

#[tokio::test]
async fn a_weak_model_is_told_to_check_the_skills_first_on_both_engines() {
    // DeepSeek and Qwen under-weight the soft `## SKILLS:` guidance and open by
    // exploring instead of loading a matching process skill, so the chain
    // injects the directive at the request TAIL on the opening turn, where
    // recency is highest.
    //
    // Both sides claim to be `deepseek-chat` — the chain through `cfg.model`,
    // the rows through `LlmProvider::model_name()` — because the behaviour is
    // gated on the model and a fixture stuck calling itself "script" could
    // never reach it.
    let dir = scratch("skill-first-onharness");
    seed(&dir);
    seed_skill(&dir);

    let chain_script = Script::text(&["ok"]).as_model("deepseek-chat");
    let _ = reference_production_as(
        chain_script.clone(),
        &dir,
        "deepseek-chat",
        say("tidy the imports in a.rs"),
    )
    .await;

    let rows_script = Script::text(&["ok"]).as_model("deepseek-chat");
    let (handle, mut app) = on_harness_handle(rows_script.clone(), &dir).await;
    let _ = drive_answering(handle, say("tidy the imports in a.rs"), &[], None, allow()).await;
    app.stop();

    const DIRECTIVE: &str = "you MUST call `use_skill`";
    for (who, seen) in [("链式", chain_script.seen()), ("行式", rows_script.seen())] {
        assert!(
            seen.contains("tidy-imports"),
            "{who}: 连技能目录都没到模型面前,这条场景就没在测它该测的东西:\n{seen}"
        );
        assert!(
            seen.contains(DIRECTIVE),
            "{who}: 弱模型必须在动手前被要求先查技能目录\n{seen}"
        );
    }
}

#[tokio::test]
async fn a_strong_model_is_not_nudged_and_the_pointer_is_not_doubled() {
    // Two negative controls in one run, because both are about what must NOT be
    // in the prompt.
    //
    // 1. The nudge is for models that need firm steering. A strong model getting
    //    it too would mean the gate does nothing, and "both engines agree" would
    //    be true of a row that always fires.
    // 2. `skill-catalog-inline` contributes under the generic row's fragment id
    //    ON PURPOSE, so the catalog REPLACES the "call `list_skills` to see
    //    them" pointer instead of sitting beside it. If that ever stops working
    //    the model gets told about its skills twice, in two different shapes,
    //    and nothing else here would notice.
    let dir = scratch("skill-first-strong");
    seed(&dir);
    seed_skill(&dir);

    let chain_script = Script::text(&["ok"]).as_model("claude-opus-5");
    let _ = reference_production_as(
        chain_script.clone(),
        &dir,
        "claude-opus-5",
        say("tidy the imports in a.rs"),
    )
    .await;

    let rows_script = Script::text(&["ok"]).as_model("claude-opus-5");
    let (handle, mut app) = on_harness_handle(rows_script.clone(), &dir).await;
    let _ = drive_answering(handle, say("tidy the imports in a.rs"), &[], None, allow()).await;
    app.stop();

    for (who, seen) in [("链式", chain_script.seen()), ("行式", rows_script.seen())] {
        assert!(
            seen.contains("tidy-imports"),
            "{who}: 目录还是要在,只是不该被催{seen}"
        );
        assert!(
            !seen.contains("you MUST call `use_skill`"),
            "{who}: 强模型不该吃这一记催promt —— 那说明开关根本没起作用\n{seen}"
        );
    }
    assert!(
        !rows_script
            .seen()
            .contains("Call `list_skills` to see them"),
        "行式:目录该顶掉那句指针,而不是并排站着 —— 同一件事说两遍,\
         两种形状\n{}",
        rows_script.seen()
    );
}

// ---- the datalog --------------------------------------------------------
//
// Judged on the FILES, not the event stream: the datalog emits no events at
// all, and an engine that wrote nothing would look identical to one that wrote
// everything in every other scenario here.
//
// Pointed at a scratch directory on both sides. The real default is
// `~/.atomcode/datalog`, and a rig that writes to the developer's home is not a
// rig.

/// Everything the datalog wrote, markdown and jsonl, as (markdown, jsonl).
fn datalog_files(root: &std::path::Path) -> (String, String) {
    let mut markdown = String::new();
    let mut jsonl = String::new();
    fn walk(dir: &std::path::Path, markdown: &mut String, jsonl: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, markdown, jsonl);
            } else if path.extension().is_some_and(|e| e == "md") {
                markdown.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                jsonl.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    walk(root, &mut markdown, &mut jsonl);
    (markdown, jsonl)
}

#[tokio::test]
async fn the_datalog_records_the_same_turn_on_both_engines() {
    let dir = scratch("datalog-onharness");
    seed(&dir);
    let logs = dir.join("logs");
    let script = || {
        Script::new(&[
            Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
            Reply::Text("that is an empty main"),
        ])
    };

    // The chain: `datalog.enabled` is false by default, so the scenario turns it
    // on the way a user would.
    let mut cfg =
        atomcode_coding::CodingAgentConfig::new("k", "http://unused.test/v1", "script", &dir);
    cfg.datalog = atomcode_config::config::DatalogConfig {
        enabled: true,
        dir: Some(logs.join("chain").to_string_lossy().into_owned()),
    };
    let opts = atomcode_coding::parts::PrepareOptions {
        mcp: false,
        web: false,
        review: false,
        memory: false,
        skill_dirs: Some(Vec::new()),
        ..Default::default()
    };
    let mut parts = atomcode_coding::parts::prepare(&cfg, opts)
        .await
        .expect("prepare");
    let chain = atomcode_coding::parts::assemble(&mut parts, &cfg, script()).expect("assemble");
    let _ = drive_until(chain.spawn(), say("read a.rs"), &[]).await;

    // The rows: an extra layer inserting the row, because for this one mounting
    // IS enabling — it is deliberately absent from `CODING_ROWS`.
    let quiet = quiet_rows(&dir);
    let insert = format!(
        "[[insert]]\nname = \"datalog\"\nconfig = {{ working_dir = {dir:?}, dir = {log:?} }}\n",
        dir = dir.to_string_lossy(),
        log = logs.join("rows").to_string_lossy(),
    );
    let (handle, mut app) = atomcode_coding::on_harness::mount(
        &dir,
        atomcode_coding::on_harness::Presence::Attended,
        script(),
        &[quiet.as_str(), insert.as_str()],
    )
    .await
    .expect("the tree with a datalog must mount");
    let _ = drive_answering(handle, say("read a.rs"), &[], None, allow()).await;
    app.stop();
    // The turn-end flush is spawned on the rows side (`emit` dispatches sync
    // listeners, so there is nowhere to await), which means the last write can
    // still be in flight when the turn ends. Give it a moment rather than
    // asserting on a race.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    for (who, root) in [("链式", logs.join("chain")), ("行式", logs.join("rows"))] {
        let (markdown, jsonl) = datalog_files(&root);
        assert!(
            !markdown.is_empty() && !jsonl.is_empty(),
            "{who}: 什么都没写 —— 这条场景对空目录和对满目录一样绿,所以先验这个"
        );
        assert!(
            markdown.contains("read a.rs"),
            "{who}: 提问要在 transcript 里\n{markdown}"
        );
        assert!(
            markdown.contains("read_file"),
            "{who}: 工具调用要在\n{markdown}"
        );
        assert!(
            markdown.contains("that is an empty main"),
            "{who}: 回答也要在\n{markdown}"
        );
        assert!(
            markdown.contains("**Stats:**"),
            "{who}: 回合结束要落一行统计\n{markdown}"
        );
        // The JSONL is the half that exists nowhere else: the assembled request.
        // A transcript without it is a nicer-looking session log.
        let records: Vec<serde_json::Value> = jsonl
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("每行都得是 JSON"))
            .collect();
        assert_eq!(records.len(), 2, "{who}: 两轮模型调用,两条记录");
        for record in &records {
            assert!(
                record["messages"].as_array().is_some_and(|m| !m.is_empty()),
                "{who}: 记录里必须有发出去的那组消息 —— 这正是它唯一的存在理由\n{record}"
            );
            assert!(
                record["tools"].as_array().is_some_and(|t| !t.is_empty()),
                "{who}: 工具清单也要在\n{record}"
            );
        }
    }
}

// ---- the user's own hooks -----------------------------------------------
//
// The one place this rig knowingly spawns a subprocess. It has to: the whole
// feature IS running the person's external command, and a fixture that stubs
// the command out would test the fold and not the thing. The commands are
// `/bin/sh` one-liners writing to the scratch dir.

/// A hooks file where both engines look for one: `<project>/.hooks.json`.
///
/// NOT `.claude/settings.json` in CC's nested-array shape, which is what the
/// first version of this wrote — the scenario then passed with zero divergences
/// because NEITHER engine found the file, which is the failure mode the
/// "does it actually fire" assertion exists to catch.
fn seed_hooks(dir: &std::path::Path, body: &str) {
    std::fs::write(dir.join(".hooks.json"), body).expect("hooks file");
}

/// A PreToolUse hook that denies `read_file`.
fn deny_read_file() -> String {
    serde_json::json!({
        "hooks": {
            "no-reading": {
                "event": "PreToolUse",
                "matcher": "read_file",
                "command": "printf '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"nope, not that file\"}}'",
            },
        },
    })
    .to_string()
}

#[tokio::test]
async fn a_users_hook_can_refuse_a_tool_on_both_engines() {
    let dir = scratch("cc-hooks-onharness");
    seed(&dir);
    seed_hooks(&dir, &deny_read_file());
    let script = || {
        Script::new(&[
            Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
            Reply::Text("could not read it"),
        ])
    };

    let a = reference_production(script(), &dir, say("read a.rs")).await;

    let quiet = quiet_rows(&dir);
    let insert = format!(
        "[[insert]]\nname = \"cc-hooks\"\nconfig = {{ working_dir = {dir:?} }}\n",
        dir = dir.to_string_lossy(),
    );
    let (handle, mut app) = atomcode_coding::on_harness::mount(
        &dir,
        atomcode_coding::on_harness::Presence::Attended,
        script(),
        &[quiet.as_str(), insert.as_str()],
    )
    .await
    .expect("the tree with cc-hooks must mount");
    let b = drive_answering(handle, say("read a.rs"), &[], None, allow()).await;
    app.stop();
    // Frozen at 1, and it is the `ToolStarted` gap that
    // `a_refused_call_still_reads_as_started_on_the_harness` owns — the third
    // scenario to carry it, which is the point: it is ONE harness-side property
    // of every refusal path, not three separate bugs in three product rows.
    let report = judge("cc_hooks_deny_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            steps
                .iter()
                .any(|s| s.kind == "ToolResult" && s.detail.contains("error=true")),
            "{who}: 人自己配的钩子说不,就得是不{report}"
        );
    }
    // The file still exists and still says what it said. Cheap, and it is the
    // assertion that would have caught the first version writing the fixture to
    // a path nothing reads.
    assert!(
        std::fs::read_to_string(dir.join(".hooks.json"))
            .unwrap_or_default()
            .contains("PreToolUse"),
        "夹具必须落在两个引擎都会去读的那个路径上"
    );
}

#[tokio::test]
async fn without_a_hooks_file_nothing_is_mounted_and_nothing_changes() {
    // The negative control, and the one that makes the scenario above mean
    // something: a row that refused every `read_file` regardless would pass it
    // too. Same script, same tree, no `hooks.json` — the call must go through.
    let dir = scratch("cc-hooks-absent");
    seed(&dir);
    let script = || {
        Script::new(&[
            Reply::call("c1", "read_file", r#"{"file_path":"a.rs"}"#),
            Reply::Text("read it"),
        ])
    };

    let a = reference_production(script(), &dir, say("read a.rs")).await;

    let quiet = quiet_rows(&dir);
    let insert = format!(
        "[[insert]]\nname = \"cc-hooks\"\nconfig = {{ working_dir = {dir:?} }}\n",
        dir = dir.to_string_lossy(),
    );
    let (handle, mut app) = atomcode_coding::on_harness::mount(
        &dir,
        atomcode_coding::on_harness::Presence::Attended,
        script(),
        &[quiet.as_str(), insert.as_str()],
    )
    .await
    .expect("the tree must mount with an inert cc-hooks row");
    let b = drive_answering(handle, say("read a.rs"), &[], None, allow()).await;
    app.stop();
    let report = judge("cc_hooks_absent_rows", &a, &b);

    for (who, steps) in [("链式", &a), ("行式", &b)] {
        assert!(
            steps
                .iter()
                .any(|s| s.kind == "ToolResult" && s.detail.contains("error=false")),
            "{who}: 没配钩子就什么都不该变{report}"
        );
    }
}
