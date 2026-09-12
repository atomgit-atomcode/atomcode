//! A team: members that stay, report to the lead, and only to the lead.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atomcode_harness::agent::{Agent, AgentStatus};
use atomcode_harness::seams::{AgentsSvc, ToolsSvc};
use atomcode_harness::session::{InjectionOrigin, SessionEvent};
use atomcode_harness::{bundle, create_agent, drive, plugins, run_turn};
use atomcode_kernel::tool::{ProgressSink, ToolContext};
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-team-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The lead's script on `llm`, the members' on `llm-utility` — so each side's
/// answers are its own and a test can say exactly who said what.
fn tree(root: &std::path::Path, lead: &str, member: &str) -> ConfigTree {
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"approval\"\nconfig = {{ mode = \"yolo\" }}\n\n\
         [[insert]]\nname = \"team-in-process\"\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\nconfig = {{ script = [ {member} ] }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let lead = format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {lead} ] }}"
    );
    let layers = vec![
        bundle::base().unwrap(),
        Layer::from_toml(&lead).unwrap(),
        Layer::from_toml(quiet).unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ];
    ConfigTree::from_layers(layers).unwrap()
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

fn peers(agent: &Agent) -> Vec<(String, String)> {
    agent
        .session()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Injected {
                text,
                origin: InjectionOrigin::Peer { from },
                ..
            } => Some((from, text)),
            _ => None,
        })
        .collect()
}

