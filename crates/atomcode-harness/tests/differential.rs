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
}

impl Script {
    fn new(replies: &[Reply]) -> Arc<Script> {
        Arc::new(Script {
            replies: replies.to_vec(),
            cursor: AtomicUsize::new(0),
        })
    }
    fn text(replies: &[&'static str]) -> Arc<Script> {
        Script::new(&replies.iter().map(|t| Reply::Text(t)).collect::<Vec<_>>())
    }
}

#[async_trait]
impl LlmProvider for Script {
    fn model_name(&self) -> &str {
        "script"
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let i = self.cursor.fetch_add(1, Ordering::SeqCst);
        // Past the end of the script, stop cleanly rather than failing: a
        // fixture running out is not a provider outage, and one engine taking
        // an extra round must not read as a provider difference.
        let reply = self.replies.get(i).cloned().unwrap_or(Reply::Text("done"));
        let mut events: Vec<StreamEvent> = Vec::new();
        match reply {
            Reply::Slow(ms, t) => {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                events.push(StreamEvent::TextDelta(t.into()));
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
            step("Snapshot", format!("messages={}", snapshot.messages.len()))
        }
        AgentEvent::TurnComplete { reason } => step("TurnComplete", format!("{reason:?}")),
        AgentEvent::Error { message, .. } => step("Error", message.clone()),
        AgentEvent::Cancelled => step("Cancelled", String::new()),
        AgentEvent::Warning(_) => None,
        AgentEvent::StreamRecovery { .. } => None,
        AgentEvent::ProviderRetry { .. } => None,
        AgentEvent::OutputTruncationRecovery { .. } => None,
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
async fn drive_with(
    mut handle: AgentHandle,
    commands: Vec<AgentCommand>,
    also: &[&str],
    late: Option<AgentCommand>,
) -> Vec<Step> {
    for c in commands {
        if handle.commands.send(c).is_err() {
            break;
        }
    }
    let mut out = Vec::new();
    let mut done = false;
    // A bound, not a hope: an engine that never terminates must fail the test
    // rather than hang it. A hang reports as slowness and gets blamed on the
    // machine.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
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
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                out.push(Step {
                    kind: "TIMEOUT",
                    detail: "the engine never finished the turn".into(),
                });
                break;
            }
        }
    }
    let _ = handle.commands.send(AgentCommand::Shutdown);
    out
}

// ---- the two engines -----------------------------------------------------

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

    let handle = app
        .context()
        .service::<atomcode_harness::seams::AgentHandleSvc>()
        .expect("agent-handle row must provide a handle")
        .take()
        .expect("the handle, once");
    let steps = drive_with(handle, commands, also, late).await;
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
    // Tests in one binary run concurrently and this is a read-modify-write on a
    // shared file. Without the lock the two baselines raced and one was lost —
    // silently, because a missing entry just looks like "first run".
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../gates/differential.baseline");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let base: Option<usize> = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}="))?.trim().parse().ok());
    match base {
        None => {
            let mut all = text;
            all.push_str(&format!("{name}={count}\n"));
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(&path, all);
            eprintln!("⊙ {name}: 建立基线 {count} 处分歧{report}");
        }
        Some(base) if count > base => {
            panic!("{name}: 分歧从 {base} 涨到 {count}{report}");
        }
        Some(base) if count < base => {
            let kept: String = text
                .lines()
                .filter(|l| !l.starts_with(&format!("{name}=")))
                .map(|l| format!("{l}\n"))
                .collect();
            let _ = std::fs::write(&path, format!("{kept}{name}={count}\n"));
            eprintln!("✓ {name}: {count} 处分歧（基线从 {base} 降到 {count}）{report}");
        }
        Some(_) => eprintln!("✓ {name}: {count} 处分歧（持平）"),
    }
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
    let script = || Script::new(&[Reply::Slow(400, "working")]);
    let a = reference_cancelling(script(), &dir, cmds()).await;
    let b = candidate_cancelling(script(), &dir, cmds()).await;
    let report = render(&a, &b);
    ratchet("cancel", divergences(&a, &b), &report);

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
