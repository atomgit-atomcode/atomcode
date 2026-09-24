//! Turn policy as rows: the round budget, provider retry, compaction and the
//! no-progress guard are none of the loop's business.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use atomcode_harness::seams::{CompactionSvc, StopReason};
use atomcode_harness::session::SessionEvent;
use atomcode_harness::{bundle, plugins, run_turn};
use atomcode_plexus::{App, ConfigTree, Layer, Next, Waterfall};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-policy-{}-{tag}-{n}", std::process::id()));
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
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 100, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut layers = vec![
        atomcode_coding::on_harness::base_layer(),
        atomcode_coding::on_harness::headless_patch(),
    ];
    for src in [script, quiet, scoped.as_str()] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

/// A model that answers once and stops.
///
/// Not an empty script: a response with neither text nor tool calls is now a
/// typed failure (`RequestError::empty`), because it cannot advance a turn and
/// silently accepting it produced empty assistant messages.
const NEVER_STOPS: &str = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [ { text = "ok" } ] }
"#;

/// Keeps calling a tool, so a round budget has something to bound.
fn always_calls(tool: &str, args: &str) -> String {
    let step = format!(r#"{{ text = "again", calls = [ {{ name = "{tool}", args = {args} }} ] }}"#);
    let steps = vec![step; 40].join(",\n  ");
    format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  {steps}\n] }}\n"
    )
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

#[tokio::test]
async fn the_round_budget_is_a_row_and_the_loop_only_has_a_fuse() {
    let dir = scratch("rounds");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let script = always_calls("read_file", r#"{ file_path = "a.txt" }"#);

    // With the row: the budget stops it, and says so.
    let capped = "[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 3 }\n\n\
                  [[remove]]\nid = \"repeat-fuse\"";
    let app = start(tree(&dir, &script, &[capped])).await;
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::MaxRounds);
    assert_eq!(outcome.rounds, 3);
    drop(app);

    // Without it: the loop's own fuse is what stops it, named differently so a
    // missing policy is visible rather than looking like a normal ending. The
    // guard comes out too, so the fuse is the only thing left that can stop it.
    let no_cap = "[[remove]]\nid = \"round-cap\"\n\n[[remove]]\nid = \"tool-loop-guard\"\n\n[[remove]]\nid = \"repeat-fuse\"\n\n\
                  [[patch]]\nid = \"agent-loop\"\nconfig = { max_rounds = 5 }";
    let app = start(tree(&dir, &script, &[no_cap])).await;
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::RunawayFuse);
    assert_eq!(outcome.rounds, 5);
}

/// The budget is a row, so it can be retuned on a *running* system — which is
/// what the front ends' `/patch` reaches. Mounted wide, then narrowed under the
/// running turn's feet: the turn must obey the patched value, not the one the
/// row was mounted with.
#[tokio::test]
async fn the_round_budget_is_reconfigurable_while_the_process_runs() {
    let dir = scratch("hot-rounds");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let script = always_calls("read_file", r#"{ file_path = "a.txt" }"#);

    // Wide enough that the mounted budget is not what ends this turn, and with
    // the other two stoppers out so `round-cap` is the only thing deciding.
    let wide = "[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 1000 }\n\n\
                [[remove]]\nid = \"repeat-fuse\"\n\n\
                [[remove]]\nid = \"tool-loop-guard\"";
    let mut app = start(tree(&dir, &script, &[wide])).await;

    app.patch(
        &Layer::from_toml("[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 4 }").unwrap(),
    )
    .await
    .expect("a running system takes a patch");

    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::MaxRounds);
    assert_eq!(
        outcome.rounds, 4,
        "the patched budget, not the 1000 it was mounted with"
    );
}

