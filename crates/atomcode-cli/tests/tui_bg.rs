//! `/bg` on the row-assembled screen: sessions kept running out of view
//! (`docs/plans/2026-09-25-bg-design.md` §七).
//!
//! End to end — the real screen, headless, over the real host
//! (`atomcode::background`) and real runtimes, with a scripted model that holds
//! a turn open until the test lets it go. What is asserted is what a person
//! would see or could ask the host, never a field of the implementation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atomcode::background::{ReviewHome, Spawn, Spawned};
use atomcode::tui_front;
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeStart, PrepareOptions,
    ProviderBuildError, SessionMode, StaticPluginHookSource, SubagentPolicy,
};
use atomcode_host_api::{
    BackgroundSession, BackgroundState, HostCommand, HostControl, HostError, HostReply,
};
use atomcode_i18n::screen::{t, Msg};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use atomcode_tui::launch::Screen;
use atomcode_tui::surface::{Key, KeyPress, Mods};
use tokio::sync::watch;

/// The model. A turn whose prompt says `slow` is held until the gate opens,
/// then answers `slow done`; anything else answers at once.
struct Model {
    gate: watch::Receiver<bool>,
    /// Slow turns that reached the model.
    started: Arc<AtomicUsize>,
    /// Slow turns the model finished — which a cancelled turn never does.
    finished: Arc<AtomicUsize>,
    /// Turns that reached the model — requests that carry tools. The side
    /// calls a session makes on its own (its title, the guess at what to say
    /// next) are answered but not counted: they land whenever they land, and a
    /// test waiting on this for "the reply reached the model" was satisfied by
    /// one of them before the reply was sent.
    count: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Script {
    gate: watch::Receiver<bool>,
    started: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
    count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmProvider for Model {
    fn model_name(&self) -> &str {
        "scripted"
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        // A turn carries tools; the side calls a session makes (its title)
        // do not, and must not be held.
        let said: Vec<&str> = messages
            .iter()
            .filter(|m| m.role == Role::User && !m.synthetic)
            .map(|m| m.text.as_str())
            .collect();
        let slow = !tools.is_empty() && said.last().is_some_and(|text| text.contains("slow"));
        // `ask me`: a question for the person, through the tool that asks one.
        let last = messages.iter().rev().find(|m| !m.synthetic);
        if !tools.is_empty() && last.is_some_and(|m| m.role == Role::User && m.text == "ask me") {
            return Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "call-ask".into(),
                    name: "request_user_input".into(),
                    arguments: serde_json::json!({
                        "header": "Flavour",
                        "question": "Which one?",
                        "mode": "single",
                        "options": [{ "label": "vanilla" }, { "label": "pistachio" }],
                    })
                    .to_string(),
                }),
                StreamEvent::Done { truncated: false },
            ])));
        }
        // `ask two`: 一次问两条 —— 批问询走的是 `{"questions": [...]}`
        // (`crate::ask::batch_for`,形状见 `crates/atomcode-tui/src/ask.rs:1276-1334`)。
        if !tools.is_empty() && last.is_some_and(|m| m.role == Role::User && m.text == "ask two") {
            return Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "call-ask-two".into(),
                    name: "request_user_input".into(),
                    arguments: serde_json::json!({
                        "questions": [
                            {
                                "header": "Flavour",
                                "question": "Which one?",
                                "mode": "single",
                                "options": [{ "label": "vanilla" }, { "label": "pistachio" }],
                            },
                            {
                                "header": "Count",
                                "question": "How many?",
                                "mode": "text",
                            },
                        ],
                    })
                    .to_string(),
                }),
            ])));
        }
        // `review it`: the model reaches for `code_review` on its own, the way a
        // person's "审查下代码改动" makes it.
        if !tools.is_empty() && last.is_some_and(|m| m.role == Role::User && m.text == "review it")
        {
            return Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "call-review".into(),
                    name: "code_review".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ])));
        }
        let text = if slow {
            self.started.fetch_add(1, Ordering::SeqCst);
            let mut gate = self.gate.clone();
            let _ = gate.wait_for(|open| *open).await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            "slow done".to_string()
        } else if tools.is_empty() {
            // A side call, not a turn: answered, and not counted (see `count`).
            "side".to_string()
        } else {
            let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
            format!("answer {n}")
        };
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta(text),
            StreamEvent::Usage(TokenUsage {
                prompt: 10,
                completion: 2,
                cached: 0,
            }),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

impl CodingProviderFactory for Script {
    fn build(
        &self,
        _config: &CodingAgentConfig,
        _session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(Arc::new(Model {
            gate: self.gate.clone(),
            started: self.started.clone(),
            finished: self.finished.clone(),
            count: self.count.clone(),
        }))
    }
}

fn start(
    project: &std::path::Path,
    script: &Script,
    front_end: Arc<FrontEnd>,
    review_home: &ReviewHome,
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
                session: SessionMode::Fresh,
                tools: true,
                skill_dirs: Some(Vec::new()),
                plugin_skill_dirs: Vec::new(),
                mcp: false,
                extra_mcp_servers: Vec::new(),
                external_subagents: Vec::new(),
                memory: false,
                web: false,
                // Mounted, and handed the terminal's way to run it out of view —
                // what the launcher does (`spawn_native_cli_runtime`).
                review: true,
                subagents: SubagentPolicy::Disabled,
                rate_limit_source: None,
                review_delegate: Some(review_home.delegate_for(&front_end)),
                front_end: Some(front_end),
                host_plugins: Default::default(),
                identity: Default::default(),
            },
            provider_factory: Arc::new(script.clone()),
            plugin_hooks: Arc::new(StaticPluginHookSource::default()),
            image_preprocessor: None,
        },
        agent,
    )
}

/// A screen with `/bg`, over a first runtime and a way to start more.
///
/// `Rig::new` points the *process's* `ATOMCODE_HOME` at this rig's temp home,
/// which is why every test that builds one is
/// `#[serial_test::serial(atomcode_home)]`: two at once and the second retargets
/// the first's host mid-flight, while whichever finishes first drops the
/// `TempDir` the other is still writing a session into.
struct Rig {
    _home: tempfile::TempDir,
    project: tempfile::TempDir,
    gate: watch::Sender<bool>,
    script: Script,
    term: Arc<atomcode_tui::surface::Headless>,
    client: Arc<atomcode_tui::plugin::AgentClient>,
    running: tokio::task::JoinHandle<()>,
    _mounted: atomcode_tui::launch::Mounted,
    /// What holds the runtimes, as the launcher gets it back.
    host: Arc<atomcode::background::Background>,
    /// How many `HostCommand::AnswerBackground` calls have gone through this
    /// rig's control, counted by [`CountingControl`].
    answers: Arc<AtomicUsize>,
}

/// A host that does not know `HostCommand::BackgroundQuestion` — a host from
/// before background questions could be brought onto the foreground. Everything
/// else goes through untouched.
struct WithoutBackgroundQuestions(Arc<dyn HostControl>);

