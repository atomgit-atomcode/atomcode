//! The agent as an entity: a registry, an inbox, a realm, and turns that are
//! not function calls.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::agent::{Agent, AgentStatus, CreateAgent};
use atomcode_harness::events::{
    AgentRequest, ModelRequest, ModelResponse, PreStep, RequestError, StepDecision, ToolExec,
    ToolsExecute,
};
use atomcode_harness::seams::{AgentsSvc, FsSvc, SessionSvc, StopReason, ToolsSvc};
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
        .only_session()
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

    let a = create_agent(&app).await.unwrap();
    let b = create_agent(&app).await.unwrap();
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
    let a = create_agent(&app).await.unwrap();
    let b = create_agent(&app).await.unwrap();

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
    let agent = create_agent(&app).await.unwrap();

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
    let agent = create_agent(&app).await.unwrap();

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
    let agent = create_agent(&app).await.unwrap();

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
    let agent = create_agent(&app).await.unwrap();

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
    let agent = create_agent(&app).await.unwrap();
    let guard = app.context().on_waterfall::<ToolsExecute>(
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
    // The turn that was stopping has stopped, so the agent is idle again — and
    // ready. A cancel that outlived its turn would end the next one before its
    // first request.
    assert_eq!(agent.status(), AgentStatus::Idle);
    assert!(
        !agent.cancelled(),
        "the next turn must not inherit this one's cancellation"
    );

    // Nothing cancels this one, and nothing should have to: the point is that
    // the agent itself is not carrying the previous turn's stop.
    guard.dispose();
    agent.send("again");
    let after = drive(&app, &agent).await.unwrap();
    assert_eq!(
        after.stop,
        StopReason::Stopped,
        "an agent asked to stop once must still be able to work"
    );
}

#[tokio::test]
async fn status_tracks_the_turn() {
    let dir = scratch("status");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let agent = create_agent(&app).await.unwrap();
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
    let a = create_agent(&app).await.unwrap();
    let b = create_agent(&app).await.unwrap();

    a.send("question from a");
    let first = drive(&app, &a).await.unwrap();
    b.send("question from b");
    let second = drive(&app, &b).await.unwrap();

    assert_eq!(first.text, "for a");
    assert_eq!(second.text, "for b");
    assert_ne!(a.id(), b.id());
}

// ---- an agent owns its session and its world ----------------------------

#[tokio::test]
async fn two_agents_keep_their_own_sessions_and_worlds() {
    let dir = scratch("own-session");
    let elsewhere = scratch("own-world");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let agents = app.context().service::<AgentsSvc>().unwrap();

    let a = agents
        .create(&app.context(), CreateAgent::new().id("a"))
        .await
        .unwrap();
    let b = agents
        .create(
            &app.context(),
            CreateAgent::new().id("b").cwd(elsewhere.clone()),
        )
        .await
        .unwrap();

    // Two logs, not one shared: the tree itself holds none.
    assert_eq!(a.session_id(), "a");
    assert_eq!(b.session_id(), "b");
    assert!(
        !app.context().service_names().contains(&"sessions"),
        "the tree has no log of its own"
    );

    a.send("hello");
    drive(&app, &a).await.unwrap();
    assert!(!a.session().is_empty(), "a spoke");
    assert!(b.session().is_empty(), "b did not, and did not hear a");

    // Two worlds: `b` was given a root of its own, `a` has the tree's.
    let a_root = a.ctx().service::<FsSvc>().unwrap().root();
    let b_root = b.ctx().service::<FsSvc>().unwrap().root();
    assert_eq!(b_root, elsewhere);
    assert_ne!(a_root, b_root);

    // Removing an agent takes its world with it: the realm no longer resolves
    // a log, and the registry no longer knows the session.
    let b_ctx = b.ctx().clone();
    agents.remove(b.id());
    assert!(b_ctx.service::<SessionSvc>().is_none());
    assert!(agents.by_session("b").is_none());
    assert!(agents.by_session("a").is_some());
}

/// A policy registered once, at the top of the tree, that writes a fact into
/// "the" log — the shape of every compaction, recovery and truncation row.
struct WritesANotice {
    ctx: atomcode_plexus::Context,
}

#[async_trait]
impl Waterfall<AgentRequest> for WritesANotice {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        // Through the running agent, not through the plugin's own context.
        let scoped = atomcode_harness::agent::scoped(&self.ctx);
        if let Some(log) = scoped.service::<SessionSvc>() {
            atomcode_harness::session::commit(
                &scoped,
                &log,
                SessionEvent::Injected {
                    turn: log.current_turn(),
                    text: "a fact from a tree-level policy".into(),
                    origin: InjectionOrigin::Continuation,
                },
            );
        }
        next.run(req).await
    }
}