#[tokio::test]
async fn a_wall_clock_deadline_is_the_same_row_with_different_config() {
    let dir = scratch("deadline");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let script = always_calls("read_file", r#"{ file_path = "a.txt" }"#);
    // `max_seconds = 0` disables the deadline, so a turn that would trip an
    // enabled one runs on until the fuse. The two limits on this row are
    // independent, and neither is the loop's.
    let deadline =
        "[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 1000, max_seconds = 0 }\n\n\
                    [[remove]]\nid = \"tool-loop-guard\"\n\n[[remove]]\nid = \"repeat-fuse\"\n\n\
                    [[patch]]\nid = \"agent-loop\"\nconfig = { max_rounds = 10 }";
    let app = start(tree(&dir, &script, &[deadline])).await;
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::RunawayFuse);
    assert_eq!(outcome.rounds, 10, "the loop's fuse, not the policy");

    // Turn the deadline on with a zero-second budget and it owns the decision
    // on the very first check.
    let immediate =
        "[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 1000, max_seconds = 1 }\n\n\
                     [[remove]]\nid = \"tool-loop-guard\"\n\n[[remove]]\nid = \"repeat-fuse\"";
    let app = start(tree(&dir, &script, &[immediate])).await;
    let outcome = run_turn(&app, "go").await.unwrap();
    assert!(
        matches!(
            outcome.stop,
            StopReason::StoppedByPolicy | StopReason::Stopped
        ),
        "a deadline that has not elapsed must not stop the turn early: {:?}",
        outcome.stop
    );
}

// ---- retry --------------------------------------------------------------

/// Fails the first `fail_times` requests, then succeeds. Registered *below*
/// the retry listener, so retrying re-runs it.
struct FlakyProvider {
    seen: Arc<std::sync::atomic::AtomicU32>,
    fail_times: u32,
    error: RequestError,
}

#[async_trait]
impl Waterfall<AgentRequest> for FlakyProvider {
    async fn handle(
        &self,
        _req: &mut ModelRequest,
        _next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_times {
            return Err(self.error.clone());
        }
        Ok(ModelResponse {
            text: "recovered".into(),
            ..Default::default()
        })
    }
}

/// A transient failure the provider marked retryable.
fn transient(message: &str) -> RequestError {
    RequestError {
        retryable: true,
        http_status: Some(503),
        ..RequestError::message(message)
    }
}

/// An auth failure: retrying it burns time and money for nothing.
fn fatal(message: &str) -> RequestError {
    RequestError {
        retryable: false,
        http_status: Some(401),
        ..RequestError::message(message)
    }
}

async fn retry_case(
    fail_times: u32,
    error: RequestError,
    attempts: u32,
) -> (StopReason, String, u32) {
    let dir = scratch("retry");
    let row = format!(
        "[[patch]]\nid = \"llm-retry\"\nconfig = {{ attempts = {attempts}, backoff_ms = 1 }}\n"
    );
    let app = start(tree(&dir, NEVER_STOPS, &[row.as_str()])).await;
    let seen = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(FlakyProvider {
            seen: seen.clone(),
            fail_times,
            error,
        }),
        false,
    );
    let outcome = run_turn(&app, "go").await.unwrap();
    (
        outcome.stop,
        outcome.error.unwrap_or(outcome.text),
        seen.load(Ordering::SeqCst),
    )
}

#[tokio::test]
async fn retry_re_runs_the_request_rather_than_replaying_the_failure() {
    let (stop, text, attempts) = retry_case(2, transient("503 service unavailable"), 3).await;
    assert_eq!(stop, StopReason::Stopped);
    assert_eq!(text, "recovered");
    assert_eq!(attempts, 3, "two failures then a success — a real re-issue");
}

#[tokio::test]
async fn retry_gives_up_after_its_budget() {
    let (stop, error, attempts) = retry_case(99, transient("503 upstream down"), 3).await;
    assert_eq!(stop, StopReason::ProviderError);
    assert!(error.contains("503"), "{error}");
    assert_eq!(attempts, 3);
}

#[tokio::test]
async fn a_fatal_error_is_not_retried() {
    let (stop, error, attempts) = retry_case(99, fatal("401 invalid api key"), 3).await;
    assert_eq!(stop, StopReason::ProviderError);
    assert!(error.contains("401"), "{error}");
    assert_eq!(
        attempts, 1,
        "retrying an auth failure burns time and money for nothing"
    );
}

#[tokio::test]
async fn removing_the_retry_row_removes_the_retrying() {
    let dir = scratch("no-retry");
    let app = start(tree(&dir, NEVER_STOPS, &["[[remove]]\nid = \"llm-retry\""])).await;
    let seen = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(FlakyProvider {
            seen: seen.clone(),
            fail_times: 1,
            error: transient("503"),
        }),
        false,
    );
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::ProviderError);
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