#[async_trait::async_trait]
impl HostControl for WithoutBackgroundQuestions {
    async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
        match command {
            HostCommand::BackgroundQuestion { .. } => Err(HostError::Failed {
                message: "this host does not bring background questions forward".into(),
            }),
            other => self.0.call(other).await,
        }
    }

    fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_host_api::HostEvent> {
        self.0.subscribe()
    }
}

/// A host wrapper that counts `HostCommand::AnswerBackground` calls and
/// forwards everything, unchanged, to the real control underneath — so a test
/// can prove a withdrawn question sent no answer without guessing at the
/// dropped runtime's own reply (design §6: `Err` and `Ok(None)` are not the
/// same, and a dropped runtime's `NotFound`/`Busy` must not be mistaken for
/// "no answer was sent").
struct CountingControl {
    inner: Arc<dyn HostControl>,
    answers: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl HostControl for CountingControl {
    async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
        if matches!(command, HostCommand::AnswerBackground { .. }) {
            self.answers.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.call(command).await
    }

    fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_host_api::HostEvent> {
        self.inner.subscribe()
    }
}

impl Rig {
    async fn new() -> Self {
        Self::build(false).await
    }

    /// The same rig on a host that refuses `HostCommand::BackgroundQuestion`
    /// (design §9): the screen keeps today's tip line, and a background
    /// question is answered by bringing its session forward with `/bg N`.
    async fn without_background_questions() -> Self {
        Self::build(true).await
    }

    async fn build(refuse_background_questions: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);
        let project = tempfile::tempdir().unwrap();
        let (gate, open) = watch::channel(false);
        let script = Script {
            gate: open,
            started: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(AtomicUsize::new(0)),
            count: Arc::new(AtomicUsize::new(0)),
        };
        let front_end = FrontEnd::new();
        let review_home = ReviewHome::new();
        let (first, config) = start(project.path(), &script, front_end.clone(), &review_home);
        let runtime = CodingRuntime::start(first).await.expect("starts");
        let spawn: Spawn = {
            let script = script.clone();
            let review_home = review_home.clone();
            Arc::new(move |working_dir: std::path::PathBuf| {
                let script = script.clone();
                let review_home = review_home.clone();
                Box::pin(async move {
                    let front_end = FrontEnd::new();
                    let (start, config) =
                        start(&working_dir, &script, front_end.clone(), &review_home);
                    let runtime = CodingRuntime::start(start)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(Spawned {
                        runtime,
                        front_end,
                        config,
                    })
                }) as futures::future::BoxFuture<'static, Result<Spawned, String>>
            })
        };
        let screen = Screen {
            headless: Some((120, 48)),
            ..Screen::default()
        };
        let (mounted, host) = tui_front::mount_with_background(
            runtime,
            front_end,
            config,
            None,
            &screen,
            home.path().join("config.toml"),
            None,
            None,
            Some(spawn),
            Some(review_home),
        )
        .await
        .expect("the screen mounts");
        let ctx = mounted.app.context();
        let answers = Arc::new(AtomicUsize::new(0));
        {
            let answers = answers.clone();
            ctx.service::<atomcode_tui::plugin::ConnectionSvc>()
                .expect("the connection the screen will take")
                .wrap_control(move |control| {
                    Arc::new(CountingControl {
                        inner: control,
                        answers,
                    })
                });
        }
        if refuse_background_questions {
            ctx.service::<atomcode_tui::plugin::ConnectionSvc>()
                .expect("the connection the screen will take")
                .wrap_control(|control| Arc::new(WithoutBackgroundQuestions(control)));
        }
        let term = ctx
            .service::<atomcode_tui::plugin::SurfaceSvc>()
            .and_then(|surface| surface.as_any_headless())
            .expect("a headless surface");
        let client = ctx
            .service::<atomcode_tui::plugin::AgentClientSvc>()
            .expect("the screen's client");
        let ui = mounted.ui.clone();
        let running = tokio::spawn(async move {
            let _ = ui.run(&ctx, None).await;
        });
        let rig = Self {
            _home: home,
            project,
            gate,
            script,
            term,
            client,
            running,
            _mounted: mounted,
            host: host.expect("a host that can start runtimes"),
            answers,
        };
        // The first session is on screen before anything is typed.
        rig.until("the first session is followed", |rig| {
            !rig.client.root().is_empty()
        })
        .await;
        rig
    }

    fn control(&self) -> Arc<dyn HostControl> {
        self.client.control().expect("host control")
    }

    /// How many `HostCommand::AnswerBackground` calls this rig's control has
    /// forwarded so far.
    fn answers_sent(&self) -> usize {
        self.answers.load(Ordering::SeqCst)
    }