/// Run the `team` tool as the lead, the way a turn would.
async fn as_lead(app: &App, lead: &Arc<Agent>, args: &str) -> atomcode_kernel::tool::ToolResult {
    let tool = app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .get("team")
        .expect("team tool mounted");
    let ctx = ToolContext {
        working_dir: std::env::current_dir().unwrap(),
        cancel: Default::default(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    let args = args.to_string();
    atomcode_harness::agent::as_agent(lead.ctx().clone(), async move {
        tool.execute(&args, &ctx).await
    })
    .await
}

/// What the lead has heard from its members. A report that arrived while the
/// lead's turn was still running was folded into it (steering) and is already
/// in the log; one that arrived after sits in the inbox until a turn claims it
/// — here, one driven on purpose, since nobody is driving the lead in a test.
async fn heard_by(app: &App, lead: &Arc<Agent>) -> Vec<(String, String)> {
    for _ in 0..100 {
        if lead.inbox().has_waking_input() || !peers(lead).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if lead.inbox().has_waking_input() {
        drive(app, lead).await.unwrap();
    }
    peers(lead)
}

async fn until_idle(agent: &Agent) {
    for _ in 0..300 {
        if agent.status() == AgentStatus::Idle
            && !agent.inbox().has_waking_input()
            && agent.session().current_turn() > 0
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("member never went idle");
}

const DELEGATE: &str = r#"{ text = "delegating", calls = [ { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "find where sessions are created" } } ] }"#;

#[tokio::test]
async fn a_member_reports_to_the_lead_and_the_lead_hears_it_between_turns() {
    let dir = scratch("report");
    let app = start(tree(
        &dir,
        &format!(r#"{DELEGATE}, {{ text = "delegated; waiting" }}, {{ text = "thanks, noted" }}"#),
        r#"{ text = "looking", calls = [ { name = "tell_parent", args = { text = "sessions are created in agent.rs" } } ] }, { text = "that is all" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    let first = run_turn(&app, "find out where sessions are created").await.unwrap();
    assert_eq!(first.text, "delegated; waiting");

    // The member ran on its own driver, on the utility model, and spoke.
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout = agents
        .by_session(&format!("{}/scout", lead.session_id()))
        .expect("the member exists under the lead's name");
    until_idle(&scout).await;
    let scout_said: Vec<String> = scout
        .session()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::AssistantMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(scout_said, vec!["looking", "that is all"], "the member's own log");
    assert_eq!(
        peers(&scout),
        vec![(lead.session_id().to_string(), "find where sessions are created".to_string())],
        "the task arrived as a message from the lead"
    );

    // Its report reached the lead as a message: folded into the running turn
    // if it was quick, waiting in the inbox — and waking a driven lead —
    // otherwise. Either way it is a fact in the lead's log with the sender named.
    let heard = heard_by(&app, &lead).await;
    assert_eq!(heard.len(), 1, "one report, not one per member turn: {heard:?}");
    assert_eq!(heard[0].0, scout.session_id());
    assert!(heard[0].1.contains("[scout] sessions are created in agent.rs"), "{heard:?}");

    // The lead's model saw it as something said to it, with the sender named.
    let shown = lead
        .session()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(shown.contains("[message from"), "{shown}");
}

#[tokio::test]
async fn a_member_that_finishes_silently_is_reported_on() {
    let dir = scratch("silent");
    let app = start(tree(
        &dir,
        &format!(r#"{DELEGATE}, {{ text = "delegated" }}"#),
        r#"{ text = "I looked and found nothing worth saying" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    run_turn(&app, "go").await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout = agents
        .by_session(&format!("{}/scout", lead.session_id()))
        .unwrap();
    until_idle(&scout).await;
    // The team spoke for it, in the member's name.
    let heard = heard_by(&app, &lead).await;
    assert_eq!(heard.len(), 1, "{heard:?}");
    assert_eq!(heard[0].0, scout.session_id());
    assert!(heard[0].1.starts_with("[scout finished turn 1"), "{}", heard[0].1);
    assert!(heard[0].1.contains("found nothing worth saying"), "{}", heard[0].1);
}

#[tokio::test]
async fn status_wait_tell_and_stop_are_the_leads_to_call() {
    let dir = scratch("lifecycle");
    let app = start(tree(
        &dir,
        &format!(r#"{DELEGATE}, {{ text = "delegated" }}"#),
        r#"{ text = "first answer" }, { text = "second answer" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    run_turn(&app, "go").await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout_session = format!("{}/scout", lead.session_id());
    let scout = agents.by_session(&scout_session).unwrap();

    let waited = as_lead(&app, &lead, r#"{"action":"wait","name":"scout"}"#).await;
    assert!(!waited.is_error, "{}", waited.content);
    assert_eq!(waited.content, "first answer");

    let status = as_lead(&app, &lead, r#"{"action":"status"}"#).await;
    assert!(status.content.starts_with("scout (explorer): Idle"), "{}", status.content);

    let told = as_lead(&app, &lead, r#"{"action":"tell","name":"scout","text":"and the tests?"}"#).await;
    assert!(!told.is_error, "{}", told.content);
    until_idle(&scout).await;
    let waited = as_lead(&app, &lead, r#"{"action":"wait","name":"scout"}"#).await;
    assert_eq!(waited.content, "second answer");
    assert_eq!(peers(&scout).len(), 2, "task, then the follow-up, both as the lead's words");

    let unknown = as_lead(&app, &lead, r#"{"action":"tell","name":"nobody","text":"hi"}"#).await;
    assert!(unknown.is_error);

    let stopped = as_lead(&app, &lead, r#"{"action":"stop","name":"scout"}"#).await;
    assert_eq!(stopped.content, "stopped: scout");
    assert!(agents.by_session(&scout_session).is_none(), "the member is gone");
    assert_eq!(as_lead(&app, &lead, r#"{"action":"status"}"#).await.content, "no members");
}

#[tokio::test]
async fn a_member_can_only_address_the_lead() {
    let dir = scratch("only-lead");
    let app = start(tree(
        &dir,
        &format!(r#"{DELEGATE}, {{ text = "delegated" }}"#),
        r#"{ text = "done" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    run_turn(&app, "go").await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout = agents
        .by_session(&format!("{}/scout", lead.session_id()))
        .unwrap();
    let names = scout.ctx().service::<ToolsSvc>().unwrap().names();
    assert!(names.contains(&"tell_parent".to_string()), "{names:?}");
    assert!(!names.contains(&"team".to_string()), "a member cannot delegate: {names:?}");
    assert!(!names.contains(&"bash".to_string()), "never a shell: {names:?}");
    assert!(!names.contains(&"write_file".to_string()), "an explorer reads: {names:?}");
}
