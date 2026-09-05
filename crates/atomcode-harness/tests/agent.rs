//! The agent as an entity: a registry, an inbox, a realm, and turns that are
//! not function calls.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::agent::{Agent, AgentStatus};
use atomcode_harness::events::{
    AgentRequest, ModelRequest, ModelResponse, PreStep, RequestError, StepDecision, ToolExec,
    ToolsExecute,
};
use atomcode_harness::seams::{AgentsSvc, SessionSvc, StopReason, ToolsSvc};
use atomcode_harness::session::{InjectionOrigin, SessionEvent};
use atomcode_harness::{bundle, create_agent, drive, plugins, run_turn};
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{App, ConfigTree, Layer, Next, Waterfall};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-agent-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn tree(root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut layers = vec![bundle::base().unwrap()];
    for src in [script, quiet, scoped.as_str()] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

/// A model that answers each step with plain text and no tools.
fn talker(steps: &[&str]) -> String {
    let list = steps
        .iter()
        .map(|t| format!("{{ text = \"{t}\" }}"))
        .collect::<Vec<_>>()
        .join(",\n  ");
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  {list}\n] }}\n"
    )
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

fn events(app: &App) -> Vec<SessionEvent> {
    app.context()
        .service::<SessionSvc>()
        .unwrap()
        .events()
        .into_iter()
        .map(|e| e.event)
        .collect()
}

// ---- the registry -------------------------------------------------------

#[tokio::test]
async fn agents_are_findable_without_being_handed_around() {
    let dir = scratch("registry");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let registry = app.context().service::<AgentsSvc>().unwrap();
    assert!(registry.is_empty());

    let a = create_agent(&app).unwrap();
    let b = create_agent(&app).unwrap();
    assert_eq!(registry.len(), 2);
    assert_eq!(registry.get(a.id()).unwrap().id(), a.id());
    assert_eq!(registry.list().len(), 2);

    registry.remove(b.id());
    assert_eq!(registry.len(), 1);
    assert!(registry.get(b.id()).is_none());
}

#[tokio::test]
async fn each_agent_gets_a_realm_of_its_own() {
    let dir = scratch("realms");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let a = create_agent(&app).unwrap();
    let b = create_agent(&app).unwrap();

    // A tool catalog for `a` alone.
    let just_for_a = Arc::new(atomcode_harness::seams::ToolBox::new());
    let _guard = a.ctx().provide::<ToolsSvc>(just_for_a).unwrap();

    assert!(
        a.ctx().service::<ToolsSvc>().unwrap().names().is_empty(),
        "the agent sees its own"
    );
    assert!(
        !b.ctx().service::<ToolsSvc>().unwrap().names().is_empty(),
        "its sibling does not"
    );
    assert!(
        !app.context()
            .service::<ToolsSvc>()
            .unwrap()
            .names()
            .is_empty(),
        "and neither does the root"
    );
}

// ---- the inbox ----------------------------------------------------------

#[tokio::test]
async fn an_injection_alone_never_opens_a_turn() {
    let dir = scratch("inject-only");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let agent = create_agent(&app).unwrap();

    agent.inject("some background context", InjectionOrigin::Reminder);
    let outcome = drive(&app, &agent).await.unwrap();

    assert_eq!(outcome.steps, 0, "context is not a request for work");
    assert_eq!(outcome.turn, 0, "and no turn was opened");
    assert!(
        events(&app).is_empty(),
        "an idle agent must not accumulate log entries from background notes"
    );
    assert_eq!(agent.inbox().len(), 1, "the context is still waiting");
}