    async fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        for _ in 0..400 {
            if done(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{what} — never happened. On screen:\n{}", self.term.text());
    }

    async fn until_screen(&self, needle: &str) {
        for _ in 0..400 {
            if self.term.text().contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("`{needle}` never came on screen:\n{}", self.term.text());
    }

    async fn until_screen_lacks(&self, needle: &str) {
        for _ in 0..400 {
            if !self.term.text().contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("`{needle}` never left the screen:\n{}", self.term.text());
    }

    async fn background(&self) -> Vec<BackgroundSession> {
        match self.control().call(HostCommand::BackgroundSessions).await {
            Ok(HostReply::BackgroundSessions { sessions }) => sessions,
            other => panic!("the host lists its background sessions: {other:?}"),
        }
    }

    async fn until_background(&self, what: &str, done: impl Fn(&[BackgroundSession]) -> bool) {
        let mut last = Vec::new();
        for _ in 0..400 {
            last = self.background().await;
            if done(&last) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{what} — never happened: {last:#?}");
    }

    async fn stored(&self, session: &str) -> String {
        match self.read_stored(session).await {
            Ok(lines) => lines,
            Err(other) => panic!("the log of {session} can be read: {other}"),
        }
    }

    /// [`stored`](Self::stored), without giving up on a failed read — for a
    /// poll that runs while the session is still writing, where the last line
    /// can be caught half written.
    async fn read_stored(&self, session: &str) -> Result<String, String> {
        match self
            .control()
            .call(HostCommand::PreviewSession {
                session: session.to_string(),
            })
            .await
        {
            Ok(HostReply::SessionPreview { lines }) => Ok(lines.join("\n")),
            other => Err(format!("{other:?}")),
        }
    }

    fn release(&self) {
        let _ = self.gate.send(true);
    }

    async fn quit(self) {
        self.term.press(KeyPress::ctrl('d'));
        let _ = tokio::time::timeout(Duration::from_secs(5), self.running).await;
    }
}

/// **A turn keeps running after `/bg`, and its answer is there on the way
/// back.** The foreground after `/bg` is a new, empty session; the panel says
/// the moved one is working, then that it is done; Esc brings it back and the
/// answer that was written while nobody was looking is on screen.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_turn_keeps_running_after_bg_and_its_result_is_there_on_return() {
    let rig = Rig::new().await;
    let first = rig.client.root();

    rig.term.type_line("slow task");
    rig.until("the slow turn reached the model", |rig| {
        rig.script.started.load(Ordering::SeqCst) == 1
    })
    .await;

    rig.term.type_line("/bg");
    rig.until_screen(&t(Msg::BgPanelMoved)).await;
    rig.until("the screen follows a new session", |rig| {
        rig.client.root() != first
    })
    .await;
    // A new session, not the old one under a new name: nothing was said in it.
    let fresh = rig.client.root();
    assert!(
        !rig.stored(&fresh).await.contains("slow task"),
        "the foreground after /bg is a fresh session"
    );
    let moved = rig.background().await;
    assert_eq!(moved.len(), 1, "{moved:#?}");
    assert_eq!(moved[0].session, first);
    assert_eq!(moved[0].state, BackgroundState::Running, "{moved:#?}");
    // Drawn in a real frame, under the group it is in.
    rig.until_screen(&t(Msg::BgGroupWorking)).await;

    rig.release();
    rig.until_background("the moved turn finished in the background", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    rig.until_screen(&t(Msg::BgGroupCompleted)).await;
    rig.until_screen("slow done").await;

    // Esc: back to the conversation that was moved.
    rig.term.press(KeyPress::plain(Key::Esc));
    rig.until("the moved session is on screen again", |rig| {
        rig.client.root() == first
    })
    .await;
    rig.until_screen("slow task").await;
    rig.until_screen("slow done").await;
    assert!(
        rig.background().await.is_empty(),
        "the empty session it was swapped with is closed, not kept as a slot"
    );
    rig.quit().await;
}

/// **`/bg drop` cancels a running one.** The turn the dropped session was
/// running never finishes — the model is let go afterwards and nothing is
/// written — and the slot is gone.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn bg_drop_cancels_a_running_one() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("slow task");
    rig.until("the slow turn reached the model", |rig| {
        rig.script.started.load(Ordering::SeqCst) == 1
    })
    .await;
    rig.term.type_line("/bg");
    rig.until_screen(&t(Msg::BgPanelMoved)).await;

    // ctrl+d twice on the selected row is `/bg drop`: the first only marks it —
    // dropping cancels its running turn, so it asks for the second.
    rig.term.press(KeyPress {
        key: Key::Char('d'),
        mods: Mods::CTRL,
    });
    rig.until_screen(&t(Msg::BgDropArmed)).await;
    assert_eq!(rig.background().await.len(), 1, "one press drops nothing");
    rig.term.press(KeyPress {
        key: Key::Char('d'),
        mods: Mods::CTRL,
    });
    rig.until_background("the slot is gone", |list| list.is_empty())
        .await;
    rig.release();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.script.finished.load(Ordering::SeqCst),
        0,
        "the dropped turn was cancelled, not left to finish"
    );
    assert!(
        !rig.stored(&first).await.contains("slow done"),
        "and nothing it would have said was written"
    );
    rig.quit().await;
}

/// **`/background <task>` runs a task without changing the foreground.** The
/// screen stays on the session it was on; the task runs in a session of its
/// own; `/bg 1` then brings that one forward with its answer.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn background_task_runs_without_changing_the_foreground() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("/background slow job");
    rig.until_screen(&t(Msg::BgStarted { slot: 1 })).await;
    assert_eq!(rig.client.root(), first, "the foreground did not move");
    let list = rig.background().await;
    assert_eq!(list.len(), 1, "{list:#?}");
    assert_ne!(list[0].session, first);
    let task = list[0].session.clone();

    rig.release();
    rig.until_background("the task finished", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    assert!(rig.stored(&task).await.contains("slow done"));

    rig.term.type_line("/bg 1");
    rig.until("the task's session comes forward", |rig| {
        rig.client.root() == task
    })
    .await;
    rig.until_screen("slow done").await;
    rig.quit().await;
}

/// **`/review` runs in the background by default.** The command is
/// `/background` with the task filled in: the person's conversation keeps its
/// place, and the review's own session is handed the prompt that names the tool
/// and the scope — which is what `/review` *is*. `/bg 1` brings it forward to
/// read.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_review_runs_in_a_background_session_by_default() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("/review staged");
    // 开始那行说出它在审哪一段,量得出文件数就说数(这个 rig 的目录不在 git 仓库
    // 里,量不出来,所以 `files: None`)。
    rig.until_screen(&t(Msg::ReviewStarted {
        what: &t(Msg::ReviewWhatStaged),
        files: None,
    }))
    .await;
    assert_eq!(rig.client.root(), first, "the foreground did not move");
    let list = rig.background().await;
    assert_eq!(list.len(), 1, "{list:#?}");
    let review = list[0].session.clone();
    assert_ne!(review, first);

    rig.until_background("the review finished", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    // The task that session was given names the tool and the scope the person
    // asked for. Read after the turn: the log is written as it goes, and before
    // the turn there is nothing to read yet.
    let log = rig.stored(&review).await;
    assert!(log.contains("code_review"), "{log}");
    assert!(log.contains(r#"{"scope":{"kind":"staged"}}"#), "{log}");

    rig.term.type_line("/bg 1");
    rig.until("the review comes forward", |rig| {
        rig.client.root() == review
    })
    .await;
    rig.quit().await;
}

/// **The review's result comes home labelled as the job's, not the person's.**
///
/// `/review` runs in a session of its own; when that session finishes,
/// `deliver_home` puts what it said back into the conversation that started it.
/// What this pins is whose words they read as: the block says「来自后台」because
/// the fact says the sender is outside this tree. Sent as a plain submit — which
/// is what this path used to do — the screen would have shown the person typing
/// a report they never wrote.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_review_result_comes_home_labelled_as_the_background_job() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("/review staged");
    rig.until_screen(&t(Msg::ReviewStarted {
        what: &t(Msg::ReviewWhatStaged),
        files: None,
    }))
    .await;

    rig.until_background("the review finished", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;

    // The content arrives in the conversation that started it, and it arrives
    // named for what it is. The foreground never moved to the review's session:
    // this is the conversation on screen.
    // Folded to its one line (`● 后台「…」的结果回来了`), which is the result
    // arriving: the conversation that started it answers under it.
    rig.until_screen("结果回来了").await;
    assert_eq!(rig.client.root(), first, "the foreground never moved");
    assert!(
        rig.term.text().contains("结果回来了"),
        "the result itself is here, not only a chip:\n{}",
        rig.term.text()
    );
    rig.quit().await;
}

/// **The model's own `code_review` runs where `/review` does.** Asked in plain
/// words, the model reaches for the tool; in the conversation in front, the
/// tool hands the review to a background session and returns at once — the turn
/// is not held for the minutes a review takes — and the review's result comes
/// home the way `/review`'s does.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn the_models_code_review_runs_in_a_background_session() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    // Something to review: a repository with an uncommitted change.
    let dir = rig.project.path();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(dir.join("a.rs"), "fn main() { changed(); }\n").unwrap();

    rig.term.type_line("review it");
    // Out of view, working for this conversation: an inline review would have
    // started no session at all.
    rig.until_background("the review went to a background session", |list| {
        list.len() == 1 && list[0].origin.as_deref() == Some(first.as_str())
    })
    .await;
    assert_eq!(rig.client.root(), first, "the foreground never moved");
    // And the review's own answer comes home, folded to its one line.
    rig.until_screen("结果回来了").await;
    rig.quit().await;
}

/// A git repository in `dir` with one uncommitted change: something for a
/// review to look at.
fn changed_repo(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(dir.join("a.rs"), "fn main() { changed(); }\n").unwrap();
}

/// The sessions `/resume` offers, by id.
async fn listed(rig: &Rig) -> Vec<String> {
    match rig
        .control()
        .call(HostCommand::ListSessions { working_dir: None })
        .await
    {
        Ok(HostReply::Sessions { sessions }) => sessions.into_iter().map(|s| s.id).collect(),
        other => panic!("the session list: {other:?}"),
    }
}

/// Wait until `/resume` does (`true`) or does not (`false`) offer `session`.
async fn until_listed(rig: &Rig, session: &str, offered: bool) {
    for _ in 0..100 {
        if listed(rig).await.iter().any(|id| id == session) == offered {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{session} offered in /resume should be {offered}");
}

/// The review started for the conversation in front, once its result is back.
async fn review_came_home(rig: &Rig) -> String {
    changed_repo(rig.project.path());
    let first = rig.client.root();
    rig.term.type_line("review it");
    rig.until_screen("结果回来了").await;
    assert_eq!(rig.client.root(), first, "the foreground never moved");
    rig.background()
        .await
        .into_iter()
        .find(|s| s.origin.as_deref() == Some(first.as_str()))
        .expect("the review's background session")
        .session
}

/// **Quitting after a review came home names the conversation, not the review.**
/// The review ran in a background session for the conversation in front and its
/// result is in that conversation: the review is not one of the person's
/// conversations in `/resume` (it still opens by id), and the `resume` line
/// printed on the way out is the conversation's alone.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn quitting_after_a_review_came_home_names_the_conversation() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    let review = review_came_home(&rig).await;

    until_listed(&rig, &review, false).await;
    assert!(listed(&rig).await.contains(&first), "the conversation is");
    assert!(
        rig.read_stored(&review).await.is_ok(),
        "the review still opens by id"
    );

    let host = rig.host.clone();
    rig.term.press(KeyPress::ctrl('d'));
    rig.until("the screen quit", |rig| rig.running.is_finished())
        .await;
    host.shutdown_all().await;

    assert_eq!(
        host.front_at_exit().as_deref(),
        Some(first.as_str()),
        "the line on the way out is for the conversation in front"
    );
    let lines = atomcode::exit_resume_hints(
        "atomcode",
        host.front_at_exit().as_deref(),
        &host.left_behind(),
        false,
    );
    assert_eq!(
        lines,
        vec![atomcode::resume_hint_line("atomcode", &first, false, false)],
        "one line, for the conversation — none for the review that worked for it"
    );
}

/// **A review a person takes up is theirs.** Brought to the front, the
/// session that worked for another conversation is one of the person's own:
/// offered in `/resume` again.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_review_brought_to_the_front_is_offered_again() {
    let rig = Rig::new().await;
    let review = review_came_home(&rig).await;
    until_listed(&rig, &review, false).await;

    rig.term.type_line("/bg 1");
    rig.until("the review is in front", |rig| rig.client.root() == review)
        .await;
    until_listed(&rig, &review, true).await;
    rig.quit().await;
}

/// **Work for a conversation that has not come home stays within reach.**
/// Until its result is in the conversation it worked for, a background task
/// is an ordinary session on disk — a crash leaves it in `/resume` — and
/// quitting while it runs names it, as a background one.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn quitting_before_a_task_came_home_names_it() {
    use atomcode_capabilities::session::manager::SessionOrigin;
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("/bg slow job");
    rig.until("the task reached the model", |rig| {
        rig.script.started.load(Ordering::SeqCst) == 1
    })
    .await;
    let task = rig
        .background()
        .await
        .into_iter()
        .find(|s| s.origin.as_deref() == Some(first.as_str()))
        .expect("the task's background session")
        .session;
    let store = atomcode_capabilities::session::SessionManager::for_project(
        rig.project.path(),
        &atomcode_coding::config::product_dirs_from_env(),
    );
    assert_eq!(
        store.read_meta(&task).unwrap().origin,
        SessionOrigin::Manual,
        "not marked while its result is not home"
    );

    let question = t(Msg::BgQuitQuestion { count: 1 }).into_owned();
    rig.term.press(KeyPress::ctrl('d'));
    rig.until_screen(&question).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until("the screen quit", |rig| rig.running.is_finished())
        .await;
    rig.until_background("the background runtime was stopped", |list| list.is_empty())
        .await;

    assert_eq!(rig.host.left_behind(), vec![task.clone()]);
    assert_eq!(
        atomcode::exit_resume_hints("atomcode", None, &rig.host.left_behind(), false),
        vec![atomcode::background_resume_hint_line(
            "atomcode", &task, false
        )],
    );
}

/// **Taking `/review`'s menu row stops on the line.** The bare form is worth
/// having — it means the working tree — but not worth firing on the pick: this
/// row starts a whole session, and somebody who opened the menu and took it is
/// about to say *which* changes. Enter without an argument still runs it.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn taking_the_review_row_leaves_it_on_the_line() {
    let rig = Rig::new().await;
    rig.term.type_text("/rev");
    rig.until_screen("/review").await;

    rig.term.press(KeyPress::plain(Key::Enter));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        rig.background().await.is_empty(),
        "taking the row started a review anyway"
    );
    assert!(
        rig.term.text().contains("/review"),
        "选中之后它停在行上,等你说审哪里:\n{}",
        rig.term.text()
    );

    // And enter again runs the default — the working tree, which is what the row
    // would have started on its own.
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until_screen(&t(Msg::ReviewStarted {
        what: &t(Msg::ReviewWhatUncommitted),
        files: None,
    }))
    .await;
    rig.quit().await;
}

/// **A finished background run's content comes home.** The answer lands in the
/// conversation that started it — not a "go read /bg" pointer — and the model
/// runs a turn on it, which is what the line promising to verify needs.
///
/// The scripted provider answers `answer N` off one counter of turns (side
/// calls are not counted, see `Model::count`), so `answer 1` on
/// this screen can only be the delivered content (the background session's own
/// transcript is not on screen), and `answer 2` is the turn that followed it.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_finished_background_run_delivers_its_answer_home() {
    let rig = Rig::new().await;
    rig.term.type_line("/background quiet job");
    rig.until_screen(&t(Msg::BgStarted { slot: 1 })).await;
    let list = rig.background().await;
    assert_eq!(list.len(), 1, "{list:#?}");

