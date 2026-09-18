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
    let mounted = tui_front::mount(second, front_end, config, &screen, config_path)
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