#[tokio::test]
async fn an_injection_rides_in_with_the_next_message() {
    let dir = scratch("inject-rides");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let agent = create_agent(&app).unwrap();

    agent.inject("remember: the user prefers Rust", InjectionOrigin::Memory);
    agent.send("what should I use?");
    drive(&app, &agent).await.unwrap();

    let logged = events(&app);
    let injected_first = logged
        .iter()
        .position(|e| matches!(e, SessionEvent::Injected { .. }));
    let message_after = logged
        .iter()
        .position(|e| matches!(e, SessionEvent::UserMessage { .. }));
    assert!(injected_first.is_some() && message_after.is_some());
    assert!(
        injected_first < message_after,
        "context must precede the message it accompanies"
    );
    assert!(agent.inbox().is_empty());
}

#[tokio::test]
async fn one_message_per_step_so_the_model_sees_them_in_order() {
    let dir = scratch("order");
    let app = start(tree(&dir, &talker(&["first answer", "second answer"]), &[])).await;
    let agent = create_agent(&app).unwrap();

    agent.send("question one");
    agent.send("question two");
    let outcome = drive(&app, &agent).await.unwrap();

    assert_eq!(outcome.steps, 2, "two messages, two steps, one turn");
    assert_eq!(outcome.turn, 1);
    let users: Vec<String> = events(&app)
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::UserMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(users, vec!["question one", "question two"]);
}

// ---- turn and step ------------------------------------------------------

#[tokio::test]
async fn the_log_records_step_boundaries_inside_one_turn() {
    let dir = scratch("steps");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [
  { text = "Looking.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "Done." },
] }
"#;
    let yolo = "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }";
    let app = start(tree(&dir, script, &[yolo])).await;
    let outcome = run_turn(&app, "read it").await.unwrap();

    assert_eq!(outcome.steps, 2, "a tool call owes another request");
    let logged = events(&app);
    let turns = logged
        .iter()
        .filter(|e| matches!(e, SessionEvent::TurnStart { .. }))
        .count();
    let steps: Vec<u32> = logged
        .iter()
        .filter_map(|e| match e {
            SessionEvent::StepStart { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    assert_eq!(turns, 1, "one turn");
    assert_eq!(steps, vec![1, 2], "two steps inside it");
    let ends: Vec<u32> = logged
        .iter()
        .filter_map(|e| match e {
            SessionEvent::StepEnd { tool_calls, .. } => Some(*tool_calls),
            _ => None,
        })
        .collect();
    assert_eq!(
        ends,
        vec![1, 0],
        "the first step called a tool, the second did not"
    );
}

/// Puts a message in the agent's inbox while its first request is in flight.
struct SendsMidTurn {
    agent: Arc<Agent>,
    fired: AtomicBool,
}

#[async_trait]
impl Waterfall<AgentRequest> for SendsMidTurn {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        if !self.fired.swap(true, Ordering::SeqCst) {
            self.agent.send("actually, also do this");
        }
        next.run(req).await
    }
}

#[tokio::test]
async fn a_message_arriving_mid_turn_joins_the_turn_already_running() {
    let dir = scratch("steering");
    let app = start(tree(&dir, &talker(&["first", "second"]), &[])).await;
    let agent = create_agent(&app).unwrap();

    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(SendsMidTurn {
            agent: agent.clone(),
            fired: AtomicBool::new(false),
        }),
        false,
    );

    agent.send("do this");
    let outcome = drive(&app, &agent).await.unwrap();

    // This is the property a turn-as-function-call cannot express: the second
    // message did not queue behind a finished turn, it extended the open one.
    assert_eq!(outcome.turn, 1, "still one turn");
    assert_eq!(outcome.steps, 2, "with a second step for the new message");
    let turns = events(&app)
        .iter()
        .filter(|e| matches!(e, SessionEvent::TurnStart { .. }))
        .count();
    assert_eq!(turns, 1);
}

// ---- pre-step -----------------------------------------------------------

struct RewritesInput;

#[async_trait]
impl Waterfall<PreStep> for RewritesInput {
    async fn handle(&self, decision: &mut StepDecision, next: Next<'_, PreStep>) -> StepDecision {
        if let Some(message) = &decision.message {
            decision.message = Some(format!("{message} (rewritten)"));
        }
        next.run(decision).await
    }
}

struct RejectsInput;

#[async_trait]
impl Waterfall<PreStep> for RejectsInput {
    async fn handle(&self, decision: &mut StepDecision, next: Next<'_, PreStep>) -> StepDecision {
        decision.rejected = Some("not allowed to ask that".into());
        next.run(decision).await
    }
}

#[tokio::test]
async fn pre_step_decides_what_the_model_sees() {
    let dir = scratch("pre-step");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let _guard = app
        .context()
        .on_waterfall::<PreStep>(Arc::new(RewritesInput), false);

    run_turn(&app, "original text").await.unwrap();
    let users: Vec<String> = events(&app)
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::UserMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        vec!["original text (rewritten)"],
        "the log records what the model was actually given, not what was typed"
    );
}

