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

#[derive(Default)]
struct Count(AtomicUsize);

/// `answer N`, for every request.
struct Scripted(Arc<Count>);

#[async_trait::async_trait]
impl LlmProvider for Scripted {
    fn model_name(&self) -> &str {
        "scripted"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
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
    let mut agent = CodingAgentConfig::new("key", "https://example.test/v1", "scripted", project);
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
                external_subagents: Vec::new(),
                memory: false,
                web: false,
                review: false,
                subagents: SubagentPolicy::Disabled,
                rate_limit_source: None,
                front_end,
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
    let mounted = tui_front::mount(second, front_end, config, None, &screen, config_path, None)
        .await
        .expect("the screen mounts");

    // The row that gets an unconfigured machine working is mounted here, and
    // not behind a condition: what it contributes is a command, and whether it
    // runs is readiness's answer when the screen starts. A build that mounted
    // it only when it was needed would have to decide that before the host has
    // been asked.
    {
        let commands = mounted
            .app
            .context()
            .service::<atomcode_tui::plugin::CommandsSvc>()
            .expect("the screen provides its commands");
        let named = match atomcode::host::readiness_for(Some(
            atomcode_coding::ProviderUnavailableReason::NotConfigured,
        )) {
            atomcode_host_api::HostReply::Readiness { fix: Some(fix), .. } => fix,
            other => panic!("nothing named for a machine with no provider: {other:?}"),
        };
        assert!(
            commands.find(&named).is_some(),
            "the command readiness names is one this screen can run: {named}"
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
    let mounted = tui_front::mount(runtime, front_end, config, None, &screen, config_path, None)
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
    let mounted = tui_front::mount(runtime, front_end, config, None, &screen, config_path, None)
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

    for name in atomcode::tui_classic_only::NAMES {
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
    let mounted = tui_front::mount(runtime, front_end, config, None, &screen, config_path, None)
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

    term.press(atomcode_tui::surface::KeyPress::ctrl('d'));
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}
