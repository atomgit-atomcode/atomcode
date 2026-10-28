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

#[tokio::test]
async fn the_runtime_mounts_distinct_first_content_and_idle_budgets() {
    let project = tempfile::tempdir().unwrap();
    let mut cfg = atomcode_coding::CodingAgentConfig::new(
        "placeholder",
        "http://localhost",
        "timed",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
    cfg.first_token_timeout = std::time::Duration::from_secs(600);
    cfg.stream_timeout = std::time::Duration::from_secs(120);
    let dump = support::mount(&cfg, quiet_options(), Arc::new(CannedProvider))
        .await
        .dump();
    assert!(
        dump.contains("\"stream_first_content_ms\": 600000"),
        "{dump}"
    );
    assert!(dump.contains("\"stream_idle_ms\": 120000"), "{dump}");
}

#[tokio::test]
async fn the_configured_absolute_compaction_budget_reaches_the_existing_strategy_row() {
    let project = tempfile::tempdir().unwrap();
    let mut file: atomcode_config::config::Config =
        toml::from_str("[compaction]\ntoken_limit=200000").unwrap();
    let runtime = atomcode_coding::CodingRuntimeConfig::from_config(
        &file,
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
        None,
        None,
        false,
        false,
    );
    let cfg = runtime.agent_config();
    assert_eq!(cfg.compact_token_limit, Some(200_000));
    let mounted = support::mount(&cfg, quiet_options(), Arc::new(CannedProvider)).await;
    assert!(mounted.dump().contains("\"token_limit\": 200000"));
    mounted.shutdown().await;
    file.compaction.token_limit = None;
    let runtime = atomcode_coding::CodingRuntimeConfig::from_config(
        &file,
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
        None,
        None,
        false,
        false,
    );
    assert!(runtime.agent_config().compact_token_limit.is_none());
}

/// Answers with a little text and a usage report — enough for a metering
/// decorator to fold a `TokenUsage` and emit one `LlmChat`.
struct CannedProvider;

#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn team_capacity_is_not_the_task_concurrency_limit() {
    let project = tempfile::tempdir().unwrap();
    let mut file = atomcode_config::config::Config::default();
    file.subagent.max_concurrent = 1;
    file.team.max_members = 6;
    let mut cfg = atomcode_coding::CodingRuntimeConfig::from_config(
        &file,
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
        None,
        None,
        false,
        false,
    )
    .agent_config();
    cfg.subagent_config = Some(Arc::new(file));
    let mut opts = quiet_options();
    opts.subagents = atomcode_coding::SubagentPolicy::Enabled;

    let dump = support::mount(&cfg, opts, Arc::new(CannedProvider))
        .await
        .dump();

    assert!(
        dump.contains("team-in-process") && dump.contains("\"max_members\": 6"),
        "the team row must receive its own capacity:\n{dump}"
    );
    assert!(
        dump.contains("subagent-in-process") && dump.contains("\"max_concurrent\": 1"),
        "the task row must receive the execution limit:\n{dump}"
    );
}

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
    let mut cfg = CodingAgentConfig::new(
        "k",
        "http://localhost",
        "",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
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
    let mut cfg = CodingAgentConfig::new(
        "k",
        "http://localhost",
        "logged-model",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
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
    let mut cfg = CodingAgentConfig::new(
        "k",
        "http://localhost",
        "model-a",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
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
            atomcode_coding::config::product_dirs_from_env(),
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
    file.lsp.enabled = true;
    file.lsp.servers.insert(
        "rs".into(),
        atomcode_config::lsp_registry::LspServerConfig {
            command: "unused-lsp-credential-test".into(),
            args: vec![],
            root_markers: vec![],
            options: atomcode_config::lsp_registry::LspOptions {
                init_options: Some(serde_json::json!({"token":SENTINEL})),
                ..Default::default()
            },
        },
    );
    let cfg = atomcode_coding::CodingRuntimeConfig::from_config(
        &file,
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
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
    assert!(
        mounted
            .context()
            .service::<atomcode_harness::seams::ToolsSvc>()
            .unwrap()
            .get("lsp")
            .is_some(),
        "the credential-bearing LSP instance really mounted"
    );
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
        atomcode_coding::config::product_dirs_from_env(),
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
            atomcode_coding::config::product_dirs_from_env(),
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

#[tokio::test]
async fn search_defaults_reach_the_shipping_tools_and_project_overrides_are_scoped() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let dirs = atomcode_coding::config::product_dirs_from_env();
    std::fs::create_dir_all(dirs.project(project.path())).unwrap();
    std::fs::write(
        dirs.project(project.path()).join("search.toml"),
        "respect_gitignore=false\nfollow_symlinks=true\n",
    )
    .unwrap();
    let mut file = atomcode_config::Config::default();
    file.tools.search.include_hidden = true;
    let mut cfg = atomcode_coding::CodingRuntimeConfig::from_config(
        &file,
        project.path(),
        dirs.clone(),
        None,
        None,
        false,
        true,
    )
    .agent_config();
    assert!(cfg.tool_search.include_hidden);
    #[cfg(unix)]
    let linked = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::fs::write(linked.path().join("linked.txt"), "needle").unwrap();
    for root in [project.path(), elsewhere.path()] {
        #[cfg(unix)]
        std::os::unix::fs::symlink(linked.path(), root.join("linked")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join(".hidden/hidden.txt"), "needle").unwrap();
        std::fs::write(root.join("ignored.txt"), "needle").unwrap();
    }
    for (root, includes_ignored) in [(project.path(), true), (elsewhere.path(), false)] {
        cfg.working_dir = root.into();
        let mounted = support::mount(&cfg, quiet_options(), Arc::new(CannedProvider)).await;
        let tools = mounted
            .context()
            .service::<atomcode_harness::seams::ToolsSvc>()
            .unwrap();
        let ctx = atomcode_kernel::tool::ToolContext {
            working_dir: root.into(),
            cancel: Default::default(),
            progress: Default::default(),
            requester: None,
        };
        for (name, args) in [
            ("glob", r#"{"pattern":"**/*.txt"}"#),
            ("grep", r#"{"pattern":"needle","context":0}"#),
        ] {
            let tool = tools.get(name).unwrap();
            let result = tool.execute(args, &ctx).await;
            assert!(!result.is_error, "{}", result.content);
            assert!(
                result.content.contains("hidden.txt"),
                "{name}: {}",
                result.content
            );
            assert_eq!(
                result.content.contains("ignored.txt"),
                includes_ignored,
                "{name}: {}",
                result.content
            );
            #[cfg(unix)]
            assert_eq!(
                result.content.contains("linked.txt"),
                includes_ignored,
                "{name}: follow_symlinks must use the current project's setting: {}",
                result.content
            );
            if !includes_ignored {
                let args = if name == "glob" {
                    r#"{"pattern":"**/*.txt","respect_gitignore":false}"#
                } else {
                    r#"{"pattern":"needle","respect_gitignore":false}"#
                };
                let result = tool.execute(args, &ctx).await;
                assert!(result.content.contains("ignored.txt"), "{}", result.content);
            }
        }
        mounted.shutdown().await;
    }
}

/// A real edit retains the driver's diff while the next request and replay use
/// the same short receipt; repeated whole-file reads retire the stale version.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn recent_context_cleanup_keeps_the_ui_diff_and_replays_the_model_view() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    struct Editor {
        step: AtomicUsize,
        seen: Mutex<Vec<Vec<Message>>>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for Editor {
        fn model_name(&self) -> &str {
            "context-cleanup"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _: &[atomcode_kernel::tool::ToolDef],
            _: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            use atomcode_kernel::{stream::StreamEvent, tool::ToolCall};
            self.seen.lock().unwrap().push(messages.to_vec());
            let step = self.step.fetch_add(1, Ordering::SeqCst);
            let call = match step {
                0 | 2 => Some(("read_file", serde_json::json!({"file_path":"a.txt"}))),
                1 => Some((
                    "edit_file",
                    serde_json::json!({"file_path":"a.txt","old_string":"original","new_string":"updated"}),
                )),
                _ => None,
            };
            let mut events = Vec::new();
            if let Some((name, args)) = call {
                events.push(StreamEvent::ToolCall(ToolCall {
                    id: format!("cleanup-{step}"),
                    name: name.into(),
                    arguments: args.to_string(),
                }));
            } else {
                events.push(StreamEvent::TextDelta("done".into()));
            }
            events.push(StreamEvent::Done { truncated: false });
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("a.txt"),
        format!("original {}\n", "kept ".repeat(30)),
    )
    .unwrap();
    let cfg = CodingAgentConfig::new(
        "k",
        "http://localhost",
        "context-cleanup",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
    let provider = Arc::new(Editor {
        step: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let mut mounted = support::mount(
        &cfg,
        PrepareOptions {
            tools: true,
            ..quiet_options()
        },
        provider.clone(),
    )
    .await;
    let turn = support::turn(&mut mounted.handle, "edit then read", support::allow()).await;
    assert!(turn.error.is_none(), "{turn:?}");
    let edit = turn
        .tool_results
        .iter()
        .find(|r| r.call_id == "cleanup-1")
        .expect("edit result");
    assert!(!edit.is_error, "{}", edit.content);
    assert!(
        edit.content.contains("@@ "),
        "the driver keeps the actual diff"
    );
    let seen = provider.seen.lock().unwrap();
    let last = seen.last().unwrap();
    let receipt = last
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("cleanup-1"))
        .unwrap();
    assert!(
        receipt.text.starts_with("Edited ") && !receipt.text.contains("@@ "),
        "{}",
        receipt.text
    );
    assert!(last
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("cleanup-0"))
        .unwrap()
        .text
        .contains("Superseded"));
    let receipt_text = receipt.text.clone();
    drop(seen);
    let log = mounted
        .context()
        .service::<atomcode_harness::seams::AgentsSvc>()
        .unwrap()
        .list()[0]
        .session();
    let events = log.events();
    let decoded: Vec<_> = events
        .iter()
        .map(|logged| atomcode_kernel::session::LoggedEvent {
            seq: logged.seq,
            at: logged.at,
            event: serde_json::from_str(&serde_json::to_string(&logged.event).unwrap()).unwrap(),
        })
        .collect();
    let records = atomcode_capabilities::session::events::turn_records(log.id(), &decoded);
    assert!(
        records
            .iter()
            .flat_map(|turn| &turn.tools)
            .any(|tool| tool.name == "edit_file" && tool.result.contains("@@ ")),
        "presentation readers retain the original diff after replay"
    );
    let replay = atomcode_kernel::session::derive_messages(&decoded);
    assert_eq!(
        replay
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("cleanup-1"))
            .unwrap()
            .text,
        receipt_text
    );
    mounted.shutdown().await;
}

/// The production mount must project schemas, rather than merely test a helper.
#[cfg(feature = "atomgit")]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[serial_test::serial(atomgit_env)]
async fn atomgit_discovery_loads_full_schemas_without_changing_the_executable_catalog() {
    use atomcode_kernel::{
        stream::StreamEvent,
        tool::{ToolCall, ToolDef},
    };
    use std::sync::Mutex;
    struct Recorder(Mutex<Vec<Vec<ToolDef>>>);
    #[async_trait::async_trait]
    impl LlmProvider for Recorder {
        fn model_name(&self) -> &str {
            "discovery-test"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            tools: &[ToolDef],
            _: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            let mut requests = self.0.lock().unwrap();
            requests.push(tools.to_vec());
            let event = if requests.len() == 1 && tools.iter().any(|t| t.name == "builtin_search") {
                StreamEvent::ToolCall(ToolCall {
                    id: "discover".into(),
                    name: "builtin_search".into(),
                    arguments: r#"{"query":"atomgit_issue","limit":1}"#.into(),
                })
            } else {
                StreamEvent::TextDelta("done".into())
            };
            Ok(Box::pin(futures::stream::iter(vec![
                event,
                StreamEvent::Done { truncated: false },
            ])))
        }
    }
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new(
        "k",
        "http://localhost",
        "discovery-test",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
    let mut eager: Vec<ToolDef> = Vec::new();
    let mut lazy: Vec<ToolDef> = Vec::new();
    for enabled in [false, true] {
        let mut file = atomcode_config::config::Config::default();
        file.tools.atomgit.tool_search = enabled;
        let runtime = atomcode_coding::CodingRuntimeConfig::from_config(
            &file,
            project.path(),
            cfg.dirs.clone(),
            None,
            None,
            false,
            false,
        );
        assert_eq!(runtime.agent_config().atomgit_tool_search, enabled);
        cfg.atomgit_tool_search = enabled;
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let mut mounted = support::mount(&cfg, quiet_options(), recorder.clone()).await;
        let catalog = mounted
            .context()
            .service::<atomcode_harness::seams::ToolsSvc>()
            .unwrap()
            .defs();
        for name in ["atomgit_repo", "atomgit_issue", "atomgit_pr", "atomgit_api"] {
            assert!(catalog.iter().any(|t| t.name == name));
        }
        turn(&mut mounted.handle, "inspect issue capabilities", allow()).await;
        let requests = recorder.0.lock().unwrap().clone();
        if enabled {
            assert!(requests.len() >= 2);
            assert!(!requests[0].iter().any(|t| t.name.starts_with("atomgit_")));
            assert!(requests[0]
                .iter()
                .any(|t| t.name == "builtin_search" && t.description.contains("atomgit_issue")));
            let original = eager.iter().find(|t| t.name == "atomgit_issue").unwrap();
            assert!(
                requests[1].contains(original),
                "search must expose the same complete schema as eager loading"
            );
            assert_eq!(
                requests[1]
                    .iter()
                    .filter(|t| t.name.starts_with("atomgit_"))
                    .count(),
                1
            );
            lazy = requests[0].clone();
        } else {
            assert!(!requests[0].iter().any(|t| t.name == "builtin_search"));
            assert_eq!(
                requests[0]
                    .iter()
                    .filter(|t| t.name.starts_with("atomgit_"))
                    .count(),
                4
            );
            eager = requests[0].clone();
        }
        mounted.shutdown().await;
    }
    let eager_bytes = serde_json::to_vec(&eager).unwrap().len();
    let lazy_bytes = serde_json::to_vec(&lazy).unwrap().len();
    eprintln!(
        "production builtin tool JSON bytes: eager={eager_bytes}, lazy={lazy_bytes}, saved={}",
        eager_bytes - lazy_bytes
    );
    assert!(lazy_bytes < eager_bytes);
    let unaffected = |tools: Vec<ToolDef>| {
        tools
            .into_iter()
            .filter(|t| !t.name.starts_with("atomgit_") && t.name != "builtin_search")
            .collect::<Vec<_>>()
    };
    assert_eq!(unaffected(eager), unaffected(lazy));
}

#[cfg(feature = "atomgit")]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[serial_test::serial(atomgit_env)]
async fn atomgit_discovery_follows_the_admitted_catalog_when_env_overrides_disabled_config() {
    struct RestoreEnv(Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("ATOMCODE_ATOMGIT", value),
                None => std::env::remove_var("ATOMCODE_ATOMGIT"),
            }
        }
    }
    let _restore = RestoreEnv(std::env::var_os("ATOMCODE_ATOMGIT"));
    std::env::set_var("ATOMCODE_ATOMGIT", "1");
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new(
        "k",
        "http://localhost",
        "canned",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
    cfg.atomgit_enabled = false;
    let mounted = support::mount(&cfg, quiet_options(), Arc::new(CannedProvider)).await;
    let catalog = mounted
        .context()
        .service::<atomcode_harness::seams::ToolsSvc>()
        .unwrap()
        .defs();
    assert!(
        catalog.iter().any(|tool| tool.name == "atomgit_repo"),
        "the environment enables registration"
    );
    assert!(
        catalog.iter().any(|tool| tool.name == "builtin_search"),
        "discovery must follow the actually admitted tools, not a stale configuration flag"
    );
    mounted.shutdown().await;
}

#[tokio::test]
async fn host_automation_tools_mount_on_the_lead_without_widening_delegation() {
    struct AutomationBackend;
    impl atomcode_capabilities::tools::automation::AutomationService for AutomationBackend {
        fn call(
            &self,
            _: &str,
            _: serde_json::Value,
            _: &Path,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({"tasks":[]}))
        }
    }
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new(
        "placeholder",
        "http://localhost",
        "test",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
    cfg.automation = Some(Arc::new(AutomationBackend));
    let mut options = quiet_options();
    options.tools = true;
    options.subagents = atomcode_coding::SubagentPolicy::Enabled;
    let mounted = support::mount(&cfg, options, Arc::new(CannedProvider)).await;
    let tools = mounted
        .context()
        .service::<atomcode_harness::seams::ToolsSvc>()
        .unwrap();
    for name in [
        "schedule_create",
        "schedule_list",
        "schedule_update",
        "schedule_delete",
    ] {
        assert!(tools.get(name).is_some(), "lead tool {name} is missing");
    }
    let delegated = mounted
        .context()
        .service::<atomcode_harness::seams::SubagentsSvc>()
        .unwrap();
    assert!(
        !delegated.describe().contains("schedule_"),
        "{}",
        delegated.describe()
    );
    mounted.shutdown().await;
}
