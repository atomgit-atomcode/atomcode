//! Recovery policies ported out of the kernel's turn loop: waiting out a rate
//! limit, compacting when the history stops fitting, and bounding a request
//! that never returns.
//!
//! Each is a row. Every test here makes the same point: the behaviour is present
//! when the row is, absent when it is not, and decided by the provider's own
//! classification rather than by matching on message text.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
// tokio 的 Instant:未暂停时等价 std,`start_paused` 下跟随虚拟时钟,
// 于是下面那些 `elapsed() < N` 的上界断言在虚拟钟上依然是真判据。
use tokio::time::Instant;

use async_trait::async_trait;
use atomcode_harness::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use atomcode_harness::seams::StopReason;
use atomcode_harness::session::SessionEvent;
use atomcode_harness::{bundle, run_turn};
use atomcode_plexus::{App, ConfigTree, Layer, Next, Waterfall};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-rec-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn tree(root: &std::path::Path, extra: &[&str]) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let base = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {{ text = \"ok\" }} ] }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 6, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut layers = vec![
        atomcode_coding::on_harness::base_layer(),
        atomcode_coding::on_harness::headless_patch(),
        Layer::from_toml(bundle::ONESHOT_APP).unwrap(),
        Layer::from_toml(&base).unwrap(),
    ];
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(atomcode_coding::on_harness::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

/// Fails a fixed number of times with a given error, then succeeds. Registered
/// below the recovery listeners, so each of their retries re-runs it.
struct Failing {
    seen: Arc<AtomicU32>,
    fail_times: u32,
    error: RequestError,
    /// Message counts seen on each attempt, so a test can check whether the
    /// history actually shrank between them.
    sizes: Arc<std::sync::Mutex<Vec<usize>>>,
}

#[async_trait]
impl Waterfall<AgentRequest> for Failing {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        _next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        self.sizes.lock().unwrap().push(req.messages.len());
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

struct Run {
    stop: StopReason,
    text: String,
    error: Option<String>,
    attempts: u32,
    sizes: Vec<usize>,
    elapsed: Duration,
}

async fn run_with(app: &App, fail_times: u32, error: RequestError) -> Run {
    let seen = Arc::new(AtomicU32::new(0));
    let sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(Failing {
            seen: seen.clone(),
            fail_times,
            error,
            sizes: sizes.clone(),
        }),
        false,
    );
    let started = Instant::now();
    let outcome = run_turn(app, "go").await.unwrap();
    let observed = sizes.lock().unwrap().clone();
    Run {
        stop: outcome.stop,
        text: outcome.text,
        error: outcome.error,
        attempts: seen.load(Ordering::SeqCst),
        sizes: observed,
        elapsed: started.elapsed(),
    }
}

fn rate_limited(retry_after: Option<u64>) -> RequestError {
    RequestError {
        retryable: true,
        http_status: Some(429),
        retry_after: retry_after.map(Duration::from_secs),
        ..RequestError::message("429 too many requests")
    }
}

fn overflowing() -> RequestError {
    RequestError {
        http_status: Some(400),
        code: Some("context_length_exceeded".into()),
        context_overflow: true,
        ..RequestError::message("this model's maximum context length is 8192 tokens")
    }
}

// ---- rate limiting ------------------------------------------------------

#[tokio::test]
async fn a_rate_limit_is_waited_out_and_the_turn_recovers() {
    let dir = scratch("429");
    let fast = "[[patch]]\nid = \"llm-rate-limit\"\nconfig = { max_waits = 5, max_wait_secs = 120, fallback_secs = 0 }";
    let app = start(tree(&dir, &[fast])).await;
    let run = run_with(&app, 2, rate_limited(Some(0))).await;

    assert_eq!(run.stop, StopReason::Stopped);
    assert_eq!(run.text, "recovered");
    assert_eq!(run.attempts, 3, "two limits waited out, then success");
}

/// A state the screen must show has to be a fact the log holds.
///
/// The failure this pins: these three recoveries used to `eprintln!` to stderr.
/// In a full-screen UI that corrupts the display it was meant to inform, a panel
/// cannot render a state that exists only on stderr, a resumed session cannot
/// reproduce it, and a test cannot construct it.
#[tokio::test]
async fn a_recovery_a_person_should_know_about_is_logged_not_printed() {
    let dir = scratch("notice");
    let fast = "[[patch]]\nid = \"llm-rate-limit\"\nconfig = { max_waits = 5, max_wait_secs = 120, fallback_secs = 0 }";
    let app = start(tree(&dir, &[fast])).await;
    run_with(&app, 2, rate_limited(Some(0))).await;

    let log = app.context().only_session().unwrap();
    let notices: Vec<_> = log
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Notice { notice, detail, .. } => Some((notice, detail)),
            _ => None,
        })
        .collect();

    assert_eq!(notices.len(), 2, "one per wait: {notices:?}");
    assert!(notices
        .iter()
        .all(|(k, _)| *k == atomcode_harness::session::NoticeKind::RateLimited));
    assert!(notices[0].1.contains("rate limited"), "{notices:?}");

    // Given to a person, kept from the model: the conversation is unchanged.
    assert!(
        !log.derive_messages()
            .iter()
            .any(|m| m.text.contains("rate limited")),
        "a notice must not reach the model"
    );
}