// ---- compaction ---------------------------------------------------------

#[tokio::test]
async fn compaction_is_a_seam_with_a_describable_provider() {
    let dir = scratch("compaction-seam");
    let app = start(tree(&dir, NEVER_STOPS, &[])).await;
    let compaction = app.context().service::<CompactionSvc>().unwrap();
    assert!(compaction.describe().contains("keep the last"));
    drop(app);

    let app = start(tree(
        &dir,
        NEVER_STOPS,
        &["[[remove]]\nid = \"compaction-tail\""],
    ))
    .await;
    assert!(!app.context().service_names().contains(&"compaction"));
}

#[tokio::test]
async fn compaction_cuts_the_projection_and_leaves_the_log_whole() {
    let dir = scratch("compaction");
    // The replay adapter reports 100 prompt tokens and a 128k window, so a
    // threshold this low makes every request cross it.
    let eager =
        "[[patch]]\nid = \"compaction-tail\"\nconfig = { threshold = 0.0000001, keep_turns = 1 }";
    let app = start(tree(&dir, NEVER_STOPS, &[eager])).await;

    run_turn(&app, "first question").await.unwrap();
    run_turn(&app, "second question").await.unwrap();
    run_turn(&app, "third question").await.unwrap();

    let log = app.context().only_session().unwrap();
    let compactions = log
        .events()
        .into_iter()
        .filter(|e| matches!(e.event, SessionEvent::Compacted { .. }))
        .count();
    assert!(compactions >= 1, "the threshold should have been crossed");

    let projected = log
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        projected.contains("EARLIER IN THIS SESSION"),
        "the model sees a summary: {projected}"
    );
    assert!(
        projected.contains("first question"),
        "and the summary names what was asked"
    );

    // The facts are all still there — compaction is a projection decision.
    let raw = log
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::UserMessage { .. }))
        .count();
    assert_eq!(raw, 3, "every user message is still in the log");
}

/// The window is what the trigger divides by, so the same conversation and the
/// same threshold must compact at one window and not at another.
///
/// The replay model reports 100 prompt tokens, so `0.0005` puts the bar at 64
/// of a 128k window — crossed — and at 500 of a 1M one — not crossed. Only the
/// window differs between the two runs.
///
/// This is the bug the row's `context_window` exists to prevent: a 1M-token
/// model whose window was never stated falls back to 128k, so it compacts at a
/// fraction of the context it actually had.
#[tokio::test]
async fn the_window_decides_whether_compaction_fires() {
    const THRESHOLD: &str = "0.0005";
    let script = |window: &str| {
        format!(
            "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n\
             config = {{ context_window = {window}, script = [ {{ text = \"ok\" }} ] }}"
        )
    };
    let eager =
        format!("[[patch]]\nid = \"compaction-tail\"\nconfig = {{ threshold = {THRESHOLD}, keep_turns = 1 }}");

    let count_compactions = |log: &atomcode_harness::session::SessionLog| {
        log.events()
            .into_iter()
            .filter(|e| matches!(e.event, SessionEvent::Compacted { .. }))
            .count()
    };

    // 128k: 100 prompt tokens clears 64, so compaction fires.
    let dir = scratch("window-small");
    let app = start(tree(&dir, &script("128000"), &[&eager])).await;
    run_turn(&app, "first question").await.unwrap();
    run_turn(&app, "second question").await.unwrap();
    let small = app.context().only_session().unwrap();
    assert!(
        count_compactions(&small) >= 1,
        "a 128k window must compact well before a 1M one would"
    );
    drop(app);

    // 1M: the same 100 tokens is nowhere near 500, so nothing fires.
    let dir = scratch("window-large");
    let app = start(tree(&dir, &script("1000000"), &[&eager])).await;
    run_turn(&app, "first question").await.unwrap();
    run_turn(&app, "second question").await.unwrap();
    let large = app.context().only_session().unwrap();
    assert_eq!(
        count_compactions(&large),
        0,
        "a 1M window must not compact on the same history that a 128k one compacts"
    );
    assert!(
        large
            .derive_messages()
            .iter()
            .any(|m| m.text.contains("first question")),
        "and the history must still be whole"
    );
}

