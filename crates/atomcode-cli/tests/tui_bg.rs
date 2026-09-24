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

use atomcode::background::{Spawn, Spawned};
use atomcode::tui_front;
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeStart, PrepareOptions,
    ProviderBuildError, SessionMode, StaticPluginHookSource, SubagentPolicy,
};
use atomcode_host_api::{BackgroundSession, BackgroundState, HostCommand, HostControl, HostReply};
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
        let text = if slow {
            self.started.fetch_add(1, Ordering::SeqCst);
            let mut gate = self.gate.clone();
            let _ = gate.wait_for(|open| *open).await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            "slow done".to_string()
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
) -> (CodingRuntimeStart, CodingAgentConfig) {
    let mut agent = CodingAgentConfig::new("key", "https://example.test/v1", "scripted", project);
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
                review: false,
                subagents: SubagentPolicy::Disabled,
                rate_limit_source: None,
                front_end: Some(front_end),
            },
            provider_factory: Arc::new(script.clone()),
            plugin_hooks: Arc::new(StaticPluginHookSource::default()),
            image_preprocessor: None,
        },
        agent,
    )
}

/// A screen with `/bg`, over a first runtime and a way to start more.
struct Rig {
    _home: tempfile::TempDir,
    _project: tempfile::TempDir,
    gate: watch::Sender<bool>,
    script: Script,
    term: Arc<atomcode_tui::surface::Headless>,
    client: Arc<atomcode_tui::plugin::AgentClient>,
    running: tokio::task::JoinHandle<()>,
    _mounted: atomcode_tui::launch::Mounted,
}

impl Rig {
    async fn new() -> Self {
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
        let (first, config) = start(project.path(), &script, front_end.clone());
        let runtime = CodingRuntime::start(first).await.expect("starts");
        let spawn: Spawn = {
            let script = script.clone();
            Arc::new(move |working_dir: std::path::PathBuf| {
                let script = script.clone();
                Box::pin(async move {
                    let front_end = FrontEnd::new();
                    let (start, config) = start(&working_dir, &script, front_end.clone());
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
        let (mounted, _) = tui_front::mount_with_background(
            runtime,
            front_end,
            config,
            None,
            &screen,
            home.path().join("config.toml"),
            None,
            None,
            Some(spawn),
        )
        .await
        .expect("the screen mounts");
        let ctx = mounted.app.context();
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
            _project: project,
            gate,
            script,
            term,
            client,
            running,
            _mounted: mounted,
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
        match self
            .control()
            .call(HostCommand::PreviewSession {
                session: session.to_string(),
            })
            .await
        {
            Ok(HostReply::SessionPreview { lines }) => lines.join("\n"),
            other => panic!("the log of {session} can be read: {other:?}"),
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

    // ctrl+x on the selected row is `/bg drop`.
    rig.term.press(KeyPress {
        key: Key::Char('x'),
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

/// **The panel's box starts a task, and the list shows it.** Typed into the
/// panel rather than as a command — the same host command underneath.
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
async fn a_question_asked_in_the_background_is_asked_again_on_return() {
    let rig = Rig::new().await;
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

/// **Space replies to a background session in place.** The words go to the
/// selected session — its log has them and the model answered — and the screen
/// stays where it was.
#[tokio::test(flavor = "multi_thread")]
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
    let before = rig.script.count.load(Ordering::SeqCst);

    rig.term.type_line("/bg list");
    rig.until_screen(&t(Msg::BgPlaceholder)).await;
    rig.term.press(KeyPress::ch(' '));
    rig.term.type_line("one more thing");
    rig.until("the reply reached the model", |rig| {
        rig.script.count.load(Ordering::SeqCst) > before
    })
    .await;
    rig.until_background("the reply's turn is done", |list| {
        list.first()
            .is_some_and(|s| s.state == BackgroundState::Done)
    })
    .await;
    assert!(rig.stored(&task).await.contains("one more thing"));
    assert_eq!(rig.client.root(), first, "the screen did not switch");
    rig.quit().await;
}

/// **`/bg` is refused while the conversation waits for an answer**, and
/// nothing moves: moving it would leave the question with nobody to ask.
#[tokio::test(flavor = "multi_thread")]
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
