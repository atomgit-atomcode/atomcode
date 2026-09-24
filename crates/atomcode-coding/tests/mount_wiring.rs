//! What a mount attributes a turn to: telemetry, the datalog, and the session's
//! own cost record.
//!
//! All three follow the model the mount was built for, which is what makes a
//! `/login` or `/model` switch honest — the tree is rebuilt (or patched) with the
//! new config, and everything downstream must report the model the person is now
//! on rather than the one the session started with.

mod support;

use std::path::Path;
use std::sync::Arc;

use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::LlmProvider;
use support::{allow, mount_parts, quiet_options, turn};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// Answers with a little text and a usage report — enough for a metering
/// decorator to fold a `TokenUsage` and emit one `LlmChat`.
struct CannedProvider;

#[async_trait::async_trait]
impl LlmProvider for CannedProvider {
    fn model_name(&self) -> &str {
        "canned"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        use atomcode_kernel::stream::{StreamEvent, TokenUsage};
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("looks good".into()),
            StreamEvent::Usage(TokenUsage {
                prompt: 500,
                completion: 30,
                cached: 0,
            }),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// A session launched before a provider was resolvable has no model name yet.
/// Once `/login` resolves one, the turn must be billed to THAT model — the
/// telemetry envelope is built with the mount, not frozen at prepare.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn telemetry_reports_the_model_the_mount_was_built_for() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let (telemetry, captured) = atomcode_telemetry::Telemetry::in_memory("test".into());
    let project = tempfile::tempdir().unwrap();
    // Onboarding: no resolvable provider, so no model name at prepare.
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "", project.path());
    cfg.telemetry = Some(telemetry);
    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();

    // `/login` resolves the real provider: the config picks up the model and the
    // tree is mounted with it.
    cfg.model = "swapped-model".to_string();
    let mut mounted = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    let _ = turn(&mut mounted.handle, "hi", allow()).await;
    mounted.shutdown().await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let records = captured.lock().await;
    let chat = records
        .iter()
        .find(|r| matches!(r.event, atomcode_telemetry::Event::LlmChat { .. }))
        .expect("the turn must emit one LlmChat");
    assert_eq!(
        chat.envelope.model.as_deref(),
        Some("swapped-model"),
        "the model active at mount, not the one prepare saw"
    );
}

/// With `[datalog]` on, the turn is written where the person pointed it.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn the_configured_datalog_records_the_turn() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let datalog_root = home.path().join("custom-datalog");
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "logged-model", project.path());
    cfg.datalog.enabled = true;
    cfg.datalog.dir = Some(datalog_root.display().to_string());

    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();
    let mut mounted = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    let _ = turn(&mut mounted.handle, "record this turn", allow()).await;
    mounted.shutdown().await;

    let project_dir = std::fs::read_dir(&datalog_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let files: Vec<_> = std::fs::read_dir(project_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let markdown = files
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
        .expect("turn markdown");
    // `.cas.jsonl` shares the `jsonl` extension, so match on the full name.
    let name_ends = |path: &std::path::PathBuf, suffix: &str| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(suffix))
    };
    let jsonl = files
        .iter()
        .find(|p| name_ends(p, ".jsonl") && !name_ends(p, ".cas.jsonl"))
        .expect("per-round request jsonl");
    let cas = files
        .iter()
        .find(|p| name_ends(p, ".cas.jsonl"))
        .expect("content-addressed store");
    assert!(std::fs::read_to_string(markdown)
        .unwrap()
        .contains("**Response:**\nlooks good"));
    let request = std::fs::read_to_string(jsonl).unwrap();
    assert!(request.contains("\"model\":\"logged-model\""));
    // The record references message bodies by hash; the prompt text lives once in
    // the cas store. Rehydrate to confirm the full request is recoverable.
    let record: serde_json::Value = serde_json::from_str(request.lines().next().unwrap()).unwrap();
    let index =
        atomcode_capabilities::datalog::build_cas_index(&std::fs::read_to_string(cas).unwrap());
    let full = atomcode_capabilities::datalog::rehydrate_record(&record, &index);
    assert!(full["messages"].to_string().contains("record this turn"));
}

/// A session that switches models keeps both models' spend, each under its own
/// name — the session's cost record is what `/cost` and the catalog read.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn a_sessions_cost_is_recorded_per_model_across_a_switch() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "model-a", project.path());
    cfg.provider_name = "provider-a".into();

    let opts = PrepareOptions {
        session: SessionMode::Fresh,
        ..quiet_options()
    };
    let mut parts = prepare(&cfg, opts.clone()).await.unwrap();
    let binding = parts.session.as_ref().unwrap();
    let manager = binding.manager.clone();
    let session_id = binding.id.clone();

    let mut first = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    parts.publish_staged_session().unwrap();
    let _ = turn(&mut first.handle, "first", allow()).await;
    first.shutdown().await;

    cfg.provider_name = "provider-b".into();
    cfg.model = "model-b".into();
    let mut second = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    let _ = turn(&mut second.handle, "second", allow()).await;
    second.shutdown().await;

    let report = atomcode_capabilities::session::aggregate_session_cost(
        &manager.read_meta(&session_id).unwrap(),
    );
    assert_eq!(report.models.len(), 2, "{report:?}");
    assert_eq!(report.models[0].provider_id, "provider-a");
    assert_eq!(report.models[0].model_id, "model-a");
    assert_eq!(report.models[1].provider_id, "provider-b");
    assert_eq!(report.models[1].model_id, "model-b");
}

