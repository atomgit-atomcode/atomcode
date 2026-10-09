//! `atomcode --tui` and the sessions the product writes are one store: a
//! session written through the product runtime comes back on the row-assembled
//! screen with its history drawn (`docs/tui-replaces-tuix-plan.md` M2).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atomcode::tui_front;
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeEvent,
    CodingRuntimeStart, PrepareOptions, ProviderBuildError, SessionMode, StaticPluginHookSource,
    SubagentPolicy, UserInput,
};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::ToolDef;
use atomcode_tui::launch::Screen;

/// How many requests, and what the last one carried.
#[derive(Default)]
struct Count(AtomicUsize, std::sync::Mutex<Vec<Message>>);

/// `answer N`, for every request.
struct Scripted(Arc<Count>);

#[async_trait::async_trait]
impl LlmProvider for Scripted {
    fn model_name(&self) -> &str {
        "scripted"
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        *self.0 .1.lock().unwrap() = messages.to_vec();
        let n = self.0 .0.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta(format!("answer {n}")),
            StreamEvent::Usage(TokenUsage {
                prompt: 10,
                completion: 2,
                cached: 0,
            }),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// The first request answers a little and then never finishes, so a turn
/// stays in flight for as long as a test needs it to.
struct HangsFirst(Arc<Count>);

#[async_trait::async_trait]
impl LlmProvider for HangsFirst {
    fn model_name(&self) -> &str {
        "scripted"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        use futures::StreamExt;
        self.0 .0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(
            futures::stream::iter(vec![StreamEvent::TextDelta("正在思考中".into())])
                .chain(futures::stream::pending()),
        ))
    }
}

struct HangingFactory(Arc<Count>);

impl CodingProviderFactory for HangingFactory {
    fn build(
        &self,
        _config: &CodingAgentConfig,
        _session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(Arc::new(HangsFirst(self.0.clone())))
    }
}

struct Factory(Arc<Count>);

impl CodingProviderFactory for Factory {
    fn build(
        &self,
        _config: &CodingAgentConfig,
        _session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(Arc::new(Scripted(self.0.clone())))
    }
}

fn start(
    project: &std::path::Path,
    count: &Arc<Count>,
    session: SessionMode,
    front_end: Option<Arc<FrontEnd>>,
) -> (CodingRuntimeStart, CodingAgentConfig) {
    let mut agent = CodingAgentConfig::new(
        "key",
        "https://example.test/v1",
        "scripted",
        project,
        atomcode_coding::config::product_dirs_from_env(),
    );
    agent.interactive = true;
    (
        CodingRuntimeStart {
            agent: agent.clone(),
            prepare: PrepareOptions {
                request_user_input: true,
                session,
                tools: true,
                skill_dirs: Some(Vec::new()),
                plugin_skill_dirs: Vec::new(),
                mcp: false,
                extra_mcp_servers: Vec::new(),
                mcp_user_config: None,
                mcp_tokens: None,
                external_subagents: Vec::new(),
                memory: false,
                web: false,
                review: false,
                subagents: SubagentPolicy::Disabled,
                rate_limit_source: None,
                front_end,
                review_delegate: None,
                host_plugins: Default::default(),
                identity: Default::default(),
            },
            provider_factory: Arc::new(Factory(count.clone())),
            plugin_hooks: Arc::new(StaticPluginHookSource::default()),
            image_preprocessor: None,
        },
        agent,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_the_product_wrote_comes_back_on_the_row_assembled_screen() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());

    // Written the way the product writes it: a runtime and its own events.
    let (first, _) = start(project.path(), &count, SessionMode::Fresh, None);
    let mut first = CodingRuntime::start(first).await.expect("starts");
    let id = first.session.clone().expect("a stored session").id;
    first
        .handle
        .submit(UserInput::from("remember pineapple"))
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), first.events.recv())
            .await
            .expect("the turn ends")
            .expect("the runtime is running");
        if matches!(event.event, CodingRuntimeEvent::TurnFinished(_)) {
            break;
        }
    }
    first.handle.shutdown().await.unwrap();
    let _ = first.task.await;

    // Resumed behind the screen `atomcode --tui` runs.
    let front_end = FrontEnd::new();
    let (second, config) = start(
        project.path(),
        &count,
        SessionMode::Resume(id),
        Some(front_end.clone()),
    );
    let second = CodingRuntime::start(second).await.expect("resumes");
    let screen = Screen {
        headless: Some((100, 30)),
        ..Screen::default()
    };
    // The settings the screen shows come from this file. Passed explicitly
    // rather than resolved inside, because "which config" is the launcher's
    // decision — the same one the binary makes from `--config`.
    let config_path = home.path().join("config.toml");
    // `None` for the host configuration: this criterion is about mounting the
    // screen, and a host that resolves nothing is the honest stand-in.
    let mounted = tui_front::mount(
        second,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");

    // A machine with no provider is told where to go (`/login`, `/provider`)
    // and nothing is opened for it: the first-run wizard is not started on its
    // own. It is still mounted, and `/onboarding` still runs it.
    {
        let commands = mounted
            .app
            .context()
            .service::<atomcode_tui::plugin::CommandsSvc>()
            .expect("the screen provides its commands");
        match atomcode::host::readiness_for(Some(
            atomcode_coding::ProviderUnavailableReason::NotConfigured,
        )) {
            atomcode_host_api::HostReply::Readiness {
                fix: None,
                why: Some(why),
                ..
            } => {
                for named in ["login", "provider"] {
                    assert!(why.contains(&format!("/{named}")), "{why}");
                    assert!(
                        commands.find(named).is_some(),
                        "the sentence names /{named}, which this screen runs"
                    );
                }
            }
            other => panic!("a machine with no provider: {other:?}"),
        }
        assert!(
            commands.find(atomcode::tui_onboarding::COMMAND).is_some(),
            "the walkthrough is still there to type"
        );
    }
    // **`/login` is the launcher's, not the screen's shipped one.**
    //
    // The shipped one only re-read configuration, so a person who had just run
    // `/logout` got "AtomGit gateway requires login — run `/login`" from the
    // very command they ran. Mounting is where that is decided: a row that
    // declares the override and mounts after the set it takes the name from is
    // what puts the sign-in flow there; if it mounted *first* the tree would
    // have refused the clash outright and this test would not get here.
    //
    // The description is what tells the two apart without running either: the
    // shipped one says "用现在配置的凭据重新登录" and this one says what it
    // actually does.
    {
        let commands = mounted
            .app
            .context()
            .service::<atomcode_tui::plugin::CommandsSvc>()
            .expect("the screen provides its commands");
        let login = commands.find("login").expect("the screen offers /login");
        assert!(
            // Case-folded: the word is `CodingPlan` in the English table and
            // `codingplan` in the Chinese one, and which language this binary
            // draws in is not what is being asserted here.
            login.about.to_lowercase().contains("codingplan"),
            "/login is the sign-in flow, not the shipped re-read: {}",
            login.about
        );
    }
    // **The plugins panel is this launcher's too**, and both halves have to be
    // here or the command is a dead end: the port, which knows what a
    // marketplace is, and the row that draws it. A screen with the command and
    // neither would answer `/plugin` with "this screen has no plugins panel" —
    // which is what the new screen did before this existed.
    {
        let ctx = mounted.app.context();
        let commands = ctx
            .service::<atomcode_tui::plugin::CommandsSvc>()
            .expect("the screen provides its commands");
        assert!(
            commands.find("plugin").is_some(),
            "the screen offers /plugin"
        );
        let port = ctx
            .service::<atomcode_tui::plugin::PluginsSvc>()
            .expect("this launcher fills the plugins port");
        // It answers from this machine rather than panicking on an empty one: a
        // fresh machine has no marketplaces, and the panel opens on a list that
        // says so.
        let _ = atomcode_tui::plugins::Plugins::rows(port.as_ref());
        let modules = ctx
            .service::<atomcode_tui::plugin::ModulesSvc>()
            .expect("the screen provides its modules");
        assert!(
            modules.has_view(atomcode_tui::modules::plugins::ID),
            "and the row that draws the panel is mounted, or the command opens nothing"
        );
    }
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    let mut screen_text = String::new();
    for _ in 0..200 {
        screen_text = term.text();
        if screen_text.contains("remember pineapple") && screen_text.contains("answer 1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        screen_text.contains("remember pineapple") && screen_text.contains("answer 1"),
        "the history the product wrote is on the screen:\n{screen_text}"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// The welcome block reads the language the *launcher* knows, not the one the
/// screen ships.
///
/// **This is the property the seam was built for, and it is not verifiable from
/// inside `atomcode-tui`.** The words come from `atomcode-config`, which the
/// screen must not depend on; they reach it through a row the launcher mounts,
/// and that row mounts *after* every row of the screen's own tree
/// (`launch::mount_with` appends the launcher's rows). A block that resolved the
/// seam when its producer mounted — rather than per opening, after the whole
/// tree is up — would silently fall back to this crate's shipped Chinese
/// sentences and nothing in the screen's own tests would notice, because there
/// the shipped sentences *are* the answer.
///
/// So the assertion is deliberately about a language the screen does not ship:
/// with `/language en` written to the configuration, the heading on screen is
/// the English one.
#[tokio::test]
async fn the_welcome_block_reads_the_language_the_launcher_knows() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());

    // The language is a fact about the host's file, which is what the launcher's
    // settings row reads. `en` rather than `zh_CN`: the screen's own fallback is
    // Chinese, so only a non-Chinese answer proves the seam was followed.
    let config_path = home.path().join("config.toml");
    std::fs::write(&config_path, "language = \"en\"\n").unwrap();
    // And the process locale, which is what `t()` answers from. Settle it the
    // way startup does, from that same file — under the lock the config crate
    // hands out for exactly this, so a sibling test never sees a locale neither
    // of them asked for.
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::i18n::resolve_initial_locale(
        None,
        Some(atomcode_config::locale::Locale::En),
    ));

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((100, 30)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");

    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    let mut screen_text = String::new();
    for _ in 0..200 {
        screen_text = term.text();
        if screen_text.contains("Tips for getting started") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        screen_text.contains("Tips for getting started"),
        "the launcher's language reached the block, rather than the screen's own \
         fallback:\n{screen_text}"
    );
    assert!(
        !screen_text.contains("上手提示"),
        "and the shipped heading is not what was drawn:\n{screen_text}"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// What the launcher knows about *this launch* is on screen, and the welcome
/// block still opens above it.
///
/// Three notices reach a person only this way — a configuration file that did
/// not parse, a `resume` that moved the working directory into another project,
/// and a session forked because the one asked for was busy. All three were dead
/// on this screen: the launcher computed them and handed them to
/// `atomcode_tuix::run`, which this screen is not. stderr is not the fix, since
/// entering the alternate screen wipes it.
///
/// **The second half is the one that needs a criterion.** `open_conversation`
/// stands down when the stream already holds a block from another producer, and
/// these are emitted before the welcome. They are `commands` blocks, the same
/// producer the readiness notice has always used, so the welcome still lands —
/// but that is an invariant two files apart, and getting it wrong trades a
/// silent notice for a silent welcome.
#[tokio::test]
async fn what_this_launch_has_to_say_is_said_and_the_welcome_still_opens() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());

    let config_path = home.path().join("config.toml");
    std::fs::write(&config_path, "language = \"en\"\n").unwrap();
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::i18n::resolve_initial_locale(
        None,
        Some(atomcode_config::locale::Locale::En),
    ));

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((100, 30)),
        ..Screen::default()
    };
    // As the launcher builds it: `merge_startup_notices` joins what it has with
    // a newline, and each piece is about its own thing.
    let notice = "this config did not parse\nthat resume moved you to another project";
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        Some(notice.to_string()),
    )
    .await
    .expect("the screen mounts");

    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    let mut screen_text = String::new();
    for _ in 0..200 {
        screen_text = term.text();
        if screen_text.contains("this config did not parse")
            && screen_text.contains("Tips for getting started")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        screen_text.contains("this config did not parse"),
        "the launcher's news reached the screen:\n{screen_text}"
    );
    assert!(
        screen_text.contains("that resume moved you to another project"),
        "and so did the second piece, as its own notice:\n{screen_text}"
    );
    assert!(
        screen_text.contains("Tips for getting started"),
        "and the welcome still opened over them:\n{screen_text}"
    );
    // Under the welcome, not over it: the screen's first line is the product's
    // banner, not a note about how this launch went.
    let welcome_at = screen_text.find("Tips for getting started").unwrap();
    let notice_at = screen_text.find("this config did not parse").unwrap();
    assert!(
        welcome_at < notice_at,
        "the launch's notices sit under the welcome:\n{screen_text}"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// The commands only the classic screen has yet (`/webui`, `/sync`, `/app`,
/// `/desktop`), typed on this one after the default moved: each says where it
/// still lives instead of "no such command" — and none is recommended, so the
/// menu never offers something that can only answer "go elsewhere"
/// (`docs/plans/2026-09-19-remaining-gaps.md`, decision 10).
#[tokio::test]
async fn a_command_only_the_classic_screen_has_says_where_it_lives() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((120, 40)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    for (name, run) in atomcode::tui_elsewhere::IN_THE_CLI {
        term.type_line(&format!("/{name}"));
        let expected = format!("/{name} 归命令行");
        let mut screen_text = String::new();
        for _ in 0..200 {
            screen_text = term.text();
            if screen_text.contains(&expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            screen_text.contains(&expected) && screen_text.contains(run),
            "/{name} says how to run it instead:\n{screen_text}"
        );
    }

    for name in atomcode::tui_elsewhere::CLASSIC_ONLY {
        term.type_line(&format!("/{name}"));
        let expected = format!("/{name} 暂时只在经典界面里有");
        let mut screen_text = String::new();
        for _ in 0..200 {
            screen_text = term.text();
            if screen_text.contains(&expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            screen_text.contains(&expected) && screen_text.contains("atomcode --classic"),
            "/{name} says where it still lives:\n{screen_text}"
        );
        assert_eq!(
            count.0.load(Ordering::SeqCst),
            0,
            "and it is not sent to the model as a prompt"
        );
    }

    // `/upgrade` is answered on this screen now (`tui_upgrade`), not pointed at
    // the CLI. An argument it does not take is refused in the classic screen's
    // words — which reaches the row without touching the network.
    term.type_line("/upgrade bogus");
    let expected = "未知的 /upgrade 参数";
    let mut screen_text = String::new();
    for _ in 0..200 {
        screen_text = term.text();
        if screen_text.contains(expected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        screen_text.contains(expected) && !screen_text.contains("/upgrade 归命令行"),
        "/upgrade is this screen's own:\n{screen_text}"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// `/welcome` — the classic screen's name for re-running the first-run
/// walkthrough — opens this screen's walkthrough rather than meeting "no such
/// command" after the default moved.
#[tokio::test]
async fn the_classic_name_for_the_walkthrough_still_opens_it() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((120, 40)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    term.type_line("/welcome");
    let mut screen_text = String::new();
    for _ in 0..200 {
        screen_text = term.text();
        if screen_text.contains("先把这台机器配好") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        screen_text.contains("先把这台机器配好"),
        "`/welcome` opened the walkthrough:\n{screen_text}"
    );
    assert_eq!(count.0.load(Ordering::SeqCst), 0, "not sent as a prompt");
    // On a screen nobody has said anything on, the walkthrough opens straight
    // into the intro: a question whose only sensible answer is yes is one
    // people learn to press through without reading.
    assert!(
        !screen_text.contains("这会清屏"),
        "nothing to lose, so nothing to ask:\n{screen_text}"
    );

    // Now say something, so there is a conversation to lose, and ask again.
    //
    // **The wiring is the half worth pinning.** Whether the warning step is
    // built is a flag, and the flag is worked out in `run()` from the session
    // on screen — a build that always passed `false` would keep every unit
    // criterion about the step green and still never show it to anybody.
    term.press(atomcode_tui::surface::KeyPress::plain(
        atomcode_tui::surface::Key::Esc,
    ));
    term.type_line("hello");
    for _ in 0..200 {
        if count.0.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        count.0.load(Ordering::SeqCst) > 0,
        "the turn was taken, so the log has something a person said"
    );

    term.type_line("/welcome");
    for _ in 0..200 {
        screen_text = term.text();
        if screen_text.contains("这会清屏") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        screen_text.contains("这会清屏"),
        "asked before the walkthrough, not after the sign-in that clears:\n{screen_text}"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// `/proxy` on this screen does what it did on the classic one: the mode is
/// written to the config file (so the next launch keeps it), and a bare `/proxy`
/// offers the three modes with the current one marked.
#[tokio::test]
async fn the_proxy_mode_is_chosen_here_and_kept_in_the_file() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    std::fs::write(&config_path, "language = \"zh_CN\"\n").unwrap();
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((140, 40)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path.clone(),
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    async fn shows(term: &atomcode_tui::surface::Headless, what: &str) -> String {
        let mut text = String::new();
        for _ in 0..200 {
            text = term.text();
            if text.contains(what) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        text
    }

    term.type_line("/proxy no_proxy");
    let said = shows(&term, "出站代理已改为 no_proxy").await;
    assert!(said.contains("出站代理已改为 no_proxy"), "{said}");
    let written = atomcode_config::config::Config::load(&config_path).expect("the file reads");
    assert_eq!(
        written.network.proxy.mode,
        atomcode_config::proxy::ProxyMode::NoProxy,
        "the mode is kept in the file"
    );
    assert_eq!(
        written.language,
        Some(atomcode_config::locale::Locale::ZhCn),
        "and nothing else in the file was lost"
    );

    term.type_line("/proxy");
    let offered = shows(&term, "出站代理（现在：no_proxy）").await;
    assert!(
        offered.contains("出站代理（现在：no_proxy）"),
        "a bare `/proxy` offers the modes, saying which is in force:\n{offered}"
    );
    for mode in ["follow_system", "default_proxy", "no_proxy"] {
        assert!(offered.contains(mode), "`{mode}` is offered:\n{offered}");
    }
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        0,
        "nothing was sent as a prompt"
    );

    term.press(atomcode_tui::surface::KeyPress::plain(
        atomcode_tui::surface::Key::Esc,
    ));
    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// `/schedule` lists what is scheduled on this machine — read-only, and it says
/// where adding and removing live (the OS scheduler runs them, screen or no
/// screen).
#[tokio::test]
async fn scheduled_tasks_are_listed_here() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((140, 40)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    term.type_line("/schedule");
    let mut screen_text = String::new();
    for _ in 0..200 {
        screen_text = term.text();
        if screen_text.contains("atomcode schedule add") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        screen_text.contains("atomcode schedule add"),
        "an empty list still says how one is made:\n{screen_text}"
    );
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        0,
        "nothing was sent as a prompt"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// Wait for the screen to read as `done` says, or fail naming `what`.
async fn until_text(
    term: &atomcode_tui::surface::Headless,
    what: &str,
    done: impl Fn(&str) -> bool,
) {
    let mut text = String::new();
    for _ in 0..400 {
        text = term.text();
        if done(&text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{what}:\n{text}");
}

/// **`/todo add` and `/todo clear` edit the plan without a turn.** The whole
/// way down: the screen hands the words to the session's `todo` command, the
/// harness writes a `todowrite` call and its result into the log between turns,
/// and the panel folds the list from there — nothing is sent as a prompt.
#[tokio::test]
async fn a_person_edits_the_plan_with_todo_add_and_clear() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((140, 40)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });
    term.type_line("/todo add 补上回归测试");
    until_text(&term, "the task is added", |text| {
        text.contains("Added task: 补上回归测试")
    })
    .await;
    term.type_line("/todo add 跑一遍全量");
    until_text(&term, "the panel counts both", |text| {
        text.contains("补上回归测试") && text.contains("跑一遍全量") && text.contains("2 待办")
    })
    .await;
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        0,
        "no edit was sent as a prompt"
    );

    // The next turn's request carries each edit as a call immediately followed
    // by its result — the pairing a provider rejects a request without.
    term.type_line("继续");
    until_text(&term, "the turn answered", |text| text.contains("answer 1")).await;
    let sent = count.1.lock().unwrap().clone();
    let edits: Vec<usize> = sent
        .iter()
        .enumerate()
        .filter(|(_, m)| m.tool_calls.iter().any(|c| c.name == "todowrite"))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(edits.len(), 2, "both edits reach the model: {sent:#?}");
    for &at in &edits {
        let call = &sent[at].tool_calls[0];
        let result = &sent[at + 1];
        assert_eq!(
            result.tool_call_id.as_deref(),
            Some(call.id.as_str()),
            "a call and its result, next to each other: {sent:#?}"
        );
    }
    let said = sent
        .iter()
        .rposition(|m| m.text == "继续")
        .expect("the person's words were sent");
    assert!(
        edits.iter().all(|&at| at + 1 < said),
        "the person's words come after the edits: {sent:#?}"
    );

    term.type_line("/todo clear");
    until_text(&term, "the list is cleared", |text| {
        text.contains("(no tasks)")
    })
    .await;
    term.type_line("/todo");
    until_text(&term, "and /todo says there is none", |text| {
        text.contains("这段对话里还没有计划清单")
    })
    .await;
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        1,
        "the clear was not sent as a prompt either"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

/// Every command this launcher mounts is in the menu `/help` prints. One
/// criterion for all of them, because the way any of these rows breaks is the
/// same: the row stops being mounted and nothing says so.
#[tokio::test]
async fn the_launchers_own_commands_are_in_the_menu() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((160, 60)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let commands = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::CommandsSvc>()
        .expect("the command registry");
    let offered: Vec<String> = commands
        .all()
        .into_iter()
        .map(|c| c.name.into_owned())
        .collect();
    for name in [
        "proxy",
        "schedule",
        "openrouter",
        "onboarding",
        "config",
        "changelog",
    ] {
        assert!(
            offered.contains(&name.to_string()),
            "`/{name}` is mounted: {offered:?}"
        );
    }
}

/// `/changelog` is picked the way `/resume` is: the releases rise from the
/// bottom, the selected one's points under the list. Enter opens the release in
/// the same panel, drawn as markdown — and Esc comes back to the list, with the
/// cursor where it was, so reading one release leads to the next.
#[tokio::test]
async fn the_changelog_is_picked_from_a_list_read_in_the_panel_and_left_back_to_it() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((120, 40)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    let newest = atomcode_config::changelog::shipped()
        .into_iter()
        .find(|r| r.version <= atomcode_config::changelog::Version::current())
        .expect("this build ships notes for itself or an earlier release");
    let first_point = newest.highlights().first().cloned().expect("a point");

    term.type_line("/changelog");
    let listed = until_shown(&term, "选一个版本查看更新内容").await;
    if std::env::var_os("SHOW_SCREEN").is_some() {
        eprintln!("{listed}");
    }
    assert!(listed.contains(&newest.version.to_string()), "{listed}");
    assert!(
        listed.contains(&first_point),
        "the selected release's points show under the list:\n{listed}"
    );

    term.press(atomcode_tui::surface::KeyPress::plain(
        atomcode_tui::surface::Key::Enter,
    ));
    let read = until_shown(&term, "esc 回到列表").await;
    if std::env::var_os("SHOW_SCREEN").is_some() {
        eprintln!("{read}");
    }
    assert!(
        !read.contains("选一个版本查看更新内容"),
        "the release is open in place of the list:\n{read}"
    );
    assert!(
        read.contains(&first_point) && !read.contains("**"),
        "the notes are drawn as markdown:\n{read}"
    );

    // The newest release lists issues: Tab shows them, each a title with no
    // address printed — the address is the link under it.
    let issues = newest.parts().issues;
    if let Some(first_issue) = issues
        .lines()
        .find_map(|line| line.strip_prefix("- ["))
        .and_then(|rest| rest.split("](").next())
    {
        term.press(atomcode_tui::surface::KeyPress::plain(
            atomcode_tui::surface::Key::Tab,
        ));
        let listed = until_shown(&term, first_issue).await;
        if std::env::var_os("SHOW_SCREEN").is_some() {
            eprintln!("{listed}");
        }
        assert!(listed.contains(first_issue), "{listed}");
        assert!(!listed.contains("https://atomgit.com"), "{listed}");
    }

    term.press(atomcode_tui::surface::KeyPress::plain(
        atomcode_tui::surface::Key::Esc,
    ));
    let back = until_shown(&term, "选一个版本查看更新内容").await;
    assert!(
        back.contains("选一个版本查看更新内容") && !back.contains("esc 回到列表"),
        "Esc is back on the list:\n{back}"
    );
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        0,
        "nothing was sent as a prompt"
    );

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

async fn until_shown(term: &atomcode_tui::surface::Headless, what: &str) -> String {
    let mut text = String::new();
    for _ in 0..200 {
        text = term.text();
        if text.contains(what) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    text
}

/// Mid-turn, a command that replaces the conversation says plainly that it
/// waits for the turn and that Esc stops it — not the runtime's "busy" — and
/// `/compact` says it comes after the turn. Neither stops the turn.
#[tokio::test]
async fn commands_typed_mid_turn_say_what_they_wait_for() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let count = Arc::new(Count::default());
    let config_path = home.path().join("config.toml");
    let _locale = atomcode_config::i18n::test_lock();
    atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);

    let front_end = FrontEnd::new();
    let (mut start, config) = start(
        project.path(),
        &count,
        SessionMode::Fresh,
        Some(front_end.clone()),
    );
    start.provider_factory = Arc::new(HangingFactory(count.clone()));
    let runtime = CodingRuntime::start(start).await.expect("starts");
    let screen = Screen {
        headless: Some((120, 40)),
        ..Screen::default()
    };
    let mounted = tui_front::mount(
        runtime,
        front_end,
        config,
        None,
        &screen,
        config_path,
        None,
        None,
    )
    .await
    .expect("the screen mounts");
    let term = mounted
        .app
        .context()
        .service::<atomcode_tui::plugin::SurfaceSvc>()
        .and_then(|surface| surface.as_any_headless())
        .expect("a headless surface");
    let ui = mounted.ui.clone();
    let ctx = mounted.app.context();
    let running = tokio::spawn(async move {
        let _ = ui.run(&ctx, None).await;
    });

    term.type_line("帮我做件事");
    until_shown(&term, "正在思考中").await;

    term.type_line("/clear");
    let said = until_shown(&term, "要等这一轮结束").await;
    assert!(said.contains("/clear 要等这一轮结束"), "{said}");
    assert!(!said.contains("busy"), "{said}");

    term.type_line("/compact");
    let said = until_shown(&term, "这一轮结束后就压缩").await;
    assert!(said.contains("这一轮结束后就压缩"), "{said}");

    assert_eq!(
        count.0.load(Ordering::SeqCst),
        1,
        "the turn is the same one"
    );
    term.press(atomcode_tui::surface::KeyPress::ctrl('c'));
    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}
