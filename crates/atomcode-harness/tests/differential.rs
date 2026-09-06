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

/// A provider that replays a fixed script.
///
/// Shared by both engines *by value*, not re-implemented on each side: two
/// scripted providers that drifted would show up as an engine difference, and
/// the whole point is that they cannot.
struct Script {
    replies: Vec<String>,
    cursor: AtomicUsize,
}

impl Script {
    fn new(replies: &[&str]) -> Arc<Script> {
        Arc::new(Script {
            replies: replies.iter().map(|s| s.to_string()).collect(),
            cursor: AtomicUsize::new(0),
        })
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
        let text = self
            .replies
            .get(i)
            .cloned()
            .unwrap_or_else(|| "done".to_string());
        Ok(Box::pin(stream::iter(vec![
            StreamEvent::TextDelta(text),
            StreamEvent::Done { truncated: false },
        ])))
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
async fn drive_until(
    mut handle: AgentHandle,
    commands: Vec<AgentCommand>,
    also: &[&str],
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

async fn candidate_until(
    script: Arc<Script>,
    dir: &std::path::Path,
    commands: Vec<AgentCommand>,
    also: &[&str],
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
    let steps = drive_until(handle, commands, also).await;
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

fn render(a: &[Step], b: &[Step]) -> String {
    let mut out = String::from("\n  参考（coding）              候选（harness）\n");
    for i in 0..a.len().max(b.len()) {
        let left = a.get(i).map(|s| format!("{} {}", s.kind, s.detail));
        let right = b.get(i).map(|s| format!("{} {}", s.kind, s.detail));
        let same = left == right;
        out.push_str(&format!(
            "  {} {:<28} {}\n",
            if same { ' ' } else { '!' },
            left.unwrap_or_else(|| "—".into()),
            right.unwrap_or_else(|| "—".into())
        ));
    }
    out
}

fn divergences(a: &[Step], b: &[Step]) -> usize {
    (0..a.len().max(b.len()))
        .filter(|&i| a.get(i) != b.get(i))
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
    let a = reference(Script::new(&["hello"]), &dir, cmds()).await;
    let b = candidate(Script::new(&["hello"]), &dir, cmds()).await;
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
    let a = reference_until(Script::new(&["ok"]), &dir, cmds(), &["Snapshot"]).await;
    let b = candidate_until(Script::new(&["ok"]), &dir, cmds(), &["Snapshot"]).await;
    let report = render(&a, &b);
    ratchet("snapshot", divergences(&a, &b), &report);
}