/// A credential read from `config.toml` reaches what it configures without
/// entering the config tree, and nothing the agent can ask about itself says it.
///
/// A config tree is printable data — `--dump-config` renders every row's config
/// verbatim — so a key carried as row config is a key on somebody's screen. The
/// judge is the printed tree and every `describe_self` aspect, with a sentinel
/// standing in for the key (never a real one).
/// `[ui] ai_session_naming` 换的是**哪一个命名器挂上**,不是另起一个。
///
/// 这个开关此前的全部效果是:运行时在回合末**自己再问一次模型**要名字,答案
/// 走 `SessionNameSuggested` 事件 —— 而默认屏幕根本没接那条事件。于是它默认
/// 打开的情况下,每个会话白烧一次模型请求,屏幕上一个字都不会变。
///
/// 现在它换的是填 `session-title` 那条缝的行。答案照旧作为一条 `Titled` 事实
/// 提交,所以会话目录、`/resume` 列表和窗口标题读到的是同一个名字 —— 一件事
/// 一个 owner。
///
/// **两半都要钉**:只钉「开了就挂模型命名器」的话,把这次 swap 写成无条件
/// 照样全绿 —— 而那样连关掉这个开关的人也要为每个会话多付一次模型请求。
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn ai_session_naming_picks_the_namer_rather_than_starting_a_second_one() {
    // 这个开关也认环境变量,而它会盖掉配置里的值。
    std::env::remove_var("ATOMCODE_AI_SESSION_NAMING");
    let project = tempfile::tempdir().unwrap();

    let named_by = |on: bool| {
        let mut file = atomcode_config::config::Config::default();
        file.ui.ai_session_naming = on;
        let mut cfg = atomcode_coding::CodingRuntimeConfig::from_config(
            &file,
            project.path(),
            None,
            None,
            false,
            false,
        )
        .agent_config();
        // 运行时是由 `install_subagent_tiers` 挂上这一份的;这里直接给,因为
        // 被测的是「读到的那份配置怎么改这棵树」,不是谁把它放上去的。
        cfg.subagent_config = Some(std::sync::Arc::new(file));
        cfg
    };

    let off = support::mount(&named_by(false), quiet_options(), Arc::new(CannedProvider))
        .await
        .dump();
    // 缝的 id 两边都叫 `session-title-first-prompt`;真正换掉的是填它的那个行
    // 的名字,dump 里写成 `id <- name`。
    assert!(
        off.contains("session-title-first-prompt <- session-title-first-prompt"),
        "关掉的时候读第一句话就够了,不问模型:\n{off}"
    );
    assert!(
        !off.contains("session-title-model"),
        "而且不许悄悄挂上问模型的那个:\n{off}"
    );

    let on = support::mount(&named_by(true), quiet_options(), Arc::new(CannedProvider))
        .await
        .dump();
    assert!(
        on.contains("session-title-first-prompt <- session-title-model"),
        "开着的时候是模型来起名字:\n{on}"
    );
    assert!(
        !on.contains("session-title-first-prompt <- session-title-first-prompt"),
        "一条缝一个命名器,不是两个都挂上:\n{on}"
    );
}