    // It arrives folded to its one line — the report (`answer 1`) waits behind
    // a click, because the conversation that started it answers right under it
    // (`answer 2`) and saying both at full length is saying it twice.
    rig.until("后台的结果回到这段对话里", |rig| {
        rig.term.text().contains("结果回来了")
    })
    .await;
    rig.until("而且它真的接着干了", |rig| {
        rig.term.text().contains("answer 2")
    })
    .await;
    assert!(
        !rig.term.text().contains("answer 1"),
        "the report is folded, not printed a second time:\n{}",
        rig.term.text()
    );
    rig.quit().await;
}

/// **`/bg` and `/background` are one command, and ← on an empty box opens
/// what runs out of view.** `/bg <task>` starts a background session like
/// `/background <task>` does; with nothing typed, ← opens the panel — and with
/// nothing in the background it opens nothing. (The status row no longer
/// counts background sessions; see `modules::status`.)
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn bg_takes_a_task_and_left_opens_the_panel() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    let panel = t(Msg::BgPlaceholder).into_owned();

    // Nothing in the background: ← is only a caret move.
    rig.term.press(KeyPress::plain(Key::Left));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rig.term.text().contains(&panel), "{}", rig.term.text());

    rig.term.type_line("/bg slow job");
    rig.until_screen(&t(Msg::BgStarted { slot: 1 })).await;
    assert_eq!(rig.client.root(), first, "the foreground did not move");
    rig.until_background("the task is in the background", |list| !list.is_empty())
        .await;

    // → is not the gesture: on an empty box it takes a suggested line, and
    // with none it does nothing.
    rig.term.press(KeyPress::plain(Key::Right));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rig.term.text().contains(&panel), "{}", rig.term.text());

    // With something typed, ← is a caret move and nothing else.
    rig.term.type_text("abc");
    rig.term.press(KeyPress::plain(Key::Left));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rig.term.text().contains(&panel), "{}", rig.term.text());
    // ← moved the caret back one; clear the line from its end.
    rig.term.press(KeyPress::plain(Key::End));
    for _ in 0..3 {
        rig.term.press(KeyPress::plain(Key::Backspace));
    }

    rig.term.press(KeyPress::plain(Key::Left));
    rig.until_screen(&panel).await;
    rig.term.press(KeyPress::plain(Key::Esc));
    rig.until("the panel closed", |rig| !rig.term.text().contains(&panel))
        .await;

    rig.release();
    rig.until_background("the task finished", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    rig.quit().await;
}

