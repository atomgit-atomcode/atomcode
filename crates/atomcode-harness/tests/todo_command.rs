//! `todo` — a person's hand on the plan (`plugins::policy_rows::TodoCommand`).
//!
//! An edit is a `todowrite` call and its result, committed between turns: the
//! same facts the model writes, so everything that reads the plan reads it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use atomcode_capabilities::tools::todo::{derive_current_todos, TodoStatus};
use atomcode_harness::agent::Agent;
use atomcode_harness::session::{derive_messages, RewindScope, SessionEvent};
use atomcode_harness::{bundle, plugins};
use atomcode_plexus::{App, ConfigTree, Layer};

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("todo-command-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn tree(dir: &Path) -> ConfigTree {
    let root = bundle::toml_string(&dir.to_string_lossy());
    let base = format!(
        "[[insert]]\nname = \"tool-fs-world\"\n\n\
         [[insert]]\nname = \"tool-todo\"\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [{{ text = \"ok\" }}] }}\n"
    );
    ConfigTree::from_layers([
        bundle::infra().expect("infra"),
        Layer::from_toml(&base).expect("rows"),
    ])
    .expect("tree")
}

async fn started(tag: &str) -> (App, Arc<Agent>) {
    let dir = scratch(tag);
    let mut app = App::new(plugins::catalog(), tree(&dir));
    app.start().await.expect("must mount");
    let agent = atomcode_harness::create_agent(&app)
        .await
        .expect("an agent to run commands against");
    (app, agent)
}

async fn todo(app: &App, agent: &Arc<Agent>, args: &str) -> Result<String, String> {
    app.context()
        .service::<atomcode_harness::seams::CommandsSvc>()
        .expect("the catalog is a core row")
        .find("todo", agent)
        .expect("`todo` is on offer where `todowrite` is")
        .run(agent.clone(), args)
        .await
}

/// What the model would be told the plan is, from the log alone.
fn plan(agent: &Agent) -> Vec<(String, TodoStatus)> {
    derive_current_todos(&derive_messages(&agent.session().events()))
        .into_iter()
        .map(|t| (t.content, t.status))
        .collect()
}

/// `add` appends a pending item, written as the model writes one — a
/// `todowrite` call and its result — so the list the model is told about is
/// the list the person sees; `clear` empties it.
#[tokio::test]
async fn a_person_adds_to_the_plan_and_clears_it() {
    let (app, agent) = started("add").await;

    assert_eq!(
        todo(&app, &agent, "add 补上回归测试").await,
        Ok("Added task: 补上回归测试".into())
    );
    todo(&app, &agent, "add 跑一遍全量")
        .await
        .expect("second add");
    assert_eq!(
        plan(&agent),
        [
            ("补上回归测试".to_string(), TodoStatus::Pending),
            ("跑一遍全量".to_string(), TodoStatus::Pending),
        ]
    );
    let events = agent.session().events();
    let calls = events
        .iter()
        .filter(|e| {
            matches!(&e.event, SessionEvent::AssistantMessage { tool_calls, .. }
                if tool_calls.iter().any(|c| c.name == "todowrite"))
        })
        .count();
    let results = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::ToolResultLogged { .. }))
        .count();
    assert_eq!(
        (calls, results),
        (2, 2),
        "each edit is one call and its result"
    );

    let listed = todo(&app, &agent, "").await.expect("listing");
    assert!(
        listed.contains("补上回归测试") && listed.contains("跑一遍全量"),
        "{listed}"
    );

    todo(&app, &agent, "clear").await.expect("clear");
    assert!(plan(&agent).is_empty(), "cleared: {:?}", plan(&agent));

    assert!(
        todo(&app, &agent, "add").await.is_err(),
        "`add` with nothing to add"
    );
    assert!(
        todo(&app, &agent, "remove 1").await.is_err(),
        "and a word it does not know"
    );
}

/// Inside a turn the pair would land between a call and its result, which is
/// a request the provider rejects: refused, and nothing written.
#[tokio::test]
async fn an_edit_waits_for_the_turn_to_end() {
    let (app, agent) = started("busy").await;
    agent.begin_turn();
    let before = agent.session().events().len();
    assert!(todo(&app, &agent, "add 不该进去").await.is_err());
    assert_eq!(
        agent.session().events().len(),
        before,
        "nothing was written"
    );
    agent.end_turn();
    todo(&app, &agent, "add 这回可以")
        .await
        .expect("between turns");
    assert_eq!(plan(&agent).len(), 1);
}

/// Filed under a turn that was taken back, the edit would be hidden with that
/// turn on screen while the model still read it. It gets a turn of its own.
#[tokio::test]
async fn an_edit_after_an_undo_is_not_filed_under_the_undone_turn() {
    let (app, agent) = started("undo").await;
    let log = agent.session();
    let turn = log.next_turn();
    let start = log.append(SessionEvent::TurnStart { turn });
    log.append(SessionEvent::Rewound {
        turn,
        to: start,
        scope: RewindScope::Conversation,
    });
    let undone = atomcode_kernel::session::undone_turns(&log.events());
    assert!(undone.contains(&turn), "the setup undid turn {turn}");

    todo(&app, &agent, "add 撤回之后加的").await.expect("add");
    let filed: Vec<u64> = log
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::ToolResultLogged { turn, .. } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(filed.len(), 1);
    assert!(
        !undone.contains(&filed[0]),
        "filed under undone turn {}",
        filed[0]
    );
    assert_eq!(plan(&agent).len(), 1, "and the model reads it");
}