#[tokio::test]
async fn a_rejected_first_claim_closes_a_turn_with_no_step() {
    let dir = scratch("reject");
    let app = start(tree(&dir, &talker(&["never reached"]), &[])).await;
    let _guard = app
        .context()
        .on_waterfall::<PreStep>(Arc::new(RejectsInput), false);

    let outcome = run_turn(&app, "please do something").await.unwrap();
    assert_eq!(outcome.stop, StopReason::InputRejected);
    assert_eq!(outcome.steps, 0);
    assert_eq!(outcome.error.as_deref(), Some("not allowed to ask that"));

    let logged = events(&app);
    assert!(
        logged
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnStart { .. })),
        "the attempt is still a fact about the session"
    );
    assert!(
        !logged
            .iter()
            .any(|e| matches!(e, SessionEvent::RequestHeader { .. })),
        "but nothing was sent"
    );
    assert!(
        !logged
            .iter()
            .any(|e| matches!(e, SessionEvent::UserMessage { .. })),
        "and the rejected input never entered the history"
    );
}

// ---- lifecycle ----------------------------------------------------------

/// Cancels the agent from inside its first tool call.
struct CancelsMidTurn {
    agent: Arc<Agent>,
}

#[async_trait]
impl Waterfall<ToolsExecute> for CancelsMidTurn {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        self.agent.cancel();
        next.run(exec).await
    }
}

#[tokio::test]
async fn an_agent_can_be_asked_to_stop() {
    let dir = scratch("cancel");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [
  { text = "Working.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
  { text = "More.", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] },
] }
"#;
    let yolo = "[[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }";
    let app = start(tree(&dir, script, &[yolo])).await;
    let agent = create_agent(&app).unwrap();
    let _guard = app.context().on_waterfall::<ToolsExecute>(
        Arc::new(CancelsMidTurn {
            agent: agent.clone(),
        }),
        false,
    );

    agent.send("go");
    let outcome = drive(&app, &agent).await.unwrap();

    assert_eq!(outcome.stop, StopReason::Cancelled);
    assert_eq!(
        outcome.steps, 1,
        "the step in flight finished, then it stopped"
    );
    assert_eq!(agent.status(), AgentStatus::Stopping);
}

#[tokio::test]
async fn status_tracks_the_turn() {
    let dir = scratch("status");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let agent = create_agent(&app).unwrap();
    assert_eq!(agent.status(), AgentStatus::Idle);

    agent.send("go");
    drive(&app, &agent).await.unwrap();
    assert_eq!(
        agent.status(),
        AgentStatus::Idle,
        "back to idle when nothing is owed"
    );
}

#[tokio::test]
async fn two_agents_share_a_process_without_sharing_a_turn() {
    let dir = scratch("two");
    let app = start(tree(&dir, &talker(&["for a", "for b"]), &[])).await;
    let a = create_agent(&app).unwrap();
    let b = create_agent(&app).unwrap();

    a.send("question from a");
    let first = drive(&app, &a).await.unwrap();
    b.send("question from b");
    let second = drive(&app, &b).await.unwrap();

    assert_eq!(first.text, "for a");
    assert_eq!(second.text, "for b");
    assert_ne!(a.id(), b.id());
}