/// A retry is logged with the provider's error as it came and the numbers apart
/// from it.
///
/// The failure this pins: the notice was one English sentence
/// (`"<error>; retrying in 3s (1/2)"`) and the driver's `ProviderRetry` had its
/// numbers zeroed, so every screen that wrote its own "N 秒后重试(a/b)" around
/// it said `…; retrying in 3s (1/2)，0 秒后重试(0/0)`.
#[tokio::test]
async fn a_provider_retry_keeps_the_error_and_its_numbers_apart() {
    let dir = scratch("provider-retry");
    let fast = "[[patch]]\nid = \"llm-retry\"\nconfig = { attempts = 3, backoff_ms = 1 }";
    let app = start(tree(&dir, &[fast])).await;
    let transient = RequestError {
        retryable: true,
        ..RequestError::message("open failed: 等待首字节超过 90s(网关无响应)")
    };
    let run = run_with(&app, 2, transient).await;
    assert_eq!(run.text, "recovered");

    let retries: Vec<_> = app
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Notice {
                notice: atomcode_harness::session::NoticeKind::ProviderRetry,
                detail,
                retry,
                ..
            } => Some((detail, retry)),
            _ => None,
        })
        .collect();

    assert_eq!(retries.len(), 2, "one per retry: {retries:?}");
    for (i, (detail, retry)) in retries.iter().enumerate() {
        assert_eq!(detail, "open failed: 等待首字节超过 90s(网关无响应)");
        let retry = retry.expect("the numbers are kept");
        assert_eq!((retry.attempt, retry.max_attempts), (i as u32 + 1, 2));
    }
}

#[tokio::test]
async fn a_real_retry_after_is_honoured_over_any_guess() {
    let dir = scratch("retry-after");
    // The fallback is huge; the header says 1s. Waiting *less* than the server
    // asked is how a client gets its limit extended, so the header has to win
    // outright — not the smaller of the two.
    let row = "[[patch]]\nid = \"llm-rate-limit\"\nconfig = { max_waits = 3, max_wait_secs = 120, fallback_secs = 30 }";
    let app = start(tree(&dir, &[row])).await;
    let run = run_with(&app, 1, rate_limited(Some(1))).await;

    assert_eq!(run.stop, StopReason::Stopped);
    assert!(
        run.elapsed >= Duration::from_millis(900) && run.elapsed < Duration::from_secs(10),
        "waited {:?} — the header said 1s",
        run.elapsed
    );
}

#[tokio::test]
async fn a_window_that_resets_too_far_out_hands_the_turn_back() {
    let dir = scratch("429-long");
    let row = "[[patch]]\nid = \"llm-rate-limit\"\nconfig = { max_waits = 5, max_wait_secs = 2, fallback_secs = 1 }";
    let app = start(tree(&dir, &[row])).await;
    let run = run_with(&app, 99, rate_limited(Some(3600))).await;

    assert_eq!(run.stop, StopReason::ProviderError);
    assert!(
        run.elapsed < Duration::from_secs(5),
        "blocking an hour is a hang, not a self-heal: waited {:?}",
        run.elapsed
    );
    let error = run.error.unwrap_or_default();
    assert!(
        error.contains("3600s"),
        "the message says how long: {error}"
    );
    assert_eq!(run.attempts, 1, "it did not keep asking");
}