/// Once a compaction has folded the plan away, the list the next turn is
/// reminded of is the sidecar plus the todo calls positioned after its
/// `through` (`atomcode-coding`'s `todo::current_todos`). An edit with no
/// position — or one that sorts before the turn's own calls — was dropped
/// there: on screen, never told to the model. Each edit stands after every
/// round of its turn, and a second after the first.
#[tokio::test]
async fn an_edit_stands_after_the_turns_own_todo_calls() {
    let (app, agent) = started("position").await;
    let log = agent.session();
    let turn = log.next_turn();
    log.append(SessionEvent::TurnStart { turn });
    log.append(SessionEvent::AssistantMessage {
        turn,
        round: 3,
        text: String::new(),
        reasoning: String::new(),
        tool_calls: vec![atomcode_kernel::tool::ToolCall {
            id: "model-plan".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"读代码","status":"pending"}]}"#.into(),
        }],
        reasoning_blocks: Vec::new(),
        meta: Some(atomcode_kernel::message::MessageMeta {
            turn_id: turn,
            round: 3,
            ..Default::default()
        }),
    });
    log.append(SessionEvent::TurnEnd {
        turn,
        stop: atomcode_harness::seams::StopReason::Stopped,
        error: None,
    });

    todo(&app, &agent, "add 第一项").await.expect("add");
    todo(&app, &agent, "add 第二项").await.expect("add");
    let positions: Vec<(u64, u32)> = log
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::AssistantMessage {
                tool_calls, meta, ..
            } if tool_calls.iter().any(|c| c.id.starts_with("todo-")) => {
                let meta = meta.as_ref().expect("an edit carries its position");
                Some((meta.turn_id, meta.round))
            }
            _ => None,
        })
        .collect();
    assert_eq!(positions, [(turn, 4), (turn, 5)]);
}

#[tokio::test]
async fn original_guidance_is_scoped_to_the_agent_and_read_without_a_new_turn() {
    let (app, agent) = started("sources").await;
    let log = agent.session();
    let original = "原始指导\r\n```diff\r\n@@ -1 +1 @@\r\n-old\r\n+new\r\n```";
    let source = log.append(SessionEvent::UserMessage {
        turn: 1,
        text: original.into(),
        images: vec![],
    });
    let args = serde_json::json!({"todos":[{"content":"apply the boundary fix","status":"in_progress"}],"source_seqs":[source]}).to_string();
    log.append(SessionEvent::AssistantMessage {
        turn: 1,
        round: 1,
        text: String::new(),
        reasoning: String::new(),
        reasoning_blocks: vec![],
        meta: None,
        tool_calls: vec![atomcode_kernel::tool::ToolCall {
            id: "source-plan".into(),
            name: "todowrite".into(),
            arguments: args.clone(),
        }],
    });
    let tools = app
        .context()
        .service::<atomcode_harness::seams::ToolsSvc>()
        .unwrap();
    let context = atomcode_kernel::tool::ToolContext {
        working_dir: ".".into(),
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: atomcode_kernel::tool::ProgressSink::noop(),
        requester: None,
    };
    let planner = tools.get("todowrite").unwrap();
    assert!(
        planner.execute(&args, &context).await.is_error,
        "no agent scope must fail closed"
    );
    let result =
        atomcode_harness::agent::as_agent(agent.ctx().clone(), planner.execute(&args, &context))
            .await;
    assert!(!result.is_error, "{}", result.content);
    log.append(SessionEvent::ToolResultLogged {
        turn: 1,
        round: 1,
        call_id: "source-plan".into(),
        content: result.content,
        is_error: false,
        images: vec![],
    });
    let before = log.events().len();
    assert!(todo(&app, &agent, "source")
        .await
        .unwrap()
        .contains(&format!("[{source}]")));
    let reader = tools.get("todo_sources").unwrap();
    let read_args = serde_json::json!({"action":"read","source_seq":source}).to_string();
    let read = atomcode_harness::agent::as_agent(
        agent.ctx().clone(),
        reader.execute(&read_args, &context),
    )
    .await;
    assert!(!read.is_error, "{}", read.content);
    let page: serde_json::Value = serde_json::from_str(&read.content).unwrap();
    assert_eq!(page["text"], original);
    assert_eq!(page["session_id"], log.id());
    assert!(todo(&app, &agent, &format!("source {source}"))
        .await
        .unwrap()
        .contains(original));
    assert_eq!(
        log.events().len(),
        before,
        "viewing a source submits no user message"
    );
    todo(&app, &agent, "clear").await.unwrap();
    assert!(todo(&app, &agent, &format!("source {source}"))
        .await
        .is_err());
}