#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn a_configured_credential_never_enters_the_config_tree() {
    const SENTINEL: &str = "sentinel-web-search-key-for-this-test";
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    std::env::remove_var("EXA_API_KEY");
    let project = tempfile::tempdir().unwrap();
    let mut file = atomcode_config::config::Config::default();
    file.web_search.api_key = Some(SENTINEL.into());
    let cfg = atomcode_coding::CodingRuntimeConfig::from_config(
        &file,
        project.path(),
        None,
        None,
        false,
        false,
    )
    .agent_config();
    assert_eq!(
        cfg.web_search_api_key.as_deref(),
        Some(SENTINEL),
        "the key was read, so the absences below are not passing by absence"
    );

    let opts = PrepareOptions {
        web: true,
        ..quiet_options()
    };
    let mounted = support::mount(&cfg, opts, Arc::new(CannedProvider)).await;
    let dump = mounted.dump();
    assert!(
        dump.contains("tool-web-keyed"),
        "the keyed web row is what mounted:\n{dump}"
    );
    assert!(
        !dump.contains(SENTINEL),
        "the key is in the printable config tree"
    );

    let describe = mounted
        .context()
        .service::<atomcode_harness::seams::ToolsSvc>()
        .unwrap()
        .get("describe_self")
        .expect("describe_self is mounted");
    let ctx = atomcode_kernel::tool::ToolContext {
        working_dir: project.path().to_path_buf(),
        cancel: Default::default(),
        progress: atomcode_kernel::tool::ProgressSink::noop(),
        requester: None,
    };
    for aspect in [
        "all",
        "session",
        "services",
        "tools",
        "models",
        "operations",
        "settings",
    ] {
        let said = describe
            .execute(&format!(r#"{{"aspect":"{aspect}"}}"#), &ctx)
            .await;
        assert!(
            !said.content.contains(SENTINEL),
            "`describe_self` aspect `{aspect}` says the key"
        );
    }
    mounted.shutdown().await;
}

/// A checkout a person stepped into bounds **changes**, not reading.
///
/// `/worktree` is already a deliberate step into a tree of one's own, so a
/// mutation outside that tree is refused whether or not anyone is there to ask.
/// The other half is the point of the shape: reads stay open, because a
/// dependency cache, a sibling repository and `~` are things a checkout has to
/// be able to look at — a fence on the read side would refuse the lookup long
/// before anyone could be asked about the change.
///
/// Read here through the mounted `fs` service rather than through a tool call,
/// because this is a property of the world the mount built: it holds whatever
/// path a tool hands over, so it is the boundary itself being asserted.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn a_checkout_bounds_the_write_and_leaves_the_read_alone() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());

    // A real repository: `.git` as a FILE is what a linked worktree has, and
    // that is the fact the mount reads to decide it is one.
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.path().join("a.txt"), "a").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "first"]);
    let checkout = repo.path().join("checkout");
    git(&[
        "worktree",
        "add",
        "-b",
        "here",
        checkout.to_str().expect("path"),
    ]);
    assert!(
        checkout.join(".git").is_file(),
        "the fixture is a linked worktree, which is what the rule keys on"
    );

    // Outside the checkout. NOT under the temp dir: that is deliberately still
    // writable, so a sample there would pass while proving the opposite.
    let sibling = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/checkout-fence")
        .join(format!("{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sibling);
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("seen.txt"), "readable").unwrap();

    let cfg = atomcode_coding::CodingRuntimeConfig::from_config(
        &atomcode_config::config::Config::default(),
        &checkout,
        None,
        None,
        false,
        true,
    )
    .agent_config();
    let opts = PrepareOptions {
        tools: true,
        ..quiet_options()
    };
    let mounted = support::mount(&cfg, opts, Arc::new(CannedProvider)).await;
    let world = mounted
        .context()
        .service::<atomcode_harness::seams::FsSvc>()
        .expect("an fs world is mounted");
    let _ = std::fs::create_dir_all(checkout.join("src"));

    // Writes inside the checkout land.
    world
        .write_text(Path::new("src/made.txt"), "mine")
        .await
        .expect("a write inside the checkout");

    // Reads outside it still work — the half that makes this livable.
    assert_eq!(
        world
            .read_text(&sibling.join("seen.txt"))
            .await
            .expect("reading outside the checkout"),
        "readable"
    );

    // And a change outside it is refused, with the file untouched.
    for target in [sibling.join("seen.txt"), sibling.join("new.txt")] {
        let refused = world.write_text(&target, "changed").await;
        assert!(
            refused.as_ref().is_err_and(|e| e.is_denied()),
            "{target:?} must be refused: {refused:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(sibling.join("seen.txt")).unwrap(),
        "readable",
        "the refused write changed nothing"
    );
    assert!(!sibling.join("new.txt").exists());

    mounted.shutdown().await;
    let _ = std::fs::remove_dir_all(&sibling);
}

/// `[tools.output] threshold_bytes` reaches the row that cuts oversized tool
/// results, beside the directory it already had; unset, the row is as written.
///
/// A deployment an editor extension starts cannot always be given an
/// environment, so the file has to reach the row on its own. A wholesale
/// `[[patch]]` that dropped `dir` would stop the row mounting at all, so both
/// fields are checked.
#[tokio::test]
async fn a_configured_output_threshold_reaches_the_row_that_cuts() {
    let project = tempfile::tempdir().unwrap();
    let row_of = |mounted: &support::Mounted| {
        mounted
            .row_configs()
            .into_iter()
            .find(|(id, _)| id == "tool-output-artifact")
            .expect("the row that spills tool output is mounted")
            .1
    };
    let dir = project.path().join(".atomcode").join("artifacts");
    let cfg_from = |file: &atomcode_config::config::Config| {
        atomcode_coding::CodingRuntimeConfig::from_config(
            file,
            project.path(),
            None,
            None,
            false,
            false,
        )
        .agent_config()
    };

    let mut file = atomcode_config::config::Config::default();
    file.tools.output.threshold_bytes = Some(204_800);
    let mounted = support::mount(&cfg_from(&file), quiet_options(), Arc::new(CannedProvider)).await;
    let row = row_of(&mounted);
    assert_eq!(row["threshold_bytes"], 204_800, "{row}");
    assert_eq!(row["dir"], dir.to_string_lossy().as_ref(), "{row}");
    mounted.stop();

    let unset = atomcode_config::config::Config::default();
    let mounted =
        support::mount(&cfg_from(&unset), quiet_options(), Arc::new(CannedProvider)).await;
    let row = row_of(&mounted);
    assert!(row.get("threshold_bytes").is_none(), "{row}");
    assert_eq!(row["dir"], dir.to_string_lossy().as_ref(), "{row}");
    mounted.stop();
}