#[tokio::test]
async fn removing_the_rate_limit_row_removes_the_waiting() {
    let dir = scratch("no-429");
    let app = start(tree(&dir, &["[[remove]]\nid = \"llm-rate-limit\""])).await;
    let run = run_with(&app, 99, rate_limited(Some(0))).await;

    assert_eq!(run.stop, StopReason::ProviderError);
    // The plain retry row deliberately leaves 429s alone: retrying a limit that
    // has not lifted just burns the budget faster.
    assert_eq!(run.attempts, 1);
}

// ---- context overflow ---------------------------------------------------

#[tokio::test]
async fn an_overflow_compacts_and_retries_with_less_history() {
    let dir = scratch("overflow");
    let app = start(tree(&dir, &[])).await;

    // Build up history first, so there is something to compact away.
    run_turn(&app, "first").await.unwrap();
    run_turn(&app, "second").await.unwrap();
    run_turn(&app, "third").await.unwrap();

    let run = run_with(&app, 1, overflowing()).await;
    assert_eq!(run.stop, StopReason::Stopped);
    assert_eq!(run.text, "recovered");
    assert_eq!(run.attempts, 2, "one overflow, one retry");
    assert!(
        run.sizes.len() == 2 && run.sizes[1] < run.sizes[0],
        "the retry must carry *less* history, or it fails identically: {:?}",
        run.sizes
    );

    // The cut is recorded, so a transcript explains why the model suddenly
    // stopped seeing the earlier turns.
    let compactions = app
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter(|e| matches!(e.event, SessionEvent::Compacted { .. }))
        .count();
    assert!(compactions >= 1);
}

/// Records the longest message each request carried.
struct Widest(Arc<std::sync::Mutex<Vec<usize>>>);

#[async_trait]
impl Waterfall<AgentRequest> for Widest {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let widest = req.messages.iter().map(|m| m.text.len()).max().unwrap_or(0);
        self.0.lock().unwrap().push(widest);
        next.run(req).await
    }
}

/// A request already past what the model can take is compacted before it is
/// sent. Not every gateway refuses one: some answer it with an empty success,
/// and a ladder that only climbs on a refusal never starts.
#[tokio::test]
async fn a_request_that_would_not_fit_is_compacted_before_it_is_sent() {
    let dir = scratch("overflow-presend");
    let window = "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n\
                  config = { context_window = 30000, script = [ { text = \"ok\" } ] }";
    let app = start(tree(&dir, &[window])).await;
    run_turn(&app, "first").await.unwrap();

    // A tool result far larger than the 30k window, as a first turn leaves one.
    let ctx = app.context();
    let log = ctx.only_session().unwrap();
    let turn = log.current_turn();
    let call = atomcode_kernel::tool::ToolCall {
        id: "big".into(),
        name: "read_file".into(),
        arguments: "{}".into(),
    };
    atomcode_harness::session::commit(
        &ctx,
        &log,
        SessionEvent::AssistantMessage {
            turn,
            round: 2,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![call],
            reasoning_blocks: Vec::new(),
            meta: None,
        },
    );
    atomcode_harness::session::commit(
        &ctx,
        &log,
        SessionEvent::ToolResultLogged {
            turn,
            round: 2,
            call_id: "big".into(),
            content: "a line of a very large file\n".repeat(6_000),
            is_error: false,
            images: vec![],
        },
    );

    let widest = Arc::new(std::sync::Mutex::new(Vec::new()));
    let _guard = ctx.on_waterfall::<AgentRequest>(Arc::new(Widest(widest.clone())), false);
    let run = run_with(&app, 0, overflowing()).await;
    assert_eq!(run.attempts, 1, "sent once");
    let widest = widest.lock().unwrap().clone();
    assert!(
        widest.iter().all(|w| *w < 10_000),
        "a request went out carrying the whole result: {widest:?}"
    );
}

#[tokio::test]
async fn a_bounded_ladder_gives_up_instead_of_spinning() {
    let dir = scratch("overflow-hopeless");
    let row = "[[patch]]\nid = \"compaction-overflow\"\nconfig = { max_attempts = 2 }";
    let app = start(tree(&dir, &[row])).await;
    run_turn(&app, "first").await.unwrap();
    run_turn(&app, "second").await.unwrap();

    let run = run_with(&app, 99, overflowing()).await;
    assert_eq!(run.stop, StopReason::ProviderError);
    assert!(
        run.attempts <= 3,
        "an unrecoverable overflow must stop, not loop: {} attempts",
        run.attempts
    );
}