/// **The panel's box starts a task, and the list shows it.** Typed into the
/// panel rather than as a command — the same host command underneath.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_task_typed_into_the_panel_starts_a_background_session() {
    let rig = Rig::new().await;
    rig.term.type_line("/bg list");
    rig.until_screen(&t(Msg::BgPlaceholder)).await;
    rig.term.type_line("answer me");
    rig.until_background("a session was started from the panel", |list| {
        list.len() == 1 && list[0].state == BackgroundState::Done
    })
    .await;
    rig.until_screen(&t(Msg::BgGroupCompleted)).await;
    rig.until_screen("answer").await;
    rig.quit().await;
}

/// **A question asked out of view waits, and is asked again on the way back.**
/// The session is filed under "needs input" with the question as its line; once
/// it is brought forward the question is on screen as a question — answering
/// it with Enter is what finishes the turn. The request is not a fact in the
/// log, so replaying the log alone would show the words and leave nothing to
/// answer.
///
/// On a host that does not bring background questions forward
/// ([`Rig::without_background_questions`]): on one that does, the question
/// comes up on the foreground by itself and `/bg 1` would be typed into it.
/// Coming back to a waiting session is still how such a host gets it answered.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_question_asked_in_the_background_is_asked_again_on_return() {
    let rig = Rig::without_background_questions().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting for a person", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let list = rig.background().await;
    assert!(
        list[0]
            .last
            .as_deref()
            .is_some_and(|last| last.contains("Which one?")),
        "its line is the question: {list:#?}"
    );
    let asking = list[0].session.clone();
    let before = rig.script.count.load(Ordering::SeqCst);

    rig.term.type_line("/bg 1");
    rig.until("the asking session comes forward", |rig| {
        rig.client.root() == asking
    })
    .await;
    rig.until_screen("Which one?").await;
    // Enter takes the first option — only if the question is really up.
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until("the answered turn goes on to the model", |rig| {
        rig.script.count.load(Ordering::SeqCst) > before
    })
    .await;
    rig.quit().await;
}

/// Whether a session's preview has `said` on a line and something after it —
/// the answer to it, since the preview alternates what was said and answered.
fn answered_after(preview: &str, said: &str) -> bool {
    let lines: Vec<&str> = preview.lines().collect();
    lines
        .iter()
        .position(|line| line.contains(said))
        .is_some_and(|at| at + 1 < lines.len())
}

/// **Space replies to a background session in place.** The words go to the
/// selected session — its log has them and the model answered — and the screen
/// stays where it was.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn space_in_the_panel_replies_without_switching() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("/background hello there");
    rig.until_background("the first task is done", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    let task = rig.background().await[0].session.clone();

    rig.term.type_line("/bg list");
    rig.until_screen(&t(Msg::BgPlaceholder)).await;
    rig.term.press(KeyPress::ch(' '));
    rig.term.type_line("one more thing");
    // Waited for in that session's own log, which is what is being asserted.
    // Not on the model's turn counter, and not on the panel's `Done`: the first
    // turn's result comes home and the foreground runs a turn on it, which moves
    // the counter before the reply is sent; and the panel still says `Done`
    // from the first turn until the reply's turn shows as running.
    // Read while that session may still be writing, so a failed read (a last
    // line caught half written) is "not yet", not a failure.
    let mut stored = String::new();
    for _ in 0..400 {
        if let Ok(now) = rig.read_stored(&task).await {
            stored = now;
            if answered_after(&stored, "one more thing") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        answered_after(&stored, "one more thing"),
        "the reply is in that session's log, and was answered there:\n{stored}"
    );
    // And the reply's turn is over before leaving — the `Done` now is that
    // turn's, since its answer is already in the log. Leaving with it still
    // running would stop at "background sessions are running, quit?".
    rig.until_background("the reply's turn is done", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    assert_eq!(rig.client.root(), first, "the screen did not switch");
    rig.quit().await;
}

/// **`/bg` is refused while the conversation waits for an answer**, and
/// nothing moves: moving it would leave the question with nobody to ask.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn bg_is_refused_while_a_question_waits() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("ask me");
    rig.until_screen("Which one?").await;
    let refused = rig
        .control()
        .call(HostCommand::Background {
            session: first.clone(),
        })
        .await;
    assert!(
        matches!(refused, Err(atomcode_host_api::HostError::Busy { .. })),
        "{refused:?}"
    );
    assert_eq!(rig.client.root(), first);
    assert!(rig.background().await.is_empty());
    rig.quit().await;
}

/// **Quitting with background sessions still running asks first.** ctrl+d
/// puts the question up instead of leaving; "stay" (Esc) leaves everything
/// running and the screen up; saying yes quits, and the background runtime is
/// stopped — its held turn never finishes even once the model lets go.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn quitting_with_background_sessions_running_asks_first() {
    let rig = Rig::new().await;
    rig.term.type_line("slow task");
    rig.until("the slow turn reached the model", |rig| {
        rig.script.started.load(Ordering::SeqCst) == 1
    })
    .await;
    rig.term.type_line("/bg");
    rig.until_screen(&t(Msg::BgPanelMoved)).await;
    rig.until_screen(&t(Msg::BgGroupWorking)).await;

    // In the panel ctrl+d is the panel's own (two of them drop a session), so
    // put it away first — ← on the empty box, which stays in this session.
    rig.term.press(KeyPress::plain(Key::Left));
    rig.until("the panel went away", |rig| {
        !rig.term.text().contains(&*t(Msg::BgPanelMoved))
    })
    .await;
    let question = t(Msg::BgQuitQuestion { count: 1 }).into_owned();
    rig.term.press(KeyPress::ctrl('d'));
    rig.until_screen(&question).await;
    assert!(!rig.running.is_finished(), "asked, not gone");

    // Stay.
    rig.term.press(KeyPress::plain(Key::Esc));
    rig.until("the question went away", |rig| {
        !rig.term.text().contains(&question)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!rig.running.is_finished(), "staying keeps the screen up");
    let list = rig.background().await;
    assert_eq!(list.len(), 1, "and the background session: {list:#?}");
    assert_eq!(list[0].state, BackgroundState::Running);

    // Asked again, and this time yes.
    rig.term.press(KeyPress::ctrl('d'));
    rig.until_screen(&question).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until("the screen quit", |rig| rig.running.is_finished())
        .await;
    rig.until_background("the background runtime was stopped", |list| list.is_empty())
        .await;
    rig.release();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.script.finished.load(Ordering::SeqCst),
        0,
        "its turn was cancelled, not left running after the screen went"
    );
}