#[tokio::test]
async fn a_tree_level_policy_writes_into_the_log_of_the_agent_whose_turn_it_is() {
    // The bug this closes: a listener at the top of the tree resolved `sessions`
    // through its own context and always found the tree's one log, so a
    // delegated child's compaction cut the parent's history.
    let dir = scratch("scoped-log");
    let app = start(tree(&dir, &talker(&["ok", "ok"]), &[])).await;
    let root = app.context();
    let _policy =
        root.on_waterfall::<AgentRequest>(Arc::new(WritesANotice { ctx: root.clone() }), false);
    let a = create_agent(&app).await.unwrap();
    let b = create_agent(&app).await.unwrap();

    b.send("only b speaks");
    drive(&app, &b).await.unwrap();

    let injected = |agent: &Agent| {
        agent
            .session()
            .events()
            .into_iter()
            .filter(|e| matches!(e.event, SessionEvent::Injected { .. }))
            .count()
    };
    assert_eq!(injected(&b), 1, "the fact landed in b's log");
    assert_eq!(injected(&a), 0, "and not in a's");
    assert!(
        atomcode_harness::agent::current().is_none(),
        "outside a turn there is no running agent"
    );
}

#[tokio::test]
async fn the_session_row_names_the_front_ends_own_agent() {
    let dir = scratch("defaults");
    let app = start(tree(
        &dir,
        &talker(&["ok"]),
        &["[[patch]]\nid = \"session\"\nconfig = { id = \"named-by-the-row\" }"],
    ))
    .await;
    let own = create_agent(&app).await.unwrap();
    assert_eq!(own.session_id(), "named-by-the-row");
    // An agent created any other way names its own.
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let other = agents
        .create(&app.context(), CreateAgent::new())
        .await
        .unwrap();
    assert_ne!(other.session_id(), "named-by-the-row");
}

// ---- work that arrives without a command still runs ----------------------

async fn until_turns(agent: &Agent, want: usize) -> usize {
    let mut turns = 0;
    for _ in 0..200 {
        turns = agent
            .session()
            .events()
            .into_iter()
            .filter(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
            .count();
        if turns >= want {
            return turns;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    turns
}

#[tokio::test]
async fn a_message_wakes_an_idle_agent_nobody_is_driving() {
    let dir = scratch("wake");
    let app = start(tree(&dir, &talker(&["ok", "ok"]), &[])).await;
    let agent = create_agent(&app).await.unwrap();
    let driving = atomcode_harness::plugins::agent_loop::keep_driven(agent.clone()).unwrap();

    // Nobody calls `drive`. A peer, a timer, a goal controller would do
    // exactly this: put a message in the inbox and expect a turn.
    agent.send("hello from nowhere");
    assert_eq!(
        until_turns(&agent, 1).await,
        1,
        "the message alone started a turn"
    );

    // An injection alone does not: context waits for a prompt.
    agent.inject("some context", InjectionOrigin::Reminder);
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert_eq!(
        until_turns(&agent, 1).await,
        1,
        "no second turn for context alone"
    );

    agent.send("and again");
    assert_eq!(until_turns(&agent, 2).await, 2);
    drop(driving);

    // Listening stopped with the guard: a third message sits in the inbox.
    agent.send("after the driver left");
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert_eq!(until_turns(&agent, 2).await, 2);
    assert!(agent.inbox().has_waking_input(), "queued, not run");
}

#[tokio::test]
async fn a_message_wakes_the_agent_behind_a_handle_too() {
    use atomcode_harness::seams::AgentHandleSvc;
    use atomcode_kernel::event::AgentEvent;
    let dir = scratch("wake-handle");
    let layers = vec![
        bundle::base().unwrap(),
        Layer::from_toml(bundle::HANDLE_APP).unwrap(),
        Layer::from_toml(&talker(&["ok"])).unwrap(),
        Layer::from_toml(
            "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }",
        )
        .unwrap(),
        Layer::from_toml(&format!(
            "[[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
             [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 4, working_dir = {root:?} }}\n\n\
             [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
             [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {root:?} }}\n\n\
             [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
            root = dir.to_string_lossy()
        ))
        .unwrap(),
    ];
    let mut app = App::new(plugins::catalog(), ConfigTree::from_layers(layers).unwrap());
    app.start().await.unwrap();
    let mut handle = app
        .context()
        .service::<AgentHandleSvc>()
        .unwrap()
        .take()
        .unwrap();
    let agent = app.context().service::<AgentsSvc>().unwrap().list()[0].clone();

    // Straight into the inbox — not a command over the wire.
    agent.send("hello from a peer");
    let mut saw_turn = false;
    for _ in 0..200 {
        match tokio::time::timeout(std::time::Duration::from_millis(50), handle.events.recv()).await
        {
            Ok(Some(AgentEvent::TurnComplete { .. })) => {
                saw_turn = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => {}
        }
    }
    assert!(
        saw_turn,
        "the pump woke on the inbox and ran the turn; log has {} events, status {:?}, inbox waking {}",
        agent.session().len(),
        agent.status(),
        agent.inbox().has_waking_input()
    );
    let _ = handle
        .commands
        .send(atomcode_kernel::event::AgentCommand::Shutdown);
}
