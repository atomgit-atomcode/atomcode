//! A team: members that stay, report to the lead, and only to the lead.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atomcode_harness::agent::{Agent, AgentStatus};
use atomcode_harness::seams::{AgentsSvc, ToolsSvc};
use atomcode_harness::session::{InjectionOrigin, SessionEvent};
use atomcode_harness::{bundle, create_agent, drive, plugins, run_turn};
use atomcode_kernel::provider::ReasoningEffort;
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
    tree_with(root, lead, member, "")
}

/// `team` is the team row's config, as TOML inline-table fields.
fn tree_with(root: &std::path::Path, lead: &str, member: &str, team: &str) -> ConfigTree {
    ConfigTree::from_layers(layers_with(root, lead, member, team)).unwrap()
}

/// [`tree_with`], with sessions kept under `sessions` and the lead's session
/// named `id` — resumed from there when `resume`.
fn kept(
    root: &std::path::Path,
    sessions: &std::path::Path,
    (id, resume): (&str, bool),
    lead: &str,
    member: &str,
    team: &str,
) -> ConfigTree {
    let mut layers = layers_with(root, lead, member, team);
    layers.push(
        Layer::from_toml(&format!(
            "[[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = false\nconfig = {{ root = {sessions:?}, project_root = {root:?} }}\n\n\
             [[patch]]\nid = \"session\"\nconfig = {{ id = {id:?}, resume = {resume} }}\n",
            sessions = sessions.to_string_lossy(),
            root = root.to_string_lossy(),
        ))
        .unwrap(),
    );
    ConfigTree::from_layers(layers).unwrap()
}

fn layers_with(root: &std::path::Path, lead: &str, member: &str, team: &str) -> Vec<Layer> {
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
         [[insert]]\nname = \"team-in-process\"\nconfig = {{ project_root = {root:?}, home = {home:?}{team} }}\n\n\
         [[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\nconfig = {{ script = [ {member} ] }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy(),
        team = if team.is_empty() { String::new() } else { format!(", {team}") },
    );
    let lead = format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {lead} ] }}"
    );
    vec![
        bundle::base().unwrap(),
        Layer::from_toml(&lead).unwrap(),
        Layer::from_toml(quiet).unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ]
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

/// The member's last words, from its own log — what a `wait` used to hand back.
fn last_said(agent: &Agent) -> Option<String> {
    agent
        .session()
        .events()
        .into_iter()
        .rev()
        .filter_map(|e| match e.event {
            SessionEvent::AssistantMessage { text, .. } if !text.trim().is_empty() => Some(text),
            _ => None,
        })
        .next()
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
    let first = run_turn(&app, "find out where sessions are created")
        .await
        .unwrap();
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
    assert_eq!(
        scout_said,
        vec!["looking", "that is all"],
        "the member's own log"
    );
    assert_eq!(
        peers(&scout),
        vec![(
            lead.session_id().to_string(),
            "find where sessions are created".to_string()
        )],
        "the task arrived as a message from the lead"
    );

    // Its report reached the lead as a message: folded into the running turn
    // if it was quick, waiting in the inbox — and waking a driven lead —
    // otherwise. Either way it is a fact in the lead's log with the sender named.
    let heard = heard_by(&app, &lead).await;
    assert_eq!(
        heard.len(),
        1,
        "one report, not one per member turn: {heard:?}"
    );
    assert_eq!(heard[0].0, scout.session_id());
    assert!(
        heard[0]
            .1
            .contains("[scout] sessions are created in agent.rs"),
        "{heard:?}"
    );

    // The lead's model saw it as something said to it, with the sender named.
    let shown = lead
        .session()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        shown.contains("another agent's report, not the user"),
        "the model is told whose words these are: {shown}"
    );
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
    assert!(
        heard[0].1.starts_with("[scout finished turn 1"),
        "{}",
        heard[0].1
    );
    assert!(
        heard[0].1.contains("found nothing worth saying"),
        "{}",
        heard[0].1
    );
}