fn compaction_cuts(log: &atomcode_harness::session::SessionLog) -> Vec<u64> {
    log.events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Compacted { through, .. } => Some(through),
            _ => None,
        })
        .collect()
}

/// The trigger runs before every request of a turn, and pressure does not drop
/// between two rounds of the same turn when what is kept is itself large. A
/// history that was folded once has nothing new to fold until another turn
/// settles — so it is folded once, not once per round.
#[tokio::test]
async fn history_already_folded_is_not_folded_again() {
    let dir = scratch("fold-once");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let call =
        r#"{ text = "again", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] }"#;
    let script = format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
         {{ text = \"first answer\" }},\n  {call},\n  {call},\n  {call},\n  {call},\n  \
         {{ text = \"done\" }}\n] }}\n"
    );
    let eager = "[[patch]]\nid = \"compaction-tail\"\n\
                 config = { threshold = 0.0000001, keep_turns = 1 }\n\n\
                 [[remove]]\nid = \"repeat-fuse\"\n\n[[remove]]\nid = \"tool-loop-guard\"";
    let app = start(tree(&dir, &script, &[eager])).await;
    run_turn(&app, "first question").await.unwrap();
    run_turn(&app, "second question").await.unwrap();

    let cuts = compaction_cuts(&app.context().only_session().unwrap());
    assert_eq!(
        cuts.len(),
        1,
        "five requests in the second turn, one settled turn to fold: {cuts:?}"
    );
}

/// A summary is what the model knows of everything before it. A later
/// compaction that rebuilds its text from the raw log instead drops whatever
/// that summary said — a written one included, which is the one worth most.
#[tokio::test]
async fn a_later_compaction_keeps_what_the_earlier_summary_said() {
    let dir = scratch("keep-summary");
    let script = "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = { script = [ \
                  { text = \"a1\" }, { text = \"a2\" }, { text = \"a3\" }, { text = \"a4\" } ] }";
    let app = start(tree(&dir, script, &[])).await;
    run_turn(&app, "q1").await.unwrap();
    run_turn(&app, "q2").await.unwrap();
    let log = app.context().only_session().unwrap();
    let end_of_first_turn = log
        .events()
        .iter()
        .filter(|e| e.event.turn() <= 1)
        .map(|e| e.seq)
        .max()
        .unwrap();
    // What a written `/compact` leaves behind.
    atomcode_harness::session::apply_compaction(
        &app.context(),
        &log,
        atomcode_harness::seams::CompactionDecision::fold(
            end_of_first_turn,
            "WE-AGREED-TO-USE-POSTGRES",
        ),
    );
    run_turn(&app, "q3").await.unwrap();
    run_turn(&app, "q4").await.unwrap();

    let compaction = app.context().service::<CompactionSvc>().unwrap();
    let pressure = atomcode_harness::seams::CompactionAsk {
        trigger: atomcode_kernel::message::CompactTrigger::Auto { utilization: 0.8 },
        window: 128_000,
        used_tokens: 102_400,
    };
    let decision = compaction
        .compact(&log, &pressure)
        .await
        .expect("turns settled since");
    atomcode_harness::session::apply_compaction(&app.context(), &log, decision);
    let projected = log
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        projected.contains("WE-AGREED-TO-USE-POSTGRES"),
        "the earlier summary is gone: {projected}"
    );
    assert!(
        projected.contains("q2"),
        "what was asked since is listed: {projected}"
    );
    assert!(
        !projected.contains("- q1"),
        "what the earlier summary already stands for is not listed again: {projected}"
    );
}

/// A compaction strategy that says it would call a model and never finds
/// anything to commit — and counts how often it was asked.
struct Reluctant(Arc<AtomicU32>);

#[async_trait]
impl atomcode_harness::seams::Compaction for Reluctant {
    fn describe(&self) -> String {
        "asks a model, commits nothing".into()
    }
    fn calls_model(
        &self,
        _: &atomcode_harness::session::SessionLog,
        _: &atomcode_harness::seams::CompactionAsk,
    ) -> bool {
        true
    }
    async fn compact(
        &self,
        _: &atomcode_harness::session::SessionLog,
        _: &atomcode_harness::seams::CompactionAsk,
    ) -> Option<atomcode_harness::seams::CompactionDecision> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
}