#[tokio::test]
async fn an_overflow_with_nothing_to_compact_says_so() {
    let dir = scratch("overflow-no-provider");
    let app = start(tree(&dir, &["[[remove]]\nid = \"compaction-tail\""])).await;
    let run = run_with(&app, 99, overflowing()).await;

    assert_eq!(run.stop, StopReason::ProviderError);
    let error = run.error.unwrap_or_default();
    assert!(
        error.contains("compaction") || error.contains("compacted"),
        "the message must explain why it cannot recover: {error}"
    );
}

#[tokio::test]
async fn removing_the_overflow_row_removes_the_ladder() {
    let dir = scratch("no-overflow");
    let app = start(tree(&dir, &["[[remove]]\nid = \"compaction-overflow\""])).await;
    let run = run_with(&app, 1, overflowing()).await;

    assert_eq!(run.stop, StopReason::ProviderError);
    assert_eq!(
        run.attempts, 1,
        "nothing tried to make the next attempt fit"
    );
}

// ---- classification -----------------------------------------------------

#[tokio::test]
async fn recovery_reads_the_providers_classification_not_the_message() {
    let dir = scratch("classify");
    let app = start(tree(&dir, &[])).await;

    // A message that *reads* like an overflow but is classified as fatal.
    // Matching on text would send it down the compaction ladder and waste the
    // turn; the provider's verdict is what decides.
    let misleading = RequestError {
        http_status: Some(401),
        retryable: false,
        context_overflow: false,
        ..RequestError::message("context length exceeded — but actually this is an auth failure")
    };
    let run = run_with(&app, 99, misleading).await;

    assert_eq!(run.stop, StopReason::ProviderError);
    assert_eq!(
        run.attempts, 1,
        "a fatal error must not be retried or compacted, whatever it reads like"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_empty_response_is_a_typed_failure_and_is_retried() {
    let dir = scratch("empty");
    let app = start(tree(&dir, &[])).await;
    let run = run_with(&app, 2, RequestError::empty()).await;

    assert_eq!(run.stop, StopReason::Stopped);
    assert_eq!(run.text, "recovered");
    assert_eq!(
        run.attempts, 3,
        "a response with no content cannot advance a turn; retrying is the only \
         thing that can, and accepting it silently produced empty assistant messages"
    );
}

// ---- liveness -----------------------------------------------------------

/// Never returns.
struct Hangs;

#[async_trait]
impl Waterfall<AgentRequest> for Hangs {
    async fn handle(
        &self,
        _req: &mut ModelRequest,
        _next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        tokio::time::sleep(Duration::from_secs(600)).await;
        Ok(ModelResponse::default())
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_request_that_never_returns_is_bounded() {
    let dir = scratch("hang");
    let row = "[[patch]]\nid = \"llm-request-timeout\"\nconfig = { request_secs = 1 }";
    let app = start(tree(&dir, &[row])).await;
    let _guard = app
        .context()
        .on_waterfall::<AgentRequest>(Arc::new(Hangs), false);

    let started = Instant::now();
    let outcome = run_turn(&app, "go").await.unwrap();

    assert_eq!(outcome.stop, StopReason::ProviderError);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a hung request must not hang the turn: {:?}",
        started.elapsed()
    );
    assert!(outcome.error.unwrap_or_default().contains("exceeded"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_timeout_bounds_the_retries_too() {
    let dir = scratch("hang-total");
    // Three retries of a 10-minute hang under a 1s ceiling: the ceiling has to
    // cover the whole recovery stack, or a bound each retry resets is not a
    // bound at all.
    let rows = "[[patch]]\nid = \"llm-request-timeout\"\nconfig = { request_secs = 1 }\n\n\
                [[patch]]\nid = \"llm-retry\"\nconfig = { attempts = 3, backoff_ms = 1 }";
    let app = start(tree(&dir, &[rows])).await;
    let _guard = app
        .context()
        .on_waterfall::<AgentRequest>(Arc::new(Hangs), false);

    let started = Instant::now();
    run_turn(&app, "go").await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the ceiling did not cover the retries: {:?}",
        started.elapsed()
    );
}

// ---- truncation ---------------------------------------------------------

/// Answers with a response the provider cut at its output limit.
struct Truncated {
    text: String,
    calls: Vec<atomcode_kernel::tool::ToolCall>,
    seen: Arc<AtomicU32>,
    /// After this many truncated answers, finish normally.
    truncate_times: u32,
}

#[async_trait]
impl Waterfall<AgentRequest> for Truncated {
    async fn handle(
        &self,
        _req: &mut ModelRequest,
        _next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n < self.truncate_times {
            return Ok(ModelResponse {
                text: self.text.clone(),
                tool_calls: self.calls.clone(),
                truncated: true,
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "finished properly".into(),
            ..Default::default()
        })
    }
}

fn call(name: &str, arguments: &str) -> atomcode_kernel::tool::ToolCall {
    atomcode_kernel::tool::ToolCall {
        id: format!("c-{name}"),
        name: name.into(),
        arguments: arguments.into(),
    }
}

#[tokio::test]
async fn a_truncated_answer_gets_a_resume_nudge_as_a_logged_fact() {
    let dir = scratch("truncated-text");
    let app = start(tree(&dir, &[])).await;
    let seen = Arc::new(AtomicU32::new(0));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(Truncated {
            text: "half an ans".into(),
            calls: vec![],
            seen,
            truncate_times: 1,
        }),
        false,
    );

    run_turn(&app, "write something long").await.unwrap();

    let injected: Vec<String> = app
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Injected { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        injected.iter().any(|t| t.contains("Output limit hit")),
        "the nudge must be in the log, not whispered outside it: {injected:?}"
    );
    assert!(
        injected.iter().any(|t| t.contains("INCREMENTALLY")),
        "and it must steer toward incremental writes rather than a re-emit"
    );
}

#[tokio::test]
async fn a_call_cut_mid_arguments_is_refused_rather_than_run() {
    let dir = scratch("truncated-call");
    let target = dir.join("would-be-truncated.txt");
    let app = start(tree(&dir, &[bundle::YOLO])).await;
    let seen = Arc::new(AtomicU32::new(0));
    // The JSON stops mid-value: this is what a `write_file` looks like when the
    // response was cut while its `content` was still streaming. Running it
    // would silently write a truncated file.
    let cut = format!(
        r#"{{"file_path": "{}", "content": "the beginning of som"#,
        target.to_string_lossy()
    );
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(Truncated {
            text: String::new(),
            calls: vec![call("write_file", &cut)],
            seen,
            truncate_times: 1,
        }),
        false,
    );

    run_turn(&app, "write it").await.unwrap();

    assert!(
        !target.exists(),
        "a call whose arguments were cut must not run — a half-written file is worse \
         than no file"
    );
    let transcript = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        transcript.contains("truncated and unsafe to run"),
        "and the model has to learn why, or it will retry the same payload: {transcript}"
    );
    assert!(transcript.contains("split the work"));
}

#[tokio::test]
async fn a_complete_call_in_a_truncated_response_still_runs() {
    let dir = scratch("truncated-mixed");
    std::fs::write(dir.join("a.txt"), "readable").unwrap();
    let app = start(tree(&dir, &[bundle::YOLO])).await;
    let seen = Arc::new(AtomicU32::new(0));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(Truncated {
            text: String::new(),
            calls: vec![
                call("read_file", r#"{"file_path": "a.txt"}"#),
                call(
                    "write_file",
                    r#"{"file_path": "b.txt", "content": "cut off"#,
                ),
            ],
            seen,
            truncate_times: 1,
        }),
        false,
    );

    run_turn(&app, "go").await.unwrap();
    let transcript = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");

    // A round that also contained good calls should not lose them.
    assert!(
        transcript.contains("readable"),
        "the complete call ran: {transcript}"
    );
    assert!(
        transcript.contains("unsafe to run"),
        "and the cut one did not"
    );
    assert!(!dir.join("b.txt").exists());
}

#[tokio::test]
async fn the_nudging_itself_is_bounded() {
    let dir = scratch("truncated-forever");
    let row = "[[patch]]\nid = \"truncation-recovery\"\nconfig = { max_continuations = 2 }";
    let app = start(tree(&dir, &[row])).await;
    let seen = Arc::new(AtomicU32::new(0));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(Truncated {
            text: "always cut".into(),
            calls: vec![],
            seen,
            truncate_times: 99,
        }),
        false,
    );

    run_turn(&app, "go").await.unwrap();
    let nudges = app
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains("Output limit")))
        .count();
    assert!(
        nudges <= 2,
        "a nudge that never works must stop being sent, or it becomes the loop: {nudges}"
    );
}