#[tokio::test]
async fn status_tell_and_stop_are_the_leads_to_call() {
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

    // There is no `wait`: the member's answer is read from its own log, and
    // what reaches the lead does so as a message rather than a return value.
    until_idle(&scout).await;
    assert_eq!(last_said(&scout).as_deref(), Some("first answer"));

    let status = as_lead(&app, &lead, r#"{"action":"status"}"#).await;
    assert!(
        status.content.starts_with("scout (explorer): Idle"),
        "{}",
        status.content
    );

    let told = as_lead(
        &app,
        &lead,
        r#"{"action":"tell","name":"scout","text":"and the tests?"}"#,
    )
    .await;
    assert!(!told.is_error, "{}", told.content);
    until_idle(&scout).await;
    assert_eq!(last_said(&scout).as_deref(), Some("second answer"));
    assert_eq!(
        peers(&scout).len(),
        2,
        "task, then the follow-up, both as the lead's words"
    );

    let unknown = as_lead(
        &app,
        &lead,
        r#"{"action":"tell","name":"nobody","text":"hi"}"#,
    )
    .await;
    assert!(unknown.is_error);
    assert!(
        unknown.content.contains("live members: scout (explorer)"),
        "a refusal says who does exist: {}",
        unknown.content
    );

    let stopped = as_lead(&app, &lead, r#"{"action":"stop","name":"scout"}"#).await;
    assert_eq!(stopped.content, "stopped: scout");
    assert!(
        agents.by_session(&scout_session).is_none(),
        "the member is gone"
    );
    assert_eq!(
        as_lead(&app, &lead, r#"{"action":"status"}"#).await.content,
        "no members"
    );
    let gone = as_lead(
        &app,
        &lead,
        r#"{"action":"tell","name":"scout","text":"hi"}"#,
    )
    .await;
    assert!(
        gone.content.contains("live members: none"),
        "{}",
        gone.content
    );
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
    assert!(
        !names.contains(&"team".to_string()),
        "a member cannot delegate: {names:?}"
    );
    assert!(
        !names.contains(&"bash".to_string()),
        "never a shell: {names:?}"
    );
    assert!(
        !names.contains(&"write_file".to_string()),
        "an explorer reads: {names:?}"
    );
}

// ---- roles are data ------------------------------------------------------

fn write_role(root: &std::path::Path, id: &str, body: &str) {
    let dir = root.join(".atomcode").join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.md")), body).unwrap();
}

#[tokio::test]
async fn a_role_comes_from_a_markdown_file_and_can_replace_a_built_in() {
    let dir = scratch("roles");
    write_role(
        &dir,
        "librarian",
        "---\npermission: explore\ndifficulty: simple\nwhen: cataloguing what exists\n\
         tools: read_file, glob\n---\nYou catalogue. Report lists, not prose.\n",
    );
    write_role(
        &dir,
        "explorer",
        "---\npermission: explore\ndifficulty: simple\n---\nYou are the house explorer, rewritten.\n",
    );
    let app = start(tree(
        &dir,
        r#"{ text = "delegating", calls = [ { name = "team", args = { action = "delegate", name = "lib", role = "librarian", task = "list the docs" } } ] }, { text = "delegated" }"#,
        r#"{ text = "catalogued" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    run_turn(&app, "go").await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let lib = agents
        .by_session(&format!("{}/lib", lead.session_id()))
        .expect("a role from a file is a role");
    let names = lib.ctx().service::<ToolsSvc>().unwrap().names();
    assert!(names.contains(&"read_file".to_string()) && names.contains(&"glob".to_string()));
    assert!(
        !names.contains(&"grep".to_string()),
        "an explicit tool list replaces the default: {names:?}"
    );
    let prompt = lib
        .ctx()
        .service::<atomcode_harness::seams::SystemPromptSvc>()
        .unwrap()
        .render();
    assert!(prompt.contains("You catalogue."), "{prompt}");

    // The built-in was replaced by the file with its name.
    let schema = app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .get("team")
        .unwrap()
        .parameters_schema();
    let roles = schema["properties"]["role"]["enum"].to_string();
    assert!(
        roles.contains("librarian") && roles.contains("explorer"),
        "{roles}"
    );
    let told = as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"x","role":"explorer","task":"t"}"#,
    )
    .await;
    assert!(!told.is_error, "{}", told.content);
    let x = agents
        .by_session(&format!("{}/x", lead.session_id()))
        .unwrap();
    let prompt = x
        .ctx()
        .service::<atomcode_harness::seams::SystemPromptSvc>()
        .unwrap()
        .render();
    assert!(prompt.contains("rewritten"), "{prompt}");
}

#[tokio::test]
async fn a_bad_role_file_refuses_to_mount() {
    let dir = scratch("bad-role");
    write_role(
        &dir,
        "broken",
        "---\npermission: root\ndifficulty: simple\n---\nnope\n",
    );
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, r#"{ text = "ok" }"#, r#"{ text = "ok" }"#),
    );
    let err = app
        .start()
        .await
        .expect_err("a role that names a permission that does not exist");
    assert!(
        format!("{err:?}").contains("permission must be explore or worker"),
        "{err:?}"
    );
}

// ---- a writing member gets a checkout of its own ---------------------------

fn sh(dir: &std::path::Path, cmd: &str) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{cmd}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[tokio::test]
async fn a_worker_edits_in_its_own_worktree_and_the_branch_outlives_it() {
    let dir = scratch("worktree");
    sh(
        &dir,
        "git init -q && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init",
    );
    // A worker on the utility model, so its script is its own.
    write_role(
        &dir,
        "scribe",
        "---\npermission: worker\ndifficulty: simple\nwhen: writing a note\n---\nYou write what you are told.\n",
    );
    let app = start(tree_with(
        &dir,
        r#"{ text = "delegating", calls = [ { name = "team", args = { action = "delegate", name = "scribe", role = "scribe", task = "write hi into note.txt" } } ] }, { text = "delegated" }"#,
        r#"{ text = "writing", calls = [ { name = "write_file", args = { file_path = "note.txt", content = "hi" } } ] }, { text = "written" }"#,
        "worktrees = true",
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    run_turn(&app, "go").await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scribe = agents
        .by_session(&format!("{}/scribe", lead.session_id()))
        .unwrap();
    until_idle(&scribe).await;

    let worktree = dir.join(".atomcode").join("worktrees").join("scribe");
    assert_eq!(
        scribe.cwd(),
        Some(&worktree),
        "the member's world is its checkout"
    );
    assert!(worktree.join("note.txt").exists(), "it wrote there");
    assert!(!dir.join("note.txt").exists(), "and not in the lead's tree");
    let status = as_lead(&app, &lead, r#"{"action":"status"}"#).await;
    assert!(
        status.content.contains("branch `team/scribe-"),
        "{}",
        status.content
    );

    let stopped = as_lead(&app, &lead, r#"{"action":"stop","name":"scribe"}"#).await;
    assert!(
        stopped.content.contains("branch `team/scribe-"),
        "{}",
        stopped.content
    );
    assert!(!worktree.exists(), "the checkout is gone");
    let branches = sh(&dir, "git branch --list 'team/*'");
    assert!(
        branches.contains("team/scribe-"),
        "the branch stays for the lead: {branches}"
    );
}

// ---- the role's thinking tier ---------------------------------------------

/// Records what each model request carried, so a test can say what tier a
/// member's turn actually ran at rather than what the config claimed.
struct EffortSpy(Arc<Mutex<Vec<Option<ReasoningEffort>>>>);

#[async_trait::async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for EffortSpy {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        self.0.lock().unwrap().push(req.options.reasoning_effort);
        next.run(req).await
    }
}

/// The role decides the tier, and that decision reaches the request. Registered
/// as a spy at the root (which a member's realm can see, since visibility is
/// one-way down), so a member's own realm listener runs first and the spy
/// reports the result.
#[tokio::test]
async fn a_members_tier_comes_from_its_role() {
    let dir = scratch("role-effort");
    write_role(
        &dir,
        "librarian",
        "---\npermission: explore\ndifficulty: simple\neffort: low\n---\nYou catalogue.\n",
    );
    write_role(
        &dir,
        "grader",
        "---\npermission: explore\ndifficulty: hard\neffort: max\n---\nYou grade.\n",
    );
    let app = start(tree(
        &dir,
        r#"{ text = "delegating", calls = [ { name = "team", args = { action = "delegate", name = "lib", role = "librarian", task = "list the docs" } } ] },
          { text = "again", calls = [ { name = "team", args = { action = "delegate", name = "gr", role = "grader", task = "grade it" } } ] },
          { text = "done" }"#,
        r#"{ text = "catalogued" }"#,
    ))
    .await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let _guard = app
        .context()
        .on_waterfall::<atomcode_harness::events::AgentRequest>(
            Arc::new(EffortSpy(seen.clone())),
            false,
        );

    run_turn(&app, "go").await.unwrap();

    let recorded = seen.lock().unwrap().clone();
    assert!(
        recorded.contains(&Some(ReasoningEffort::Low)),
        "the simple role asked for `low`: {recorded:?}"
    );
    assert!(
        recorded.contains(&Some(ReasoningEffort::Max)),
        "the hard role asked for `max`: {recorded:?}"
    );
}

/// Who a member is, and the tier it thinks at, are said by the row that made it
/// a member — the same tier its requests carry, and nothing for a role that
/// states none (`docs/adr/0022` §5).
#[tokio::test]
async fn a_member_is_described_by_the_row_that_made_it_one() {
    let dir = scratch("described");
    write_role(
        &dir,
        "librarian",
        "---\npermission: explore\ndifficulty: simple\neffort: low\n---\nYou catalogue.\n",
    );
    write_role(
        &dir,
        "plain",
        "---\npermission: explore\ndifficulty: simple\n---\nYou look.\n",
    );
    let app = start(tree(
        &dir,
        r#"{ text = "ok" }"#,
        r#"{ text = "catalogued" }, { text = "looked" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    for (name, role) in [("lib", "librarian"), ("pl", "plain")] {
        let told = as_lead(
            &app,
            &lead,
            &format!(r#"{{"action":"delegate","name":"{name}","role":"{role}","task":"t"}}"#),
        )
        .await;
        assert!(!told.is_error, "{}", told.content);
    }
    let described = |name: &str| {
        agents
            .by_session(&format!("{}/{name}", lead.session_id()))
            .expect("the member")
            .describe()
    };

    let lib = described("lib");
    assert_eq!(lib.parent.as_deref(), Some(lead.session_id()));
    assert_eq!(
        lib.member,
        Some(atomcode_kernel::agent::MemberIdentity {
            name: "lib".into(),
            role: "librarian".into(),
        })
    );
    assert_eq!(lib.reasoning_effort, Some(ReasoningEffort::Low));

    let plain = described("pl");
    assert_eq!(plain.member.map(|m| m.role).as_deref(), Some("plain"));
    assert_eq!(plain.reasoning_effort, None);

    let lead_described = lead.describe();
    assert_eq!(lead_described.member, None, "the lead is no one's member");
    assert_eq!(
        lead_described.reasoning_effort, None,
        "a member's tier is the member's alone"
    );
}

/// A role that states no effort must not have one invented for it — the
/// session's own row stays in charge, which is what makes `effort` optional in
/// a role file rather than something every project has to repeat.
#[tokio::test]
async fn a_role_without_an_effort_line_inherits_the_session_setting() {
    let dir = scratch("role-effort-inherit");
    write_role(
        &dir,
        "librarian",
        "---\npermission: explore\ndifficulty: simple\n---\nYou catalogue.\n",
    );
    let app = start(tree(
        &dir,
        r#"{ text = "delegating", calls = [ { name = "team", args = { action = "delegate", name = "lib", role = "librarian", task = "list the docs" } } ] }"#,
        r#"{ text = "catalogued" }"#,
    ))
    .await;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let _guard = app
        .context()
        .on_waterfall::<atomcode_harness::events::AgentRequest>(
            Arc::new(EffortSpy(seen.clone())),
            false,
        );

    run_turn(&app, "go").await.unwrap();
    assert!(
        seen.lock().unwrap().iter().all(|e| e.is_none()),
        "the role said nothing, so nothing may be set on its behalf: {:?}",
        seen.lock().unwrap()
    );
}

/// A misspelled tier must be an error, not a silent fallback to the default: a
/// role file that says `effort: hihg` and a member that quietly thinks at the
/// session's rate is the kind of bug nobody notices until the bill.
///
/// It fails at MOUNT, not at the first turn — the roles are read when the row
/// applies, so a bad file is caught before any work can be delegated under it.
#[tokio::test]
async fn a_typo_in_a_roles_effort_is_refused_at_mount() {
    let dir = scratch("role-effort-typo");
    write_role(
        &dir,
        "librarian",
        "---\npermission: explore\ndifficulty: simple\neffort: hihg\n---\nYou catalogue.\n",
    );
    let tree = tree(
        &dir,
        r#"{ text = "delegating", calls = [ { name = "team", args = { action = "delegate", name = "lib", role = "librarian", task = "list the docs" } } ] }"#,
        r#"{ text = "catalogued" }"#,
    );
    let mut app = App::new(plugins::catalog(), tree);
    let err = app
        .start()
        .await
        .expect_err("a bad role file must stop the mount");
    let rendered = err.to_string();
    assert!(
        rendered.contains("hihg") && rendered.contains("effort"),
        "the finding must name the file, the key and the value: {rendered}"
    );
}

/// A member is driven by a pump of its own, like the agent a front end holds
/// (`docs/adr/0023` §6): what reaches it through that pump runs a turn, and
/// stopping the member stops the pump.
#[tokio::test]
async fn a_member_is_driven_through_a_pump_of_its_own() {
    let dir = scratch("pumped");
    let app = start(tree(
        &dir,
        &format!(r#"{DELEGATE}, {{ text = "delegated" }}"#),
        r#"{ text = "first look" }, { text = "heard you" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    run_turn(&app, "delegate the look").await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout = agents
        .by_session(&format!("{}/scout", lead.session_id()))
        .expect("the member exists");
    until_idle(&scout).await;
    assert!(scout.is_driven(), "a pump drives the member");

    assert!(
        scout.command(atomcode_kernel::event::AgentCommand::SendMessage {
            text: "a word from the person".into(),
            images: Vec::new(),
        })
    );
    let ended = |agent: &Agent| {
        agent
            .session()
            .events()
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
            .count()
    };
    for _ in 0..300 {
        if ended(&scout) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(ended(&scout), 2, "the command ran a turn");
    assert!(scout.session().events().iter().any(|e| matches!(
        &e.event,
        SessionEvent::UserMessage { text, .. } if text == "a word from the person"
    )));

    as_lead(&app, &lead, r#"{"action":"stop","name":"scout"}"#).await;
    for _ in 0..300 {
        if !scout.is_driven() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!scout.is_driven(), "stopping the member stopped its pump");
}

// ---- a person's commands ------------------------------------------------------

/// A lead a person is connected to: created and driven by a pump, the way a
/// front end's own agent is.
async fn lead_on_a_pump(app: &App) -> (Arc<Agent>, atomcode_kernel::agent::AgentHandle) {
    use atomcode_harness::plugins::handle;
    let ctx = app.context();
    let driven = handle::spawn(
        &ctx,
        handle::wire(),
        Arc::new(handle::NoAnswers),
        atomcode_harness::agent::CreateAgent::root(&ctx),
    )
    .await
    .unwrap();
    (driven.agent, driven.handle)
}

/// A command for the agent behind `session`, with a receipt.
fn to(
    session: &str,
    id: &str,
    command: atomcode_kernel::event::AgentCommand,
) -> atomcode_kernel::event::AgentCommand {
    atomcode_kernel::event::AgentCommand::To {
        session: session.into(),
        command: Box::new(atomcode_kernel::event::AgentCommand::Tagged {
            id: id.into(),
            command: Box::new(command),
        }),
    }
}

async fn settled(agent: &Agent, turns: usize) {
    for _ in 0..500 {
        if turns_ended(agent) >= turns
            && agent.status() == AgentStatus::Idle
            && !agent.inbox().has_waking_input()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{} never settled after {turns} turn(s); it ended {}",
        agent.session_id(),
        turns_ended(agent)
    );
}

fn notes(agent: &Agent) -> Vec<(InjectionOrigin, String)> {
    agent
        .session()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Injected { text, origin, .. }
                if matches!(
                    origin,
                    InjectionOrigin::PersonToMember { .. } | InjectionOrigin::TeamNote { .. }
                ) =>
            {
                Some((origin, text))
            }
            _ => None,
        })
        .collect()
}

/// Events off a handle until `done` says so, or give up naming what came.
async fn events_until(
    handle: &mut atomcode_kernel::agent::AgentHandle,
    done: impl Fn(&atomcode_kernel::event::AgentEvent) -> bool,
) -> Vec<atomcode_kernel::event::AgentEvent> {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), handle.events.recv()).await {
            Ok(Some(event)) => {
                let last = done(&event);
                seen.push(event);
                if last {
                    return seen;
                }
            }
            _ => panic!("gave up waiting; saw {seen:#?}"),
        }
    }
}

/// A person stops a member from a front end, through the command catalog
/// (`docs/adr/0021` §10, `docs/adr/0023` §8). A member's description offers
/// `stop` and the lead's does not; invoking it on the member stops it the way
/// the lead's tool would and says so, and asking the lead for it is refused.
#[tokio::test]
async fn a_person_stops_a_member_from_the_catalog() {
    use atomcode_kernel::agent::CommandTarget;
    use atomcode_kernel::event::{AgentCommand, AgentEvent, CommandError};

    let dir = scratch("catalog-stop");
    let app = start(tree(&dir, r#"{ text = "ok" }"#, r#"{ text = "looked" }"#)).await;
    let (lead, mut handle) = lead_on_a_pump(&app).await;
    let told = as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    assert!(!told.is_error, "{}", told.content);
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout_session = format!("{}/scout", lead.session_id());
    let scout = agents.by_session(&scout_session).unwrap();
    until_idle(&scout).await;

    assert!(
        scout
            .describe()
            .commands
            .iter()
            .any(|c| c.name == "stop" && c.target == CommandTarget::Agent),
        "{:?}",
        scout.describe().commands
    );
    assert!(
        !lead.describe().commands.iter().any(|c| c.name == "stop"),
        "a lead is no one's member"
    );

    for (id, session) in [
        ("on-lead", lead.session_id()),
        ("on-scout", scout_session.as_str()),
    ] {
        handle
            .commands
            .send(AgentCommand::Invoke {
                id: id.into(),
                session: session.into(),
                name: "stop".into(),
                args: String::new(),
            })
            .unwrap();
    }
    let seen = events_until(
        &mut handle,
        |e| matches!(e, AgentEvent::Invoked { id, .. } if id == "on-scout"),
    )
    .await;
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Rejected { command, error: CommandError::NotFound } if command == "on-lead"
        )),
        "{seen:#?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::Accepted { command, .. } if command == "on-scout")),
        "{seen:#?}"
    );
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Invoked { id, output } if id == "on-scout" && output == "stopped: scout"
        )),
        "{seen:#?}"
    );
    assert!(
        agents.by_session(&scout_session).is_none(),
        "the member is gone"
    );
    assert!(
        scout
            .session()
            .events()
            .last()
            .is_some_and(|e| matches!(e.event, SessionEvent::Stopped { .. })),
        "and its log says it was stopped"
    );
}

/// A person talks to a member through the connection they have to the lead
/// (`docs/adr/0023` §4, §7): the member takes it as the person's word and runs
/// a turn; the lead is told — what was said, and what the member answered —
/// and is not woken for it.
#[tokio::test]
async fn a_person_talks_to_a_member_and_the_lead_is_told_without_being_woken() {
    use atomcode_kernel::event::{AgentCommand, AgentEvent};

    let dir = scratch("person-to-member");
    let app = start(tree(
        &dir,
        r#"{ text = "noted" }, { text = "should not run" }"#,
        r#"{ text = "found it" }, { text = "switched files" }"#,
    ))
    .await;
    let (lead, mut handle) = lead_on_a_pump(&app).await;
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    let scout_session = format!("{}/scout", lead.session_id());
    let scout = app
        .context()
        .service::<AgentsSvc>()
        .unwrap()
        .by_session(&scout_session)
        .unwrap();
    settled(&scout, 1).await;
    // The lead asked for that one, so it was woken to hear it.
    settled(&lead, 1).await;

    handle
        .commands
        .send(to(
            &scout_session,
            "to-scout",
            AgentCommand::SendMessage {
                text: "use the other file".into(),
                images: Vec::new(),
            },
        ))
        .unwrap();
    let seen = events_until(
        &mut handle,
        |e| matches!(e, AgentEvent::Accepted { command, .. } if command == "to-scout"),
    )
    .await;
    assert!(
        matches!(
            seen.last(),
            Some(AgentEvent::Accepted { turn: Some(2), .. })
        ),
        "the receipt names the member's turn: {seen:#?}"
    );
    settled(&scout, 2).await;
    assert!(scout.session().events().iter().any(|e| matches!(
        &e.event,
        SessionEvent::UserMessage { text, .. } if text == "use the other file"
    )));

    for _ in 0..300 {
        if notes(&lead).len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(turns_ended(&lead), 1, "the lead was not woken");
    assert!(!lead.inbox().has_waking_input());
    let told = notes(&lead);
    assert!(
        told.iter().any(|(origin, text)| matches!(
            origin,
            InjectionOrigin::PersonToMember { member } if member == "scout"
        ) && text == "use the other file"),
        "{told:#?}"
    );
    assert!(
        told.iter().any(|(origin, text)| matches!(
            origin,
            InjectionOrigin::TeamNote { member } if member == "scout"
        ) && text.contains("switched files")),
        "{told:#?}"
    );
    let shown = lead
        .session()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(shown.contains("not an instruction to you"), "{shown}");
}

/// A member turn that carries the lead's message reports as a message and wakes
/// the lead, whoever else spoke in it (`docs/adr/0023` §7).
#[tokio::test]
async fn a_turn_carrying_the_leads_message_still_wakes_the_lead() {
    use atomcode_harness::agent::MessageOrigin;

    let dir = scratch("lead-and-person");
    let app = start(tree(
        &dir,
        r#"{ text = "noted" }, { text = "noted again" }"#,
        r#"{ text = "found it" }, { text = "tests are fine" }, { text = "docs too" }"#,
    ))
    .await;
    let (lead, _handle) = lead_on_a_pump(&app).await;
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    let scout = app
        .context()
        .service::<AgentsSvc>()
        .unwrap()
        .by_session(&format!("{}/scout", lead.session_id()))
        .unwrap();
    settled(&scout, 1).await;
    settled(&lead, 1).await;

    // Both queued before the member's pump can run: one turn takes them both.
    scout.send_from("and the tests?", MessageOrigin::Peer(lead.id()));
    scout.send_from("and the docs", MessageOrigin::User);
    settled(&scout, 2).await;
    settled(&lead, 2).await;
    assert!(
        lead.session().events().iter().any(|e| matches!(
            &e.event,
            SessionEvent::Injected { text, origin: InjectionOrigin::Peer { .. }, .. }
                if text.starts_with("[scout finished turn 2")
        )),
        "reported as a message: {:#?}",
        notes(&lead)
    );
}

/// A person cancels a member's turn and compacts it through the lead's
/// connection; an address nobody answers to is refused (`docs/adr/0023` §8).
#[tokio::test]
async fn a_person_cancels_and_compacts_a_member() {
    use atomcode_kernel::event::{AgentCommand, AgentEvent, CommandError};

    let dir = scratch("cancel-member");
    let mut layers = layers_with(&dir, r#"{ text = "noted" }, { text = "noted" }"#, "", "");
    layers.push(
        Layer::from_toml("[[patch]]\nid = \"llm-utility\"\nname = \"test-stalling-utility\"\n")
            .unwrap(),
    );
    let mut registry = plugins::catalog();
    registry.register(Arc::new(StallingUtilityRow));
    let mut app = App::new(registry, ConfigTree::from_layers(layers).unwrap());
    app.start().await.expect("must mount");
    let (lead, mut handle) = lead_on_a_pump(&app).await;
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    let scout_session = format!("{}/scout", lead.session_id());
    let scout = app
        .context()
        .service::<AgentsSvc>()
        .unwrap()
        .by_session(&scout_session)
        .unwrap();
    for _ in 0..300 {
        if scout.status() == AgentStatus::Working {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Another conversation in the same tree is not this connection's to steer.
    let stranger = create_agent(&app).await.unwrap();
    handle
        .commands
        .send(to(stranger.session_id(), "stranger", AgentCommand::Cancel))
        .unwrap();
    handle
        .commands
        .send(to("nobody/here", "nobody", AgentCommand::Cancel))
        .unwrap();
    handle
        .commands
        .send(to(&scout_session, "cancel", AgentCommand::Cancel))
        .unwrap();
    let seen = events_until(
        &mut handle,
        |e| matches!(e, AgentEvent::Accepted { command, .. } if command == "cancel"),
    )
    .await;
    for refused in ["nobody", "stranger"] {
        assert!(
            seen.iter().any(|e| matches!(
                e,
                AgentEvent::Rejected { command, error: CommandError::NotFound } if command == refused
            )),
            "{refused}: {seen:#?}"
        );
    }
    settled(&scout, 1).await;
    assert!(scout.session().events().iter().any(|e| matches!(
        e.event,
        SessionEvent::TurnEnd {
            stop: atomcode_harness::seams::StopReason::Cancelled,
            ..
        }
    )));
    assert!(
        app.context()
            .service::<AgentsSvc>()
            .unwrap()
            .by_session(&scout_session)
            .is_some(),
        "a cancel is not a stop: the member stays on the team"
    );
    // Only the member's turn: the lead, woken by the report of a turn it had
    // asked for, runs its own to the end.
    settled(&lead, 1).await;
    assert!(
        !lead.session().events().iter().any(|e| matches!(
            e.event,
            SessionEvent::TurnEnd {
                stop: atomcode_harness::seams::StopReason::Cancelled,
                ..
            }
        )),
        "the lead's turn was not cancelled"
    );

    handle
        .commands
        .send(to(
            &scout_session,
            "compact",
            AgentCommand::Compact { focus: None },
        ))
        .unwrap();
    let seen = events_until(&mut handle, |e| matches!(e, AgentEvent::Compacted { .. })).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::Accepted { command, .. } if command == "compact")),
        "{seen:#?}"
    );
}

/// A person stopping a member tells the lead in proportion (`docs/adr/0023`
/// §8): a member with nothing of the lead's outstanding is a note; one still
/// working on what the lead asked wakes the lead, which is waiting on it.
#[tokio::test]
async fn a_person_stopping_a_member_wakes_the_lead_only_when_it_was_owed_a_report() {
    use atomcode_kernel::event::{AgentCommand, AgentEvent};

    // Nothing outstanding: it reported, and the lead heard.
    let dir = scratch("stop-reported");
    let app = start(tree(
        &dir,
        r#"{ text = "noted" }, { text = "should not run" }"#,
        r#"{ text = "found it" }"#,
    ))
    .await;
    let (lead, mut handle) = lead_on_a_pump(&app).await;
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    let scout_session = format!("{}/scout", lead.session_id());
    let scout = app
        .context()
        .service::<AgentsSvc>()
        .unwrap()
        .by_session(&scout_session)
        .unwrap();
    settled(&scout, 1).await;
    settled(&lead, 1).await;
    handle
        .commands
        .send(AgentCommand::Invoke {
            id: "stop".into(),
            session: scout_session.clone(),
            name: "stop".into(),
            args: String::new(),
        })
        .unwrap();
    events_until(&mut handle, |e| matches!(e, AgentEvent::Invoked { .. })).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(turns_ended(&lead), 1, "not woken");
    assert!(
        notes(&lead).iter().any(|(origin, text)| matches!(
            origin,
            InjectionOrigin::TeamNote { member } if member == "scout"
        ) && text.contains("stopped")),
        "{:#?}",
        notes(&lead)
    );

    // Still on the lead's task: the lead is woken to hear it will not come.
    let dir = scratch("stop-owed");
    let mut layers = layers_with(&dir, r#"{ text = "noted" }"#, "", "");
    layers.push(
        Layer::from_toml("[[patch]]\nid = \"llm-utility\"\nname = \"test-stalling-utility\"\n")
            .unwrap(),
    );
    let mut registry = plugins::catalog();
    registry.register(Arc::new(StallingUtilityRow));
    let mut app = App::new(registry, ConfigTree::from_layers(layers).unwrap());
    app.start().await.expect("must mount");
    let (lead, mut handle) = lead_on_a_pump(&app).await;
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    let scout_session = format!("{}/scout", lead.session_id());
    let scout = app
        .context()
        .service::<AgentsSvc>()
        .unwrap()
        .by_session(&scout_session)
        .unwrap();
    for _ in 0..300 {
        if scout.status() == AgentStatus::Working {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle
        .commands
        .send(AgentCommand::Invoke {
            id: "stop".into(),
            session: scout_session.clone(),
            name: "stop".into(),
            args: String::new(),
        })
        .unwrap();
    events_until(&mut handle, |e| matches!(e, AgentEvent::Invoked { .. })).await;
    settled(&lead, 1).await;
    assert!(
        lead.session().events().iter().any(|e| matches!(
            &e.event,
            SessionEvent::Injected { text, origin: InjectionOrigin::Peer { .. }, .. }
                if text.contains("stopped by the person before it reported back")
        )),
        "woken with a message: {:#?}",
        lead.session()
            .events()
            .iter()
            .map(|e| &e.event)
            .collect::<Vec<_>>()
    );
}

/// A lead turn the person interrupts and has undone takes back the members it
/// delegated; one delegated before it stays (`docs/adr/0023` §9).
#[tokio::test]
async fn an_undone_lead_turn_takes_back_the_members_it_delegated() {
    use atomcode_kernel::event::AgentCommand;

    let dir = scratch("undone-delegation");
    let mut layers = layers_with(
        &dir,
        r#"{ text = "noted" }, { text = "delegating", calls = [
             { name = "team", args = { action = "delegate", name = "fresh", role = "explorer", task = "look" } },
             { name = "bash", args = { command = "sleep 30" } }
           ] }, { text = "done" }"#,
        r#"{ text = "looked" }, { text = "looked" }"#,
        "",
    );
    layers.push(
        Layer::from_toml(&format!(
            "[[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {:?}, undo_cancelled = true }}\n",
            dir.to_string_lossy()
        ))
        .unwrap(),
    );
    let app = start(ConfigTree::from_layers(layers).unwrap()).await;
    let (lead, handle) = lead_on_a_pump(&app).await;
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"old","role":"explorer","task":"look"}"#,
    )
    .await;
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let old = agents
        .by_session(&format!("{}/old", lead.session_id()))
        .unwrap();
    settled(&old, 1).await;
    settled(&lead, 1).await;

    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: Vec::new(),
        })
        .unwrap();
    let fresh_session = format!("{}/fresh", lead.session_id());
    for _ in 0..500 {
        if agents.by_session(&fresh_session).is_some()
            && lead
                .session()
                .events()
                .iter()
                .any(|e| matches!(&e.event, SessionEvent::ToolStarted { .. }))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(agents.by_session(&fresh_session).is_some(), "delegated");
    handle.commands.send(AgentCommand::Cancel).unwrap();
    for _ in 0..500 {
        if agents.by_session(&fresh_session).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        agents.by_session(&fresh_session).is_none(),
        "the member of the undone turn is stopped"
    );
    assert!(
        agents
            .by_session(&format!("{}/old", lead.session_id()))
            .is_some(),
        "an earlier one is not"
    );
}

/// Cancelling the lead does not cancel its members: they work across its turns,
/// and a person stopping a wordy lead has not asked the team to stop
/// (`docs/adr/0023` §9).
#[tokio::test]
async fn cancelling_the_lead_leaves_its_members_working() {
    use atomcode_kernel::event::AgentCommand;

    let dir = scratch("lead-cancel");
    let mut layers = layers_with(
        &dir,
        r#"{ text = "delegating", calls = [
             { name = "team", args = { action = "delegate", name = "scout", role = "explorer", task = "look" } },
             { name = "bash", args = { command = "sleep 30" } }
           ] }, { text = "done" }"#,
        "",
        "",
    );
    layers.push(
        Layer::from_toml("[[patch]]\nid = \"llm-utility\"\nname = \"test-stalling-utility\"\n")
            .unwrap(),
    );
    let mut registry = plugins::catalog();
    registry.register(Arc::new(StallingUtilityRow));
    let mut app = App::new(registry, ConfigTree::from_layers(layers).unwrap());
    app.start().await.expect("must mount");
    let (lead, handle) = lead_on_a_pump(&app).await;
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: Vec::new(),
        })
        .unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout_session = format!("{}/scout", lead.session_id());
    for _ in 0..500 {
        let working = agents
            .by_session(&scout_session)
            .is_some_and(|scout| scout.status() == AgentStatus::Working);
        let sleeping = lead
            .session()
            .events()
            .iter()
            .any(|e| matches!(&e.event, SessionEvent::ToolStarted { .. }));
        if working && sleeping {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let scout = agents.by_session(&scout_session).expect("delegated");

    handle.commands.send(AgentCommand::Cancel).unwrap();
    settled(&lead, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(scout.status(), AgentStatus::Working, "the member works on");
    assert_eq!(turns_ended(&scout), 0, "its turn was not cut short");
}

/// A member's pump keeps nothing nobody reads: its events are not buffered for
/// the member's whole life.
#[tokio::test]
async fn a_driven_agents_unread_events_are_not_kept() {
    let dir = scratch("unread-events");
    let app = start(tree(&dir, r#"{ text = "hi" }"#, "")).await;
    let agent = create_agent(&app).await.unwrap();
    let mut driven = atomcode_harness::plugins::handle::drive(&app.context(), agent.clone());
    assert!(
        agent.command(atomcode_kernel::event::AgentCommand::SendMessage {
            text: "hello".into(),
            images: Vec::new(),
        })
    );
    settled(&agent, 1).await;
    assert!(matches!(
        driven.handle.events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

/// A member that is gone can still be read, whole, by its session id — from
/// the log its store kept (`docs/adr/0023` §5); a conversation that was never
/// delegated from anyone cannot be reached that way.
#[tokio::test]
async fn a_stopped_members_log_is_read_by_its_session_id() {
    use atomcode_kernel::event::{AgentCommand, AgentEvent, CommandError};

    let dir = scratch("read-stopped");
    let sessions = scratch("read-stopped-sessions");
    let app = start(kept(
        &dir,
        &sessions,
        ("read-stopped-lead", false),
        r#"{ text = "noted" }"#,
        r#"{ text = "found the thing" }"#,
        "",
    ))
    .await;
    let (lead, mut handle) = lead_on_a_pump(&app).await;
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    let scout_session = format!("{}/scout", lead.session_id());
    let scout = app
        .context()
        .service::<AgentsSvc>()
        .unwrap()
        .by_session(&scout_session)
        .unwrap();
    settled(&scout, 1).await;
    as_lead(&app, &lead, r#"{"action":"stop","name":"scout"}"#).await;
    stored_until(&app, &scout_session, "said it was stopped", |events| {
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::Stopped { .. }))
    })
    .await;

    // A conversation of its own, kept and no longer live.
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let stranger = agents
        .create(&app.context(), atomcode_harness::agent::CreateAgent::new())
        .await
        .unwrap();
    let stranger_session = stranger.session_id().to_string();
    agents.remove(stranger.id());
    drop(stranger);
    let store = app
        .context()
        .service::<atomcode_harness::seams::SessionPersistenceSvc>()
        .unwrap();
    assert!(
        store.header(&stranger_session).await.unwrap().is_some(),
        "kept, so only the rule refuses it"
    );
    handle
        .commands
        .send(AgentCommand::Tagged {
            id: "not-delegated".into(),
            command: Box::new(AgentCommand::Subscribe {
                session: stranger_session,
                from: 0,
            }),
        })
        .unwrap();
    handle
        .commands
        .send(AgentCommand::Subscribe {
            session: scout_session.clone(),
            from: 0,
        })
        .unwrap();
    let seen = events_until(&mut handle, |e| {
        matches!(e, AgentEvent::Fact(c) if c.session == scout_session
            && matches!(c.event, SessionEvent::Stopped { .. }))
    })
    .await;
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Rejected { command, error: CommandError::NotFound } if command == "not-delegated"
        )),
        "{seen:#?}"
    );
    assert!(
        seen.iter().any(|e| matches!(
            e,
            AgentEvent::Fact(c) if c.session == scout_session
                && matches!(&c.event, SessionEvent::AssistantMessage { text, .. } if text == "found the thing")
        )),
        "{seen:#?}"
    );
}

/// A row's commands are the row's: switching the team row off takes `stop`
/// out of the catalog with it (`docs/adr/0021` §10).
#[tokio::test]
async fn a_rows_commands_go_when_the_row_does() {
    let dir = scratch("catalog-unload");
    let mut app = start(tree(&dir, r#"{ text = "ok" }"#, r#"{ text = "looked" }"#)).await;
    let lead = create_agent(&app).await.unwrap();
    as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    let scout = app
        .context()
        .service::<AgentsSvc>()
        .unwrap()
        .by_session(&format!("{}/scout", lead.session_id()))
        .unwrap();
    let catalog = app
        .context()
        .service::<atomcode_harness::seams::CommandsSvc>()
        .expect("the commands row is in the base bundle");
    assert!(catalog.find("stop", &scout).is_some());

    app.patch(&Layer::from_toml("[[patch]]\nid = \"team-in-process\"\ndisabled = true").unwrap())
        .await
        .unwrap();
    assert!(
        catalog.find("stop", &scout).is_none(),
        "the team row is gone, and its command with it"
    );
}

// ---- a team across a restart ---------------------------------------------------

/// Wait until `id`'s stored log satisfies `done`.
async fn stored_until(
    app: &App,
    id: &str,
    what: &str,
    done: impl Fn(&[atomcode_harness::session::LoggedEvent]) -> bool,
) -> Vec<atomcode_harness::session::LoggedEvent> {
    let store = app
        .context()
        .service::<atomcode_harness::seams::SessionPersistenceSvc>()
        .expect("sessions are kept");
    for _ in 0..300 {
        let events = store.load(id).await.unwrap_or_default();
        if done(&events) {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("`{id}` never {what} on disk");
}

fn turns_ended(agent: &Agent) -> usize {
    agent
        .session()
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
        .count()
}

/// A team outlives a restart (`docs/adr/0024` §11, §13). Every member's log is
/// kept under the lead, and a stopped member's ends saying so; resuming the
/// lead brings back the others — the same sessions, idle, with their own
/// history and what they were delegated with — and a word to one of them runs
/// a turn. The stopped one stays stopped, its log still readable by id.
#[tokio::test]
async fn a_resumed_lead_brings_back_the_members_it_did_not_stop() {
    let dir = scratch("kept-team");
    let sessions = scratch("kept-team-sessions");
    write_role(
        &dir,
        "scribe",
        "---\npermission: worker\ndifficulty: simple\nwhen: writing a note\n---\nYou write what you are told.\n",
    );
    let lead_id = "kept-lead";
    let scribe_id = format!("{lead_id}/scribe");
    let scout_id = format!("{lead_id}/scout");
    {
        let app = start(kept(
            &dir,
            &sessions,
            (lead_id, false),
            r#"{ text = "ok" }"#,
            r#"{ text = "noted" }, { text = "looked" }"#,
            "",
        ))
        .await;
        let lead = create_agent(&app).await.unwrap();
        assert_eq!(lead.session_id(), lead_id);
        run_turn(&app, "put a team together").await.unwrap();
        for args in [
            r#"{"action":"delegate","name":"scribe","role":"scribe","task":"keep the notes","scope":["notes/**"],"effort":"high"}"#,
            r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
        ] {
            let told = as_lead(&app, &lead, args).await;
            assert!(!told.is_error, "{}", told.content);
        }
        let agents = app.context().service::<AgentsSvc>().unwrap();
        until_idle(&agents.by_session(&scribe_id).unwrap()).await;
        until_idle(&agents.by_session(&scout_id).unwrap()).await;
        let stopped = as_lead(&app, &lead, r#"{"action":"stop","name":"scout"}"#).await;
        assert_eq!(stopped.content, "stopped: scout");

        stored_until(&app, lead_id, "ended its turn", |events| {
            events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
        })
        .await;
        stored_until(&app, &scribe_id, "ended its turn", |events| {
            events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
        })
        .await;
        stored_until(&app, &scout_id, "said it was stopped", |events| {
            events
                .last()
                .is_some_and(|e| matches!(e.event, SessionEvent::Stopped { .. }))
        })
        .await;

        let store = app
            .context()
            .service::<atomcode_harness::seams::SessionPersistenceSvc>()
            .unwrap();
        let header = store.header(&scribe_id).await.unwrap().expect("a header");
        assert_eq!(header.parent.as_deref(), Some(lead_id));
        assert_eq!(
            header.member.as_ref().map(|m| m.scope.clone()),
            Some(vec!["notes/**".to_string()]),
            "what it was delegated with is in its header"
        );
        assert_eq!(
            store.list().await.unwrap(),
            vec![lead_id.to_string()],
            "a member is kept under its lead, not listed beside it"
        );
    }

    let app = start(kept(
        &dir,
        &sessions,
        (lead_id, true),
        r#"{ text = "back" }"#,
        r#"{ text = "heard you" }"#,
        "",
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    assert!(lead.seed_len() > 0, "the lead was resumed");
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let mut scribe = None;
    for _ in 0..300 {
        scribe = agents.by_session(&scribe_id);
        if scribe.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let scribe = scribe.expect("the member the lead did not stop came back");
    assert!(
        agents.by_session(&scout_id).is_none(),
        "the stopped one did not"
    );
    assert_eq!(scribe.status(), AgentStatus::Idle);
    assert_eq!(
        scribe.describe().reasoning_effort,
        Some(ReasoningEffort::High),
        "at the tier it was delegated at"
    );
    assert_eq!(turns_ended(&scribe), 1, "with its own history, and idle");
    assert_eq!(
        peers(&scribe),
        vec![(lead_id.to_string(), "keep the notes".to_string())],
        "the task it was given is its history, not sent again"
    );
    let status = as_lead(&app, &lead, r#"{"action":"status"}"#).await;
    assert!(
        status.content.starts_with("scribe (scribe): Idle")
            && status.content.contains("writes [notes/**]")
            && !status.content.contains("scout"),
        "{}",
        status.content
    );

    assert!(
        scribe.command(atomcode_kernel::event::AgentCommand::SendMessage {
            text: "a word after the restart".into(),
            images: Vec::new(),
        }),
        "it is driven again"
    );
    for _ in 0..300 {
        if turns_ended(&scribe) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(turns_ended(&scribe), 2, "the word ran a turn");
    assert_eq!(last_said(&scribe).as_deref(), Some("heard you"));

    let store = app
        .context()
        .service::<atomcode_harness::seams::SessionPersistenceSvc>()
        .unwrap();
    let scout_log = store.load(&scout_id).await.unwrap();
    assert!(
        scout_log
            .last()
            .is_some_and(|e| matches!(e.event, SessionEvent::Stopped { .. })),
        "the stopped member's log is still there to read"
    );
    let again = as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look again"}"#,
    )
    .await;
    assert!(
        again.is_error && again.content.contains("was stopped"),
        "and no new member writes after it: {}",
        again.content
    );
}

/// A model that never answers, so a member is still busy when it is stopped.
struct Stalling;

#[async_trait::async_trait]
impl atomcode_kernel::provider::LlmProvider for Stalling {
    fn model_name(&self) -> &str {
        "stalling"
    }
    async fn chat_stream(
        &self,
        _messages: &[atomcode_kernel::message::Message],
        _tools: &[atomcode_kernel::tool::ToolDef],
        _options: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        Ok(Box::pin(futures::stream::pending()))
    }
}

struct StallingUtilityRow;

#[async_trait::async_trait]
impl atomcode_plexus::Plugin for StallingUtilityRow {
    fn name(&self) -> &'static str {
        "test-stalling-utility"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm-utility"]
    }
    fn description(&self) -> &'static str {
        "a side-call model that never answers"
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _config: &serde_json::Value,
    ) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmUtilitySvc>(Arc::new(Stalling))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// A member stopped in the middle of a turn is stopped after that turn has
/// unwound: its log ends with the turn closed and then the fact that it was
/// stopped, never the other way round (`docs/adr/0024` §13).
#[tokio::test]
async fn a_member_stopped_mid_turn_says_so_last() {
    let dir = scratch("stopped-busy");
    let sessions = scratch("stopped-busy-sessions");
    let lead_id = "stopped-busy-lead";
    let scout_id = format!("{lead_id}/scout");
    let mut layers = layers_with(&dir, r#"{ text = "ok" }"#, "", "");
    layers.push(
        Layer::from_toml(&format!(
            "[[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = false\nconfig = {{ root = {sessions:?}, project_root = {root:?} }}\n\n\
             [[patch]]\nid = \"session\"\nconfig = {{ id = {lead_id:?}, resume = false }}\n\n\
             [[patch]]\nid = \"llm-utility\"\nname = \"test-stalling-utility\"\n",
            sessions = sessions.to_string_lossy(),
            root = dir.to_string_lossy(),
        ))
        .unwrap(),
    );
    let mut registry = plugins::catalog();
    registry.register(Arc::new(StallingUtilityRow));
    let mut app = App::new(registry, ConfigTree::from_layers(layers).unwrap());
    app.start().await.expect("must mount");
    let lead = create_agent(&app).await.unwrap();
    let told = as_lead(
        &app,
        &lead,
        r#"{"action":"delegate","name":"scout","role":"explorer","task":"look around"}"#,
    )
    .await;
    assert!(!told.is_error, "{}", told.content);
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scout = agents.by_session(&scout_id).unwrap();
    for _ in 0..300 {
        if scout.status() == AgentStatus::Working {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(scout.status(), AgentStatus::Working, "busy when stopped");

    let stopped = as_lead(&app, &lead, r#"{"action":"stop","name":"scout"}"#).await;
    assert_eq!(stopped.content, "stopped: scout");
    let on_disk = stored_until(
        &app,
        &scout_id,
        "closed its turn and said it was stopped",
        |events| {
            events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
                && events
                    .iter()
                    .any(|e| matches!(e.event, SessionEvent::Stopped { .. }))
        },
    )
    .await;
    assert!(
        on_disk
            .last()
            .is_some_and(|e| matches!(e.event, SessionEvent::Stopped { .. })),
        "{:#?}",
        on_disk.iter().map(|e| &e.event).collect::<Vec<_>>()
    );
}

/// A member that worked in a checkout of its own comes back to it, with the
/// branch it was on (`docs/adr/0024` §13).
#[tokio::test]
async fn a_resumed_member_works_in_its_checkout_again() {
    let dir = scratch("kept-worktree");
    let sessions = scratch("kept-worktree-sessions");
    sh(
        &dir,
        "git init -q && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init",
    );
    write_role(
        &dir,
        "scribe",
        "---\npermission: worker\ndifficulty: simple\nwhen: writing a note\n---\nYou write what you are told.\n",
    );
    let lead_id = "kept-worktree-lead";
    let scribe_id = format!("{lead_id}/scribe");
    let worktree = dir.join(".atomcode").join("worktrees").join("scribe");
    let branch = {
        let app = start(kept(
            &dir,
            &sessions,
            (lead_id, false),
            r#"{ text = "ok" }"#,
            r#"{ text = "noted" }"#,
            "worktrees = true",
        ))
        .await;
        let lead = create_agent(&app).await.unwrap();
        run_turn(&app, "put a team together").await.unwrap();
        let told = as_lead(
            &app,
            &lead,
            r#"{"action":"delegate","name":"scribe","role":"scribe","task":"keep the notes"}"#,
        )
        .await;
        assert!(!told.is_error, "{}", told.content);
        let agents = app.context().service::<AgentsSvc>().unwrap();
        until_idle(&agents.by_session(&scribe_id).unwrap()).await;
        stored_until(&app, &scribe_id, "ended its turn", |events| {
            events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
        })
        .await;
        stored_until(&app, lead_id, "ended its turn", |events| {
            events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
        })
        .await;
        let status = as_lead(&app, &lead, r#"{"action":"status"}"#).await.content;
        status
            .split('`')
            .find(|part| part.starts_with("team/scribe-"))
            .unwrap_or_else(|| panic!("no branch in: {status}"))
            .to_string()
    };

    let app = start(kept(
        &dir,
        &sessions,
        (lead_id, true),
        r#"{ text = "back" }"#,
        r#"{ text = "heard you" }"#,
        "worktrees = true",
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let mut scribe = None;
    for _ in 0..300 {
        scribe = agents.by_session(&scribe_id);
        if scribe.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let scribe = scribe.expect("the member came back");
    assert_eq!(scribe.cwd(), Some(&worktree), "to its own checkout");
    let status = as_lead(&app, &lead, r#"{"action":"status"}"#).await.content;
    assert!(
        status.contains(&format!("branch `{branch}`")),
        "on the branch it was on: {status}"
    );
}

// ---- what a writing member may touch ------------------------------------------

/// A writer that shares the lead's workspace writes only inside the scope it
/// was given — never into `.git`, never outside the workspace, whatever the
/// scope says (`docs/adr/0023`, addendum).
#[tokio::test]
async fn a_writing_member_writes_only_inside_its_scope() {
    let dir = scratch("lane");
    write_role(
        &dir,
        "scribe",
        "---\npermission: worker\ndifficulty: simple\nwhen: writing a note\n---\nYou write what you are told.\n",
    );
    let app = start(tree(
        &dir,
        r#"{ text = "delegating", calls = [ { name = "team", args = { action = "delegate", name = "scribe", role = "scribe", task = "write the notes", scope = ["src/**", ".git/**"] } } ] }, { text = "delegated" }"#,
        r#"{ text = "writing", calls = [
            { name = "write_file", args = { file_path = "src/in.txt", content = "ok" } },
            { name = "write_file", args = { file_path = "docs/out.txt", content = "no" } },
            { name = "write_file", args = { file_path = ".git/hooks/pre-commit", content = "no" } },
            { name = "write_file", args = { file_path = "../escaped.txt", content = "no" } }
        ] }, { text = "written" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    run_turn(&app, "go").await.unwrap();
    let agents = app.context().service::<AgentsSvc>().unwrap();
    let scribe = agents
        .by_session(&format!("{}/scribe", lead.session_id()))
        .unwrap();
    until_idle(&scribe).await;

    assert!(dir.join("src/in.txt").exists(), "inside its scope");
    assert!(!dir.join("docs/out.txt").exists(), "outside its scope");
    assert!(
        !dir.join(".git/hooks/pre-commit").exists(),
        "into .git, though the scope named it"
    );
    assert!(
        !dir.parent().unwrap().join("escaped.txt").exists(),
        "outside the workspace"
    );
    // Refused by the member's bounds, not by whatever else happens to fence
    // this tree's filesystem: a product tree with an unfenced world has only them.
    let results: Vec<String> = scribe
        .session()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::ToolResultLogged { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert!(
        results
            .iter()
            .any(|r| r.contains("escaped.txt is outside the working directory")),
        "{results:#?}"
    );
}

/// Writers sharing a workspace say what they will write, and never the same
/// files.
#[tokio::test]
async fn writers_sharing_the_workspace_need_scopes_of_their_own() {
    let dir = scratch("scopes");
    let app = start(tree(
        &dir,
        r#"{ text = "ok" }"#,
        r#"{ text = "ok" }, { text = "ok" }, { text = "ok" }"#,
    ))
    .await;
    let lead = create_agent(&app).await.unwrap();
    let delegate = |name: &str, scope: Option<&[&str]>| {
        let mut args = serde_json::json!({
            "action": "delegate", "name": name, "role": "implementer", "task": "change it"
        });
        if let Some(scope) = scope {
            args["scope"] = serde_json::json!(scope);
        }
        args.to_string()
    };

    let unscoped = as_lead(&app, &lead, &delegate("a", None)).await;
    assert!(
        unscoped.is_error && unscoped.content.contains("`scope` is required"),
        "{}",
        unscoped.content
    );
    let first = as_lead(&app, &lead, &delegate("b", Some(&["src/**"]))).await;
    assert!(!first.is_error, "{}", first.content);
    let overlapping = as_lead(&app, &lead, &delegate("c", Some(&["src/auth/x.rs"]))).await;
    assert!(
        overlapping.is_error && overlapping.content.contains("overlaps what `b` may write"),
        "{}",
        overlapping.content
    );
    let disjoint = as_lead(&app, &lead, &delegate("d", Some(&["docs/**"]))).await;
    assert!(!disjoint.is_error, "{}", disjoint.content);
}

/// A role file chooses a member's tools from what its permission allows: a
/// file in a cloned repository cannot hand a member a shell.
#[tokio::test]
async fn a_role_file_cannot_hand_a_member_a_shell() {
    let dir = scratch("shell-role");
    write_role(
        &dir,
        "operator",
        "---\npermission: worker\ndifficulty: simple\ntools: read_file, bash\n---\nYou run things.\n",
    );
    let mut app = App::new(
        plugins::catalog(),
        tree(&dir, r#"{ text = "ok" }"#, r#"{ text = "ok" }"#),
    );
    let err = app.start().await.expect_err("a role listing `bash`");
    assert!(
        format!("{err:?}").contains("may not have `bash`"),
        "{err:?}"
    );
}

/// Only putting a writer to work is a decision to ask about: looking at the
/// team, telling or stopping a member, delegating a reader, and a `task`
/// whose children only read are not (`docs/adr/0023`, addendum).
#[tokio::test]
async fn only_putting_a_writer_to_work_is_risky() {
    use atomcode_kernel::tool::RiskLevel;
    let dir = scratch("risk");
    let mut layers = tree(&dir, r#"{ text = "ok" }"#, r#"{ text = "ok" }"#);
    layers
        .apply(
            &Layer::from_toml("[[patch]]\nid = \"subagent-in-process\"\ndisabled = false\n")
                .unwrap(),
        )
        .unwrap();
    let app = start(layers).await;
    let tools = app.context().service::<ToolsSvc>().unwrap();
    let team = tools.get("team").unwrap();
    for safe in [
        r#"{"action":"status"}"#,
        r#"{"action":"tell","name":"a","text":"more"}"#,
        r#"{"action":"stop","name":"a"}"#,
        r#"{"action":"delegate","name":"a","role":"explorer","task":"look"}"#,
        r#"{"action":"delegate","name":"a","role":"security","task":"look"}"#,
    ] {
        assert_eq!(team.risk(safe), RiskLevel::Safe, "{safe}");
    }
    assert_eq!(
        team.risk(r#"{"action":"delegate","name":"a","role":"implementer","task":"edit","scope":["src/**"]}"#),
        RiskLevel::Risky
    );
    assert_eq!(
        tools.get("task").unwrap().risk(r#"{"task":"look"}"#),
        RiskLevel::Safe,
        "a child that only reads"
    );
}

/// The roles the product always shipped are built in, so a switch to this row
/// does not take any away.
#[tokio::test]
async fn the_products_roles_are_built_in() {
    let dir = scratch("roles");
    let app = start(tree(&dir, r#"{ text = "ok" }"#, r#"{ text = "ok" }"#)).await;
    let schema = app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .get("team")
        .unwrap()
        .parameters_schema();
    let roles: Vec<String> =
        serde_json::from_value(schema["properties"]["role"]["enum"].clone()).unwrap();
    for role in [
        "planner",
        "architect",
        "explorer",
        "implementer",
        "rust",
        "tui_ux",
        "reviewer",
        "tester",
        "debugger",
        "security",
        "performance",
        "docs_writer",
        "release_manager",
        "migration_compat",
    ] {
        assert!(
            roles.iter().any(|r| r == role),
            "`{role}` missing: {roles:?}"
        );
    }
}
