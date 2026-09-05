//! Two things ported out of the kernel's turn loop: overlapping the tool calls
//! that are safe to overlap, and letting a cancellation actually reach a tool.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use atomcode_harness::seams::{SessionSvc, StopReason, ToolsSvc};
use atomcode_harness::{bundle, create_agent, drive, plugins};
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{App, ConfigTree, Layer};
use serde_json::{json, Value};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-conc-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Sleeps, and records when it started and finished. Declares itself
/// parallel-safe or not on demand, so a test can watch what the loop does with
/// each claim.
struct Slow {
    name: &'static str,
    millis: u64,
    parallel_safe: bool,
    log: Arc<Mutex<Vec<(String, Instant, Instant)>>>,
}

#[async_trait]
impl Tool for Slow {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "sleeps"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "n": { "type": "string" } } })
    }
    fn read_only_hint(&self) -> bool {
        self.parallel_safe
    }
    fn parallel_safe(&self, _args: &str) -> bool {
        self.parallel_safe
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(self.millis)).await;
        let done = Instant::now();
        self.log
            .lock()
            .unwrap()
            .push((format!("{}{args}", self.name), started, done));
        ToolResult {
            call_id: String::new(),
            content: format!("{} done", self.name),
            is_error: false,
            images: vec![],
        }
    }
}

/// Blocks until the turn is cancelled — the shape of a long-running command.
struct WaitsForCancel {
    saw_cancel: Arc<Mutex<bool>>,
}

#[async_trait]
impl Tool for WaitsForCancel {
    fn name(&self) -> &str {
        "waits"
    }
    fn description(&self) -> &str {
        "waits until cancelled"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
        // A tool that polls its token is exactly what a cancel has to reach.
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                *self.saw_cancel.lock().unwrap() = true;
                ToolResult { call_id: String::new(), content: "cancelled".into(), is_error: true, images: vec![] }
            }
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                ToolResult { call_id: String::new(), content: "ran to completion".into(), is_error: false, images: vec![] }
            }
        }
    }
}

fn tree(root: &std::path::Path, script: &str, extra: &[&str]) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let base = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"tool-loop-guard\"\ndisabled = true\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut layers = vec![
        bundle::base().unwrap(),
        Layer::from_toml(bundle::ONESHOT_APP).unwrap(),
    ];
    for src in [base.as_str(), script, bundle::YOLO] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

/// One model turn that fires `n` calls of `tool` at once.
fn fires(tool: &str, n: usize) -> String {
    let calls = (0..n)
        .map(|i| format!(r#"{{ name = "{tool}", args = {{ n = "{i}" }} }}"#))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
         {{ text = \"working\", calls = [ {calls} ] }},\n  {{ text = \"done\" }},\n] }}\n"
    )
}

async fn start_with(tree: ConfigTree, tools: Vec<Arc<dyn Tool>>) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    // Register into the live catalog rather than replacing the service: the
    // `tools` row already fills that slot in this realm, and `provide` refuses
    // a second claimant rather than overwriting it.
    let catalog = app
        .context()
        .service::<ToolsSvc>()
        .expect("the tools row is mounted");
    for tool in tools {
        catalog.register(tool).expect("test tool name is free");
    }
    app
}

#[tokio::test]
async fn read_only_calls_overlap() {
    let dir = scratch("overlap");
    let log = Arc::new(Mutex::new(Vec::new()));
    let app = start_with(
        tree(&dir, &fires("slow", 4), &[]),
        vec![Arc::new(Slow {
            name: "slow",
            millis: 150,
            parallel_safe: true,
            log: log.clone(),
        })],
    )
    .await;

    let started = Instant::now();
    let agent = create_agent(&app).unwrap();
    agent.send("go");
    let outcome = drive(&app, &agent).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome.tool_calls, 4);
    assert_eq!(log.lock().unwrap().len(), 4);
    // Serially this is 600ms. With a cap of 4 it is one wave.
    assert!(
        elapsed < Duration::from_millis(450),
        "four 150ms read-only calls took {elapsed:?} — they did not overlap"
    );
}

#[tokio::test]
async fn a_side_effecting_call_runs_alone() {
    let dir = scratch("barrier");
    let log = Arc::new(Mutex::new(Vec::new()));
    let app = start_with(
        tree(&dir, &fires("mutates", 3), &[]),
        vec![Arc::new(Slow {
            name: "mutates",
            millis: 120,
            parallel_safe: false,
            log: log.clone(),
        })],
    )
    .await;

    let agent = create_agent(&app).unwrap();
    agent.send("go");
    drive(&app, &agent).await.unwrap();

    // No two runs may overlap: a concurrent read must never observe a
    // half-applied mutation, so a side-effecting tool takes the write side of
    // the barrier and runs alone.
    let mut spans = log.lock().unwrap().clone();
    assert_eq!(spans.len(), 3, "all three must have run");
    spans.sort_by_key(|(_, start, _)| *start);
    for pair in spans.windows(2) {
        let (a_name, _, a_end) = &pair[0];
        let (b_name, b_start, _) = &pair[1];
        assert!(
            b_start >= a_end,
            "`{b_name}` started before `{a_name}` finished — the barrier did not hold"
        );
    }
}

#[tokio::test]
async fn results_come_back_in_emission_order() {
    let dir = scratch("order");
    let log = Arc::new(Mutex::new(Vec::new()));
    // Descending durations: finishing order is the reverse of firing order.
    let app = start_with(
        tree(&dir, &fires("slow", 3), &[]),
        vec![Arc::new(Slow {
            name: "slow",
            millis: 60,
            parallel_safe: true,
            log: log.clone(),
        })],
    )
    .await;

    let agent = create_agent(&app).unwrap();
    agent.send("go");
    drive(&app, &agent).await.unwrap();

    // The transcript must follow the order the model asked for, whatever order
    // the futures completed in — otherwise a replay is not reproducible.
    let ids: Vec<String> = app
        .context()
        .service::<SessionSvc>()
        .unwrap()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            atomcode_harness::session::SessionEvent::ToolResultLogged { call_id, .. } => {
                Some(call_id)
            }
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 3, "all three results must be logged");
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        ids, sorted,
        "results were logged out of emission order: {ids:?}"
    );
}