// ---- partial streams ----------------------------------------------------

/// Breaks mid-stream after producing real output, then succeeds.
struct BreaksMidStream {
    seen: Arc<AtomicU32>,
    break_times: u32,
}

#[async_trait]
impl Waterfall<AgentRequest> for BreaksMidStream {
    async fn handle(
        &self,
        _req: &mut ModelRequest,
        _next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        if n < self.break_times {
            return Err(RequestError {
                retryable: true,
                code: Some(atomcode_kernel::stream::STREAM_TRANSPORT_INTERRUPTED_CODE.into()),
                ..RequestError::message("connection reset mid-stream")
            }
            .with_partial(ModelResponse {
                text: "I had already written this much".into(),
                ..Default::default()
            }));
        }
        Ok(ModelResponse {
            text: "and then finished".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_broken_stream_keeps_what_it_produced() {
    let dir = scratch("partial");
    let app = start(tree(&dir, &[])).await;
    let seen = Arc::new(AtomicU32::new(0));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(BreaksMidStream {
            seen: seen.clone(),
            break_times: 1,
        }),
        false,
    );

    let outcome = run_turn(&app, "write something").await.unwrap();
    assert_eq!(outcome.stop, StopReason::Stopped);

    let transcript = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        transcript.contains("I had already written this much"),
        "the tokens were paid for; discarding them makes the retry redo the work: {transcript}"
    );
    assert!(
        transcript.contains("cut off mid-stream"),
        "and the model has to know why its own message is unfinished"
    );
    assert!(transcript.contains("and then finished"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stream_recovery_is_bounded() {
    let dir = scratch("partial-forever");
    let row = "[[patch]]\nid = \"llm-stream-recovery\"\nconfig = { max_recoveries = 1 }";
    let app = start(tree(&dir, &[row])).await;
    let seen = Arc::new(AtomicU32::new(0));
    let _guard = app.context().on_waterfall::<AgentRequest>(
        Arc::new(BreaksMidStream {
            seen: seen.clone(),
            break_times: 99,
        }),
        false,
    );

    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::ProviderError);
    // A connection that keeps breaking is a signal, not something to absorb
    // indefinitely — the retry row's budget bounds it from there.
    assert!(
        seen.load(Ordering::SeqCst) <= 6,
        "it kept absorbing breaks: {} attempts",
        seen.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_open_failure_has_nothing_to_preserve() {
    let dir = scratch("no-partial");
    let app = start(tree(&dir, &[])).await;
    // A failure with no partial output: the stream never produced anything, so
    // there is nothing to keep and nothing to explain.
    let run = run_with(
        &app,
        1,
        RequestError {
            retryable: true,
            ..RequestError::message("connection refused")
        },
    )
    .await;

    assert_eq!(run.stop, StopReason::Stopped, "the plain retry handles it");
    let injected = app
        .context()
        .only_session()
        .unwrap()
        .events()
        .into_iter()
        .filter(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains("cut off mid-stream")))
        .count();
    assert_eq!(
        injected, 0,
        "no resume nudge for a stream that never started"
    );
}

// The shipping row assembly, using a real provider stream and tool terminal.
struct StreamScript(Arc<atomcode_kernel::testkit::RecordingProvider>);
#[async_trait]
impl atomcode_plexus::Plugin for StreamScript {
    fn name(&self) -> &'static str {
        "recovery-stream-script"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _: &serde_json::Value,
    ) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
fn transport() -> atomcode_kernel::stream::StreamEvent {
    atomcode_kernel::stream::StreamEvent::Error(atomcode_kernel::stream::ProviderError {
        code: Some(atomcode_kernel::stream::STREAM_TRANSPORT_INTERRUPTED_CODE.into()),
        message: "connection reset".into(),
        ..Default::default()
    })
}
fn completed_call(id: &str, name: &str) -> Vec<atomcode_kernel::stream::StreamEvent> {
    use atomcode_kernel::{stream::StreamEvent, tool::ToolCall};
    vec![
        StreamEvent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }),
        StreamEvent::Done { truncated: false },
    ]
}
fn finished() -> Vec<atomcode_kernel::stream::StreamEvent> {
    use atomcode_kernel::stream::StreamEvent;
    vec![
        StreamEvent::TextDelta("finished".into()),
        StreamEvent::Done { truncated: false },
    ]
}
async fn streamed_app(
    scripts: Vec<Vec<atomcode_kernel::stream::StreamEvent>>,
    extra: &[&str],
) -> (
    App,
    Arc<atomcode_kernel::testkit::RecordingProvider>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let dir = scratch("confirmed-stream");
    let provider = Arc::new(atomcode_kernel::testkit::RecordingProvider::new(scripts));
    let mut registry = atomcode_coding::on_harness::catalog();
    registry.register(Arc::new(StreamScript(provider.clone())));
    let mut layers =
        vec!["[[patch]]\nid = \"llm\"\nname = \"recovery-stream-script\"\nconfig = {}"];
    layers.extend_from_slice(extra);
    let mut app = App::new(registry, tree(&dir, &layers));
    app.start().await.unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    app.context()
        .require::<atomcode_harness::seams::ToolsSvc>()
        .unwrap()
        .register(Arc::new(atomcode_kernel::testkit::CountingTool::new(
            count.clone(),
        )))
        .unwrap();
    (app, provider, count)
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shipping_rows_continue_after_confirmed_tools_without_repeating_mutations() {
    use atomcode_kernel::stream::StreamEvent;
    let (app, provider, count) = streamed_app(
        vec![
            completed_call("first", "count"),
            vec![
                StreamEvent::Reasoning("thinking after results".into()),
                transport(),
            ],
            completed_call("repeat", "count"),
            finished(),
        ],
        &[],
    )
    .await;
    let outcome = run_turn(&app, "go").await.unwrap();
    assert_eq!(outcome.stop, StopReason::Stopped);
    assert_eq!(provider.recorded().len(), 4);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(provider.recorded()[3]
        .0
        .iter()
        .any(|m| m.tool_call_id.as_deref() == Some("repeat")
            && m.text.contains("tool not executed again")));
    let log = app.context().only_session().unwrap();
    assert_eq!(
        log.events()
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::ToolStarted { .. }))
            .count(),
        1
    );
    atomcode_harness::session::assert_model_visible_is_logged(&log, &provider.recorded()[3].0)
        .unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shipping_rows_refuse_unexecuted_calls_and_preserve_safe_text() {
    use atomcode_kernel::{stream::StreamEvent, tool::ToolCall};
    for pending in [
        StreamEvent::ToolCall(ToolCall {
            id: "new".into(),
            name: "count".into(),
            arguments: "{}".into(),
        }),
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("new".into()),
            name: Some("count".into()),
            arguments: "{".into(),
        },
    ] {
        let (app, provider, count) = streamed_app(
            vec![
                completed_call("first", "count"),
                vec![
                    StreamEvent::TextDelta("safe partial".into()),
                    pending,
                    transport(),
                ],
                finished(),
            ],
            &[],
        )
        .await;
        assert_eq!(
            run_turn(&app, "go").await.unwrap().stop,
            StopReason::ProviderError
        );
        assert_eq!(provider.recorded().len(), 2);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(app
            .context()
            .only_session()
            .unwrap()
            .derive_messages()
            .iter()
            .any(|m| m.text == "safe partial"));
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shipping_rows_need_actual_results_and_a_typed_transport_error() {
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    for (name, code) in [("missing", true), ("count", false)] {
        let error = if code {
            transport()
        } else {
            StreamEvent::Error(ProviderError {
                message: "logical failure".into(),
                ..Default::default()
            })
        };
        let (app, provider, _) = streamed_app(
            vec![
                completed_call("first", name),
                vec![StreamEvent::Reasoning("thinking".into()), error],
                finished(),
            ],
            &[],
        )
        .await;
        assert_eq!(
            run_turn(&app, "go").await.unwrap().stop,
            StopReason::ProviderError
        );
        assert_eq!(provider.recorded().len(), 2);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shipping_rows_bound_recovery_and_keep_both_partial_replies() {
    use atomcode_kernel::stream::StreamEvent;
    let (app, provider, _) = streamed_app(
        vec![
            vec![StreamEvent::TextDelta("first partial".into()), transport()],
            vec![StreamEvent::TextDelta("second partial".into()), transport()],
            finished(),
        ],
        &[],
    )
    .await;
    assert_eq!(
        run_turn(&app, "go").await.unwrap().stop,
        StopReason::ProviderError
    );
    assert_eq!(provider.recorded().len(), 2);
    let messages = app.context().only_session().unwrap().derive_messages();
    assert!(messages.iter().any(|m| m.text == "first partial"));
    assert!(messages.iter().any(|m| m.text == "second partial"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shipping_recovery_budget_resets_for_each_accepted_turn() {
    use atomcode_kernel::stream::StreamEvent;
    let (app, provider, _) = streamed_app(
        vec![
            vec![StreamEvent::TextDelta("first".into()), transport()],
            finished(),
            vec![StreamEvent::TextDelta("second".into()), transport()],
            finished(),
        ],
        &[],
    )
    .await;
    assert_eq!(
        run_turn(&app, "one").await.unwrap().stop,
        StopReason::Stopped
    );
    assert_eq!(
        run_turn(&app, "two").await.unwrap().stop,
        StopReason::Stopped
    );
    assert_eq!(provider.recorded().len(), 4);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn removing_shipping_recovery_row_removes_tool_continuation() {
    use atomcode_kernel::stream::StreamEvent;
    let (app, provider, count) = streamed_app(
        vec![
            completed_call("first", "count"),
            vec![StreamEvent::Reasoning("thinking".into()), transport()],
            finished(),
        ],
        &["[[remove]]\nid = \"llm-stream-recovery\""],
    )
    .await;
    assert_eq!(
        run_turn(&app, "go").await.unwrap().stop,
        StopReason::ProviderError
    );
    assert_eq!(provider.recorded().len(), 2);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

struct BatchRedaction;
#[async_trait]
impl Waterfall<atomcode_harness::events::ToolsExecuteBatch> for BatchRedaction {
    async fn handle(
        &self,
        batch: &mut atomcode_harness::events::ToolBatch,
        next: Next<'_, atomcode_harness::events::ToolsExecuteBatch>,
    ) -> Vec<atomcode_kernel::tool::ToolResult> {
        let mut results = next.run(batch).await;
        for result in &mut results {
            result.content = result.content.replace("count#1", "[redacted]");
        }
        results
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shipping_reuse_preserves_committed_batch_redaction() {
    use atomcode_kernel::stream::StreamEvent;
    let (app, provider, count) = streamed_app(
        vec![
            completed_call("first", "count"),
            vec![StreamEvent::Reasoning("thinking".into()), transport()],
            completed_call("repeat", "count"),
            finished(),
        ],
        &[],
    )
    .await;
    let _guard = app
        .context()
        .on_waterfall::<atomcode_harness::events::ToolsExecuteBatch>(
            Arc::new(BatchRedaction),
            false,
        );
    assert_eq!(
        run_turn(&app, "go").await.unwrap().stop,
        StopReason::Stopped
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let calls = provider.recorded();
    let receipt = calls[3]
        .0
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("repeat"))
        .unwrap();
    assert!(receipt.text.contains("[redacted]"));
    assert!(!receipt.text.contains("count#1"));
}

struct SyntheticToolSuccess;
#[async_trait]
impl Waterfall<atomcode_harness::events::ToolsExecute> for SyntheticToolSuccess {
    async fn handle(
        &self,
        exec: &mut atomcode_harness::events::ToolExec,
        _: Next<'_, atomcode_harness::events::ToolsExecute>,
    ) -> atomcode_kernel::tool::ToolResult {
        atomcode_kernel::tool::ToolResult {
            call_id: exec.call.id.clone(),
            content: "synthetic success".into(),
            is_error: false,
            images: vec![],
        }
    }
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_synthetic_success_is_not_confirmed_execution() {
    use atomcode_kernel::stream::StreamEvent;
    let (app, provider, count) = streamed_app(
        vec![
            completed_call("first", "count"),
            vec![StreamEvent::Reasoning("thinking".into()), transport()],
            finished(),
        ],
        &[],
    )
    .await;
    let _guard = app
        .context()
        .on_waterfall::<atomcode_harness::events::ToolsExecute>(
            Arc::new(SyntheticToolSuccess),
            false,
        );
    assert_eq!(
        run_turn(&app, "go").await.unwrap().stop,
        StopReason::ProviderError
    );
    assert_eq!(provider.recorded().len(), 2);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shipping_transport_and_idle_failures_share_one_continuation_budget() {
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    let (app, provider, _) = streamed_app(
        vec![
            vec![StreamEvent::TextDelta("first".into()), transport()],
            vec![
                StreamEvent::TextDelta("second".into()),
                StreamEvent::Error(ProviderError {
                    code: Some(atomcode_harness::plugins::agent_loop::STREAM_IDLE_CODE.into()),
                    message: "idle timeout".into(),
                    retryable: true,
                    ..Default::default()
                }),
            ],
            finished(),
        ],
        &[],
    )
    .await;
    assert_eq!(
        run_turn(&app, "go").await.unwrap().stop,
        StopReason::Timeout
    );
    assert_eq!(
        provider.recorded().len(),
        2,
        "the plain retry must not replay partial work"
    );
}
