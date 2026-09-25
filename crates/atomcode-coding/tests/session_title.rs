//! Naming a session: when it happens, who answers, and who wins.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use atomcode_harness::session::SessionEvent;
use atomcode_harness::{bundle, create_agent, plugins, run_turn};
use atomcode_plexus::{App, ConfigTree, Layer};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-title-{}-{tag}-{n}", std::process::id()));
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
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n",
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

fn talker(steps: &[&str]) -> String {
    let list = steps
        .iter()
        .map(|t| format!("{{ text = {t:?} }}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {list} ] }}")
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

/// The title is asked for in the background; give it a moment.
async fn titled(agent: &atomcode_harness::agent::Agent) -> Option<String> {
    for _ in 0..100 {
        if let Some(t) = agent.session().title() {
            return Some(t);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

fn titles(agent: &atomcode_harness::agent::Agent) -> Vec<String> {
    agent
        .session()
        .events()
        .into_iter()
        .filter_map(|e| match e.event {
            SessionEvent::Titled { title, .. } => Some(title),
            _ => None,
        })
        .collect()
}

const MODEL_NAMER: &str =
    "[[patch]]\nid = \"session-title-first-prompt\"\nname = \"session-title-model\"";

#[tokio::test]
async fn the_first_prompt_names_the_session_once() {
    let dir = scratch("first");
    let app = start(tree(&dir, &talker(&["ok", "ok"]), &[])).await;
    let agent = create_agent(&app).await.unwrap();
    run_turn(
        &app,
        "make the build stop failing on windows please, it is urgent",
    )
    .await
    .unwrap();
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("make the build stop failing on windows please"),
        "eight words of the first prompt, logged as a fact"
    );
    run_turn(&app, "and then update the changelog")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(titles(&agent).len(), 1, "named once, not once per prompt");
}

#[tokio::test]
async fn the_utility_model_names_it_when_the_row_says_so() {
    let dir = scratch("model");
    // The conversation's script and the namer's script are different rows, so
    // the title comes from the utility model and the conversation keeps its
    // own answers.
    let utility = "[[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
                   config = { script = [ { text = \"\\\"Windows build fix.\\\"\" } ] }";
    let app = start(tree(
        &dir,
        &talker(&["the answer", "the answer"]),
        &[MODEL_NAMER, utility],
    ))
    .await;
    let agent = create_agent(&app).await.unwrap();
    let outcome = run_turn(&app, "make the build stop failing on windows please")
        .await
        .unwrap();
    assert_eq!(
        outcome.text, "the answer",
        "the conversation's script was not eaten"
    );
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("Windows build fix"),
        "quotes and the trailing period are tidied away"
    );
}

/// A provider that always answers with one line.
struct Says(&'static str);

#[async_trait::async_trait]
impl atomcode_kernel::provider::LlmProvider for Says {
    fn model_name(&self) -> &str {
        "says"
    }
    async fn chat_stream(
        &self,
        _messages: &[atomcode_kernel::message::Message],
        _tools: &[atomcode_kernel::tool::ToolDef],
        _options: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        use atomcode_kernel::stream::StreamEvent;
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta(self.0.into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// A host catalog with no ranks anywhere — so `llm-utility-selected` would
/// leave the utility slot empty — whose current model answers `"Repo tour."`.
struct Unranked;

#[async_trait::async_trait]
impl atomcode_harness::seams::Models for Unranked {
    fn list(&self) -> Vec<atomcode_harness::seams::ModelInfo> {
        vec![atomcode_harness::seams::ModelInfo {
            id: "the-conversation".into(),
            display_name: "the conversation's model".into(),
            context_window: 128_000,
            supports_vision: false,
            capable_rank: None,
            effort_levels: Vec::new(),
            note: None,
            account: "here".into(),
        }]
    }
    fn current(&self) -> Option<String> {
        Some("the-conversation".into())
    }
    async fn provider(
        &self,
        id: &str,
    ) -> Result<std::sync::Arc<dyn atomcode_kernel::provider::LlmProvider>, String> {
        assert_eq!(
            id, "the-conversation",
            "borrows the model the conversation is on"
        );
        Ok(std::sync::Arc::new(Says("\"Repo tour.\"")))
    }
}

struct UnrankedPlugin;

#[async_trait::async_trait]
impl atomcode_plexus::Plugin for UnrankedPlugin {
    fn name(&self) -> &'static str {
        "test-models-unranked"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["models"]
    }
    fn description(&self) -> &'static str {
        "test: a host catalog with no ranks"
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _config: &serde_json::Value,
    ) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::ModelsSvc>(std::sync::Arc::new(Unranked))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test]
async fn without_a_utility_model_the_conversation_model_names_it() {
    let dir = scratch("borrow");
    // `session-title-model` mounted — the person asked for model-made names —
    // and no model is ranked, so there is no utility model. The host's
    // catalog still serves the conversation's own.
    let models = "[[insert]]\nid = \"models\"\nname = \"test-models-unranked\"";
    let mut catalog = plugins::catalog();
    catalog.register(std::sync::Arc::new(UnrankedPlugin));
    let mut app = App::new(
        catalog,
        tree(&dir, &talker(&["the answer"]), &[MODEL_NAMER, models]),
    );
    app.start().await.expect("must mount");
    let agent = create_agent(&app).await.unwrap();
    let outcome = run_turn(&app, "what is this repository").await.unwrap();
    assert_eq!(outcome.text, "the answer");
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("Repo tour"),
        "named by the conversation's model, not cut from the prompt"
    );
}

#[tokio::test]
async fn with_nothing_to_borrow_the_script_is_not_eaten() {
    let dir = scratch("no-catalog");
    // No utility row and no catalog: a fixture driving a scripted `llm`. The
    // namer must not reach for that script — the turn gets its one line.
    let app = start(tree(&dir, &talker(&["only one answer"]), &[MODEL_NAMER])).await;
    let agent = create_agent(&app).await.unwrap();
    let outcome = run_turn(&app, "what is this repository").await.unwrap();
    assert_eq!(outcome.text, "only one answer");
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("what is this repository")
    );
}

#[tokio::test]
async fn a_namer_that_gets_nothing_back_leaves_the_first_prompt() {
    let dir = scratch("fallback");
    // The utility call fails, as a dead gateway or a bad key does: the
    // session is still named, from what the person said.
    let utility = "[[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-replay\"\n\
                   config = { script = [ { fail = \"gateway down\" } ] }";
    let app = start(tree(
        &dir,
        &talker(&["only one answer"]),
        &[MODEL_NAMER, utility],
    ))
    .await;
    let agent = create_agent(&app).await.unwrap();
    let outcome = run_turn(&app, "what is this repository").await.unwrap();
    assert_eq!(outcome.text, "only one answer");
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("what is this repository")
    );
}

/// A side-call model that thinks before it answers, the way the reasoning
/// models behind openai-compat do: thinking and answer come out of ONE
/// `max_tokens`, thinking first. Given a cap below what the thinking takes, the
/// stream ends `finish_reason=length` with no visible text at all.
struct ThinkingUtility;

const THINKING_TOKENS: u32 = 120;

#[async_trait::async_trait]
impl atomcode_kernel::provider::LlmProvider for ThinkingUtility {
    fn model_name(&self) -> &str {
        "thinking-utility"
    }
    async fn chat_stream(
        &self,
        _messages: &[atomcode_kernel::message::Message],
        _tools: &[atomcode_kernel::tool::ToolDef],
        options: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        use atomcode_kernel::stream::StreamEvent;
        let mut events = vec![StreamEvent::Reasoning("let me think ".repeat(40))];
        match options.max_tokens {
            Some(cap) if cap <= THINKING_TOKENS => {
                events.push(StreamEvent::Done { truncated: true });
            }
            _ => {
                events.push(StreamEvent::TextDelta("Windows build fix".into()));
                events.push(StreamEvent::Done { truncated: false });
            }
        }
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct ThinkingUtilityPlugin;

#[async_trait::async_trait]
impl atomcode_plexus::Plugin for ThinkingUtilityPlugin {
    fn name(&self) -> &'static str {
        "llm-utility-thinking"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm-utility"]
    }
    fn description(&self) -> &'static str {
        "test: a side-call model that thinks before it answers"
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _config: &serde_json::Value,
    ) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmUtilitySvc>(std::sync::Arc::new(ThinkingUtility))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test]
async fn a_model_that_thinks_first_is_not_starved_by_an_output_cap() {
    let dir = scratch("thinking");
    let utility = "[[insert]]\nid = \"llm-utility\"\nname = \"llm-utility-thinking\"";
    let mut catalog = plugins::catalog();
    catalog.register(std::sync::Arc::new(ThinkingUtilityPlugin));
    let mut app = App::new(
        catalog,
        tree(&dir, &talker(&["ok"]), &[MODEL_NAMER, utility]),
    );
    app.start().await.expect("must mount");
    let agent = create_agent(&app).await.unwrap();
    run_turn(&app, "make the build stop failing on windows please")
        .await
        .unwrap();
    assert_eq!(
        titled(&agent).await.as_deref(),
        Some("Windows build fix"),
        "a title is a handful of tokens, but the thinking before it is not: a cap \
         sized for the title is a cap the thinking eats"
    );
}

#[tokio::test]
async fn a_name_the_person_gave_is_kept() {
    let dir = scratch("kept");
    let app = start(tree(&dir, &talker(&["ok"]), &[])).await;
    let agent = create_agent(&app).await.unwrap();
    atomcode_harness::session::commit(
        &app.context(),
        &agent.session(),
        SessionEvent::Titled {
            turn: 0,
            title: "mine".into(),
            user_set: true,
        },
    );
    run_turn(&app, "something else entirely").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(agent.session().title().as_deref(), Some("mine"));
    assert_eq!(titles(&agent), vec!["mine".to_string()]);
}

#[tokio::test]
async fn removing_the_policy_row_leaves_sessions_unnamed() {
    let dir = scratch("no-policy");
    let app = start(tree(
        &dir,
        &talker(&["ok"]),
        &["[[remove]]\nid = \"session-title-on-first-prompt\""],
    ))
    .await;
    let agent = create_agent(&app).await.unwrap();
    run_turn(&app, "hello").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(agent.session().title().is_none());
}