struct ReluctantRow(Arc<AtomicU32>);

#[async_trait]
impl atomcode_plexus::Plugin for ReluctantRow {
    fn name(&self) -> &'static str {
        "compaction-reluctant"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["compaction"]
    }
    fn description(&self) -> &'static str {
        "a compaction that never commits"
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _: &serde_json::Value,
    ) -> Result<(), String> {
        let _ = ctx
            .provide::<CompactionSvc>(Arc::new(Reluctant(self.0.clone())))
            .map_err(|e| e.to_string())?;
        plugins::loop_policy::mount_compaction_trigger(ctx, 0.0000001);
        Ok(())
    }
}

/// Pressure does not fall between two rounds of one turn, and the trigger runs
/// before each. A compaction that calls a model is tried once a turn — not once
/// a round, billed each time for finding nothing — and the start a driver was
/// shown for it is closed even though it committed nothing, or the driver
/// holds its work back for a compaction that is never going to finish.
#[tokio::test]
async fn a_slow_compaction_is_tried_once_a_turn_and_its_start_is_always_closed() {
    let dir = scratch("once-a-turn");
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    let call =
        r#"{ text = "again", calls = [ { name = "read_file", args = { file_path = "a.txt" } } ] }"#;
    let script = format!(
        "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [\n  \
         {call},\n  {call},\n  {call},\n  {call},\n  {{ text = \"done\" }}\n] }}\n"
    );
    let swap = "[[remove]]\nid = \"compaction-tail\"\n\n\
                [[insert]]\nid = \"compaction-reluctant\"\nname = \"compaction-reluctant\"\n\n\
                [[remove]]\nid = \"repeat-fuse\"\n\n[[remove]]\nid = \"tool-loop-guard\"";
    let asked = Arc::new(AtomicU32::new(0));
    let mut registry = plugins::catalog();
    registry.register(Arc::new(ReluctantRow(asked.clone())));
    let mut app = App::new(registry, tree(&dir, &script, &[swap]));
    app.start().await.expect("must mount");

    run_turn(&app, "first").await.unwrap();
    // The first request has no usage to measure; the four after it all do.
    assert_eq!(asked.load(Ordering::SeqCst), 1, "asked once in the turn");

    let events: Vec<SessionEvent> = app
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .map(|e| e.event)
        .collect();
    let driven = plugins::handle::replay(&events, 128_000);
    let started = driven
        .iter()
        .position(|e| {
            matches!(
                e,
                atomcode_kernel::event::AgentEvent::CompactionStarted { .. }
            )
        })
        .expect("the driver was told a compaction started");
    assert!(
        driven[started..].iter().any(|e| matches!(
            e,
            atomcode_kernel::event::AgentEvent::Compacted {
                committed: false,
                ..
            }
        )),
        "and never that it finished"
    );
}

// ---- compaction written by the utility model -----------------------------

/// Swapping the strategy is a patch, not a rebuild: remove the model-free row,
/// insert the summarising one. One seam has one provider, so they replace.
fn swap_compaction(extra: &[&str]) -> Vec<String> {
    let mut rows = vec![
        "[[remove]]\nid = \"compaction-tail\"".to_string(),
        "[[insert]]\nid = \"compaction-summary\"\nname = \"compaction-summary\"\n\
         config = { threshold = 0.0000001, keep_turns = 1 }"
            .to_string(),
    ];
    rows.extend(extra.iter().map(|s| s.to_string()));
    rows
}