/// **With nothing running in the background, quitting is not asked about.**
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn quitting_with_nothing_running_does_not_ask() {
    let rig = Rig::new().await;
    rig.term.type_line("/background hello");
    rig.until_background("the task is done", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    rig.until_screen(&t(Msg::BgStarted { slot: 1 })).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.term.press(KeyPress::ctrl('d'));
    rig.until("the screen quit", |rig| rig.running.is_finished())
        .await;
}

/// **A background session waiting for an answer says so on the foreground**,
/// on the row above the composer, with the way to open it; once it is
/// answered the line is gone.
///
/// On a host that does not bring background questions forward (design §9): the
/// screen keeps this line and does not put the question up itself.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_background_session_waiting_for_an_answer_is_told_on_the_foreground() {
    let rig = Rig::without_background_questions().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let list = rig.background().await;
    let title = list[0].title.clone().unwrap_or_default();
    let tip = t(Msg::BgWaitingTip {
        slot: 1,
        title: &title,
    })
    .into_owned();
    rig.until_screen(&tip).await;
    // A host that cannot hand the question over leaves it where it is: the
    // screen does not claim to have brought it up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let who = t(Msg::BgAsker {
        slot: 1,
        title: &title,
    })
    .into_owned();
    assert!(
        !rig.term.text().contains(&who),
        "no background question on the foreground:\n{}",
        rig.term.text()
    );
    assert_eq!(
        rig.answers_sent(),
        0,
        "a host that cannot bring the question over was not sent an answer for it"
    );

    let before = rig.script.count.load(Ordering::SeqCst);
    rig.term.type_line("/bg 1");
    rig.until_screen("Which one?").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until("answered", |rig| {
        rig.script.count.load(Ordering::SeqCst) > before
    })
    .await;
    rig.until("the line went away", |rig| !rig.term.text().contains(&tip))
        .await;
    rig.quit().await;
}

/// **Leaving prints how to come back to every background session it stopped**
/// — the foreground's `resume` line, then one per background session, in the
/// same words.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn quitting_says_how_to_resume_the_background_sessions_it_stopped() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("slow task");
    rig.until("the slow turn reached the model", |rig| {
        rig.script.started.load(Ordering::SeqCst) == 1
    })
    .await;
    rig.term.type_line("/bg");
    rig.until_screen(&t(Msg::BgPanelMoved)).await;
    let fresh = rig.client.root();
    assert_ne!(fresh, first);

    // In the panel ctrl+d is the panel's own (two of them drop a session), so
    // put it away first — ← on the empty box, which stays in this session.
    rig.term.press(KeyPress::plain(Key::Left));
    rig.until("the panel went away", |rig| {
        !rig.term.text().contains(&*t(Msg::BgPanelMoved))
    })
    .await;
    let question = t(Msg::BgQuitQuestion { count: 1 }).into_owned();
    rig.term.press(KeyPress::ctrl('d'));
    rig.until_screen(&question).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until("the screen quit", |rig| rig.running.is_finished())
        .await;
    rig.until_background("the background runtime was stopped", |list| list.is_empty())
        .await;

    // The foreground in front at exit is the fresh one `/bg` put there, and
    // nothing was said in it: no line for it, as for any empty session.
    assert_eq!(
        rig.host.front_at_exit(),
        None,
        "an empty foreground has no line"
    );
    let lines = atomcode::exit_resume_hints(
        "atomcode",
        rig.host.front_at_exit().as_deref(),
        &rig.host.left_behind(),
        false,
    );
    assert_eq!(
        lines,
        vec![atomcode::background_resume_hint_line(
            "atomcode", &first, false
        )],
        "the stopped background session's line, said as a background one"
    );
    assert!(
        lines[0].contains(&format!("atomcode resume {first}")),
        "{lines:?}"
    );
}