#[tokio::test]
async fn removing_the_scheduler_restores_serial_execution() {
    let dir = scratch("serial");
    let log = Arc::new(Mutex::new(Vec::new()));
    let serial = "[[remove]]\nid = \"tool-exec-parallel\"";
    let app = start_with(
        tree(&dir, &fires("slow", 3), &[serial]),
        vec![Arc::new(Slow {
            name: "slow",
            millis: 80,
            parallel_safe: true,
            log: log.clone(),
        })],
    )
    .await;

    let started = Instant::now();
    let agent = create_agent(&app).unwrap();
    agent.send("go");
    drive(&app, &agent).await.unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(240),
        "with no scheduler row the loop's own terminal runs them in a line"
    );
}

#[tokio::test]
async fn a_cancel_reaches_a_running_tool() {
    let dir = scratch("cancel");
    let saw = Arc::new(Mutex::new(false));
    let app = start_with(
        tree(&dir, &fires("waits", 1), &[]),
        vec![Arc::new(WaitsForCancel {
            saw_cancel: saw.clone(),
        })],
    )
    .await;

    let agent = create_agent(&app).unwrap();
    agent.send("go");
    let cancelling = agent.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancelling.cancel();
    });

    let started = Instant::now();
    let outcome = drive(&app, &agent).await.unwrap();

    assert!(
        *saw.lock().unwrap(),
        "the tool never saw the cancel — its context held a token nobody could fire"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the turn waited for the tool's own timeout instead of stopping"
    );
    assert_eq!(outcome.stop, StopReason::Cancelled);
}

#[tokio::test]
async fn a_call_that_had_not_started_when_cancelled_never_runs() {
    let dir = scratch("cancel-queue");
    let log = Arc::new(Mutex::new(Vec::new()));
    let serial = "[[remove]]\nid = \"tool-exec-parallel\"";
    let app = start_with(
        tree(&dir, &fires("slow", 4), &[serial]),
        vec![Arc::new(Slow {
            name: "slow",
            millis: 100,
            parallel_safe: true,
            log: log.clone(),
        })],
    )
    .await;

    let agent = create_agent(&app).unwrap();
    agent.send("go");
    let cancelling = agent.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancelling.cancel();
    });
    drive(&app, &agent).await.unwrap();

    let ran = log.lock().unwrap().len();
    assert!(
        ran < 4,
        "every queued call ran anyway ({ran}/4) — a cancel must stop the ones that had not started"
    );
}