#[tokio::test]
async fn the_utility_model_writes_the_summary_when_the_summary_row_is_mounted() {
    let dir = scratch("summary-model");
    // A script per call, because the eager threshold compacts more than once.
    let utility = "[[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
                   config = { script = [ { text = \"WE-AGREED-TO-USE-POSTGRES\" }, \
                                         { text = \"WE-AGREED-TO-USE-POSTGRES\" }, \
                                         { text = \"WE-AGREED-TO-USE-POSTGRES\" } ] }";
    let rows = swap_compaction(&[utility]);
    let extra: Vec<&str> = rows.iter().map(String::as_str).collect();
    let app = start(tree(&dir, NEVER_STOPS, &extra)).await;
    assert!(
        app.context().service_names().contains(&"compaction"),
        "the swap left the seam filled"
    );

    run_turn(&app, "first question").await.unwrap();
    run_turn(&app, "second question").await.unwrap();
    run_turn(&app, "third question").await.unwrap();

    let log = app.context().only_session().unwrap();
    let projected = log
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        projected.contains("WE-AGREED-TO-USE-POSTGRES"),
        "the model's summary is what the continuation sees: {projected}"
    );
    assert!(
        !projected.contains("first question"),
        "and it replaced the raw list rather than joining it: {projected}"
    );
}

#[tokio::test]
async fn without_a_utility_model_the_summary_falls_back_to_the_model_free_list() {
    let dir = scratch("summary-fallback");
    // Mounted with no `llm-utility` row at all: the call resolves to nothing and
    // the model-free text stands, so a provider that is down degrades the
    // *quality* of the summary and nothing else.
    let rows = swap_compaction(&[]);
    let extra: Vec<&str> = rows.iter().map(String::as_str).collect();
    let app = start(tree(&dir, NEVER_STOPS, &extra)).await;

    run_turn(&app, "first question").await.unwrap();
    run_turn(&app, "second question").await.unwrap();
    run_turn(&app, "third question").await.unwrap();

    let log = app.context().only_session().unwrap();
    let projected = log
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        projected.contains("EARLIER IN THIS SESSION"),
        "the fallback is the same block the model-free row would have written: {projected}"
    );
    assert!(
        projected.contains("first question"),
        "and it still names what was asked: {projected}"
    );
}

// ---- tool-loop guard ----------------------------------------------------

#[tokio::test]
async fn the_guard_warns_then_ends_a_turn_that_is_not_progressing() {
    let dir = scratch("guard");
    std::fs::write(dir.join("a.txt"), "same content every time").unwrap();
    let script = always_calls("read_file", r#"{ file_path = "a.txt" }"#);
    // Round budget above the guard's threshold, so the guard is what stops it.
    let rows = "[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 20 }\n\n\
                [[remove]]\nid = \"repeat-fuse\"\n\n\
                [[patch]]\nid = \"tool-loop-guard\"\nconfig = { warn_after = 2, stop_after = 3 }";
    let app = start(tree(&dir, &script, &[rows])).await;
    let outcome = run_turn(&app, "go").await.unwrap();

    assert_eq!(outcome.stop, StopReason::ToolLoopDetected);
    let text = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Change approach"),
        "the model gets a warning before the turn is taken away: {text}"
    );
    assert!(text.contains("Stopping"), "{text}");
}

#[tokio::test]
async fn the_guard_does_not_fire_when_results_differ() {
    let dir = scratch("guard-progress");
    // Each call appends, so the result differs every time — that is progress,
    // not a loop, and the guard must not confuse the two. Counted rather than
    // timed: BSD `date` (macOS) has no `%N`, so two calls in the same second
    // answered the same and the guard, rightly, stopped the turn.
    let script = always_calls(
        "bash",
        r#"{ command = "echo x >> ticks && wc -l < ticks" }"#,
    );
    let rows = "[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 5 }\n\n\
                [[remove]]\nid = \"repeat-fuse\"\n\n\
                [[patch]]\nid = \"tool-loop-guard\"\nconfig = { warn_after = 2, stop_after = 3 }\n\n\
                [[patch]]\nid = \"approval\"\nconfig = { mode = \"yolo\" }";
    let app = start(tree(&dir, &script, &[rows])).await;
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(
        outcome.stop,
        StopReason::MaxRounds,
        "a changing result is progress; the round budget is what should stop this"
    );
}

#[tokio::test]
async fn every_policy_row_can_be_removed_at_once() {
    let dir = scratch("no-policy");
    let strip = r#"
[[remove]]
id = "round-cap"
[[remove]]
id = "llm-retry"
[[remove]]
id = "compaction-tail"
[[remove]]
id = "tool-loop-guard"
"#;
    let app = start(tree(&dir, NEVER_STOPS, &[strip])).await;
    let outcome = run_turn(&app, "go").await.unwrap();
    // An empty script answers with no tool calls, so the turn ends normally.
    assert_eq!(outcome.stop, StopReason::Stopped);
    assert!(
        !app.context().service_names().contains(&"compaction"),
        "the compaction seam goes with its only provider"
    );
}