/// **The foreground `resume` line is for the session in front at exit**, not
/// the one the screen started on: after `/bg`, something said in the new
/// foreground, then quitting — the line names the new session, and the one
/// it started on is named once, as the background session it now is.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn the_resume_line_on_exit_names_the_session_in_front_then() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("slow task");
    rig.until("the slow turn reached the model", |rig| {
        rig.script.started.load(Ordering::SeqCst) == 1
    })
    .await;
    rig.term.type_line("/bg");
    rig.until_screen(&t(Msg::BgPanelMoved)).await;
    let fresh = rig.client.root();
    assert_ne!(fresh, first);
    // Say something in the new foreground: ← puts the panel away and stays
    // here (Esc would go back to the moved session).
    rig.term.press(KeyPress::plain(Key::Left));
    rig.until("the panel is put away", |rig| {
        !rig.term.text().contains(&*t(Msg::BgPanelMoved))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = rig.script.count.load(Ordering::SeqCst);
    rig.term.type_line("hello there");
    rig.until("the new foreground's turn was answered", |rig| {
        rig.script.count.load(Ordering::SeqCst) > before
    })
    .await;
    rig.until_screen("hello there").await;
    assert_eq!(rig.client.root(), fresh, "still on the new foreground");

    rig.term.press(KeyPress::ctrl('d'));
    rig.until_screen(&t(Msg::BgQuitQuestion { count: 1 })).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until("the screen quit", |rig| rig.running.is_finished())
        .await;
    rig.until_background("the background runtime was stopped", |list| list.is_empty())
        .await;

    assert_eq!(rig.host.front_at_exit().as_deref(), Some(fresh.as_str()));
    let lines = atomcode::exit_resume_hints(
        "atomcode",
        rig.host.front_at_exit().as_deref(),
        &rig.host.left_behind(),
        false,
    );
    assert_eq!(
        lines,
        vec![
            atomcode::resume_hint_line("atomcode", &fresh, false, false),
            atomcode::background_resume_hint_line("atomcode", &first, false),
        ],
        "the session in front at exit first, then the background one, said as one"
    );
}

/// **Space on a session waiting for an answer does not reply to it.** Nothing
/// is sent; the panel says the session is waiting and Enter opens it.
///
/// On a host that does not bring background questions forward: on one that
/// does, the question is already up on the foreground and `/bg list` would be
/// typed into it.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn space_on_a_session_waiting_for_an_answer_says_to_open_it() {
    let rig = Rig::without_background_questions().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    rig.term.type_line("/bg list");
    rig.until_screen(&t(Msg::BgPlaceholder)).await;
    let before = rig.script.count.load(Ordering::SeqCst);
    rig.term.press(KeyPress::ch(' '));
    rig.until_screen(&t(Msg::BgReplyWaiting)).await;
    let title = rig.background().await[0].title.clone().unwrap_or_default();
    assert!(
        !rig.term
            .text()
            .contains(&*t(Msg::BgReplyTo { title: &title })),
        "no reply box was opened:\n{}",
        rig.term.text()
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        rig.script.count.load(Ordering::SeqCst),
        before,
        "nothing sent"
    );
    assert_eq!(rig.background().await[0].state, BackgroundState::Waiting);
    rig.quit().await;
}

/// **The panel answers the mouse.** A click selects a row without switching;
/// a second click on the selected row opens it; the wheel walks the list — so
/// after scrolling away, a click on that row only selects it again.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn the_panel_is_worked_with_the_mouse() {
    let rig = Rig::new().await;
    let first = rig.client.root();
    rig.term.type_line("/background hello");
    rig.until_background("one done", |list| {
        list.len() == 1 && list[0].state == BackgroundState::Done
    })
    .await;
    rig.term.type_line("/background there");
    rig.until_background("both done", |list| {
        list.len() == 2 && list.iter().all(|s| s.state == BackgroundState::Done)
    })
    .await;
    let second = rig.background().await[1].session.clone();
    rig.term.type_line("/bg list");
    rig.until_screen(&t(Msg::BgPlaceholder)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The two rows under the one heading, in slot order.
    let heading = t(Msg::BgGroupCompleted).into_owned();
    let rows = rig.term.screen();
    let at = rows
        .iter()
        .position(|row| row.contains(&heading))
        .expect("the completed heading is drawn") as u16;
    let row_of_second = at + 2;
    let click = |y: u16| {
        rig.term.pointer(atomcode_tui::surface::Click::Press, 10, y);
        rig.term
            .pointer(atomcode_tui::surface::Click::Release, 10, y);
    };

    click(row_of_second);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        rig.client.root(),
        first,
        "one click selects, it does not open"
    );

    // Scroll back up: the selection leaves that row, so the next click on it
    // selects again rather than opening.
    rig.term
        .pointer(atomcode_tui::surface::Click::WheelUp, 10, row_of_second);
    tokio::time::sleep(Duration::from_millis(100)).await;
    click(row_of_second);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        rig.client.root(),
        first,
        "the wheel moved the selection away"
    );

    click(row_of_second);
    rig.until("a second click on the selected row opens it", |rig| {
        rig.client.root() == second
    })
    .await;
    rig.quit().await;
}

/// 后台会话挂着的那个问询要能整个取回来:id、载荷、以及画它要用的日志尾巴。
///
/// 这条脚本里的 "ask me" 走的是 `request_user_input` 工具的请求往返
/// (`request_user_input.rs:329-333`:「不是 `user-questions` 那个通道，
/// 没有 `Asked`/`Answered` 事实会为它落盘」)，所以尾巴里不会有一条
/// `SessionEvent::Asked` —— 断言只留「尾巴不是空的」，按 brief 的兜底走。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_background_question_comes_back_whole() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let target = rig.background().await[0].session.clone();

    let reply = rig
        .control()
        .call(HostCommand::BackgroundQuestion {
            target: target.clone(),
        })
        .await
        .expect("在等的会话有问询可取");
    let HostReply::BackgroundQuestion {
        session,
        kind,
        payload,
        facts,
        ..
    } = reply
    else {
        panic!("不是一条问询: {reply:?}");
    };
    assert_eq!(session, target);
    assert!(!kind.is_empty(), "kind 要说得出是什么问法");
    assert!(payload.is_object(), "载荷要原样带回来: {payload:?}");
    assert!(!facts.is_empty(), "尾巴不能是空的");
    rig.quit().await;
}

/// 答了,那个后台会话就不再等它了 —— 而且挂着的那个确实被这一次答掉了。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn answering_a_background_question_clears_what_it_was_waiting_on() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let target = rig.background().await[0].session.clone();
    let HostReply::BackgroundQuestion { id, .. } = rig
        .control()
        .call(HostCommand::BackgroundQuestion {
            target: target.clone(),
        })
        .await
        .expect("在等的会话有问询可取")
    else {
        panic!("不是一条问询");
    };

    // `Value::Null` 是任何 kind 都收得下的「没有答案」,与屏幕画不出来时的答复一致
    // (`plugin.rs:4020-4024`)。这一条测的是**送达与记账**;答的语义由端到端那条测。
    rig.control()
        .call(HostCommand::AnswerBackground {
            target: target.clone(),
            id,
            value: serde_json::Value::Null,
        })
        .await
        .expect("挂着的就是它,答得进去");

    rig.until_background("it moved on", |list| {
        list.first()
            .is_some_and(|s| s.state != BackgroundState::Waiting)
    })
    .await;

    // 同一个 id 再答一次:挂着的已经不是它了,明说,而不是塞给一个等着别的东西的 runtime。
    assert!(matches!(
        rig.control()
            .call(HostCommand::AnswerBackground {
                target: target.clone(),
                id,
                value: serde_json::Value::Null,
            })
            .await,
        Err(HostError::Busy { .. })
    ));
    rig.quit().await;
}

/// 不在后台的会话没得答:如实说找不到。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn answering_a_session_that_is_not_in_the_background_is_not_found() {
    let rig = Rig::new().await;
    assert!(matches!(
        rig.control()
            .call(HostCommand::AnswerBackground {
                target: "nobody".into(),
                id: 1,
                value: serde_json::Value::Null,
            })
            .await,
        Err(HostError::NotFound)
    ));
    rig.quit().await;
}

/// 后台会话的问询**直接出现在前台**,人不用先 `/bg 1` 切过去 —— 而且那个会话还在后台。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_background_question_comes_out_on_the_foreground() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;

    let list = rig.background().await;
    let title = list[0].title.clone().unwrap_or_default();
    // 屏幕上那句话得说得出是谁在问 —— 没有这一句,人会以为是自己那段对话在问。
    let who = t(Msg::BgAsker {
        slot: 1,
        title: &title,
    });
    rig.until_screen(&who).await;

    // 而且它没有被换到前台:人还留在自己那段对话里(设计 §4 的目标边界)。
    assert_eq!(
        rig.background().await.first().map(|s| s.session.clone()),
        Some(list[0].session.clone()),
        "它还在后台"
    );
}

/// 后台一次问两条，提到前台的**是那两条**（第一页先出），而不是只把第一条当成一条单问询。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_background_batch_comes_out_as_a_batch() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask two");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;

    let title = rig.background().await[0].title.clone().unwrap_or_default();
    rig.until_screen(&t(Msg::BgAsker {
        slot: 1,
        title: &title,
    }))
    .await;
    // 批是一块面板、一页一条：第一条在屏幕上就说明它整批都提上来了
    // （只提第一条的话，`batch_for` 根本不会被走到）。
    rig.until_screen("Which one?").await;
}

/// **在前台回答后台的问询，答案真的到了那个会话**：它接着跑，人没有离开自己那段对话。
///
/// 这是设计 §5.3 那条路由的判据 —— Task 3 只判到「挂着的被答掉了」，判不到答案是
/// 不是进了**那个 runtime**；这一条靠那个会话自己的下一个回合来证明。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn answering_it_from_the_foreground_sends_the_answer_to_that_session() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let title = rig.background().await[0].title.clone().unwrap_or_default();

    // 它在当前这块屏上等着 —— 不用 /bg 切过去。
    rig.until_screen("Which one?").await;
    rig.until_screen(&t(Msg::BgAsker {
        slot: 1,
        title: &title,
    }))
    .await;

    // 就地答掉：面板亮着第一行，enter 取它（`answer_question` 的规矩）。
    rig.term.press(KeyPress::plain(Key::Enter));

    // 它接着跑：不在等人了。答案真进了那个 runtime，才会这样。
    rig.until_background("the background session moved on", |list| {
        list.first()
            .is_some_and(|s| s.state != BackgroundState::Waiting)
    })
    .await;

    // 而人还留在自己那段对话里：后面还能照常打字。
    rig.term.type_line("still here");
    rig.until_screen("still here").await;
    rig.quit().await;
}

/// 屏幕上正问着一个后台会话，它被 `/bg drop` 了：问询收回，**没有任何答案**被送出去
/// ——收回是取消，不是替人拒绝。
///
/// 设计 §6「等答案的任务必须分开 `Err` 与 `Ok(None)`」的判据。Task 6 的代码写的是
/// `let Ok(chosen) = answer.await else { return };`，这一条防的是以后有人照前台
/// `ask()` 的 `.ok().flatten()` / `unwrap_or_default()` 改回去——那样收回会变成一次
/// 拒绝发出去。不能只断言「那个 runtime 没收到答案」：被 drop 的 runtime 已经停了，
/// 照抄出来的拒绝会被 `NotFound` 挡掉，这条判据会碰巧绿——所以断言的是
/// [`Rig::answers_sent`]：整个进程里一次 `AnswerBackground` 都没发生过。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn a_dropped_background_question_is_withdrawn_not_refused() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let target = rig.background().await[0].session.clone();
    rig.until_screen("Which one?").await;

    // 问询面板亮着,键会被它吃掉——直接调宿主命令,而不是把 `/bg drop 1` 打进输入框。
    rig.control()
        .call(HostCommand::DropBackground { target })
        .await
        .expect("挂着的那个能被收回");
    rig.until_background("it is gone", |list| list.is_empty())
        .await;

    // 问询不在屏幕上了。
    rig.until_screen_lacks("Which one?").await;
    // 并且一个 AnswerBackground 都没发过。
    assert_eq!(rig.answers_sent(), 0, "收回不是拒绝");
    rig.quit().await;
}

/// 前台答掉一个后台会话的问询,宿主**当场**告诉屏幕它不在等了 —— 不等那个会话
/// 自己下一步跑出什么来。
///
/// 不说的话,屏幕手里的列表还当它在等:提示行指着它,下一次提问询又挑中它、拿回
/// `NotFound`,排在后面那个真在等的会话就出不来。判的是 `AnswerBackground` 一返回,
/// 订阅里**已经**有一条把它标成不在等的 `BackgroundChanged` —— 那个 runtime 自己
/// 收到答复、跑下一步、再报列表,要晚得多。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn answering_it_tells_the_screen_at_once_that_it_is_not_waiting() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let target = rig.background().await[0].session.clone();
    let HostReply::BackgroundQuestion { id, .. } = rig
        .control()
        .call(HostCommand::BackgroundQuestion {
            target: target.clone(),
        })
        .await
        .expect("在等的会话有问询可取")
    else {
        panic!("不是一条问询");
    };
    let mut events = rig.host.subscribe();
    rig.control()
        .call(HostCommand::AnswerBackground {
            target: target.clone(),
            id,
            value: serde_json::Value::Null,
        })
        .await
        .expect("挂着的就是它,答得进去");
    let mut told = false;
    while let Ok(event) = events.try_recv() {
        if let atomcode_host_api::HostEvent::BackgroundChanged { sessions } = event {
            told |= sessions
                .iter()
                .any(|s| s.session == target && s.state != BackgroundState::Waiting);
        }
    }
    assert!(told, "答复一送出,列表就该说它不在等了");
    rig.quit().await;
}

/// 两个后台会话都在等:前台答掉第一个,第二个的问询接着出现在前台 —— 而不是被
/// 一张还当第一个在等的旧列表挡在后面。
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(atomcode_home)]
async fn answering_one_background_question_brings_up_the_next_one() {
    let rig = Rig::new().await;
    rig.term.type_line("/background ask me");
    rig.until_background("the first background session is waiting", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    // 第一条问询一上屏,键就归它了 —— 第二个会话由宿主命令起,不经输入框。
    rig.control()
        .call(HostCommand::StartBackground {
            text: "ask me".into(),
            scope: None,
        })
        .await
        .expect("第二个后台会话起得来");
    rig.until_background("both background sessions are waiting", |list| {
        list.len() == 2 && list.iter().all(|s| s.state == BackgroundState::Waiting)
    })
    .await;
    let list = rig.background().await;
    let title = |at: usize| list[at].title.clone().unwrap_or_default();
    let (first, second) = (title(0), title(1));
    rig.until_screen(&t(Msg::BgAsker {
        slot: 1,
        title: &first,
    }))
    .await;

    // 就地答掉第一个。
    rig.term.press(KeyPress::plain(Key::Enter));

    // 第二个的问询接着上来。
    rig.until_screen(&t(Msg::BgAsker {
        slot: 2,
        title: &second,
    }))
    .await;
    rig.term.press(KeyPress::plain(Key::Enter));
    rig.until_background("both moved on", |list| {
        list.len() == 2 && list.iter().all(|s| s.state != BackgroundState::Waiting)
    })
    .await;
    rig.quit().await;
}
