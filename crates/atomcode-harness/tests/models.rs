//! Delegating to a different model.
//!
//! The claim under test is not "there is a `model` argument". It is:
//!
//! * a delegated child actually TALKS to the provider that was named — judged
//!   by which provider was asked to open a stream, not by what the tool said;
//! * an id the catalog does not offer is REFUSED, with the list, rather than
//!   quietly falling back to the conversation's model;
//! * a model stronger than the conversation's is not on offer at all, so the
//!   model cannot spend more than the person chose to;
//! * and the system prompt says nothing that moves when the catalog does —
//!   because the catalog changes with every login, and that fragment is a
//!   cache prefix.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::seams::{
    cheapest, delegatable, ModelInfo, Models, ModelsSvc, SystemPromptSvc,
};
use atomcode_harness::{bundle, plugins};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::ToolDef;
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};
use futures::stream::BoxStream;
use serde_json::Value;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-models-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

// ---- a model that signs its work ----------------------------------------

/// One recorded request: which provider, and at what thinking level.
///
/// The judge for both halves of a delegation. A child that ran on the wrong
/// provider records the wrong name; one whose `effort` was dropped on the way
/// records `None`. Neither is visible in anything the tool says.
type Calls = Arc<Mutex<Vec<(String, Option<String>)>>>;

fn who(calls: &Calls) -> Vec<String> {
    calls
        .lock()
        .expect("calls poisoned")
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

/// Answers one line naming itself, and records the request it was given.
struct Signed {
    id: String,
    calls: Calls,
}

#[async_trait]
impl LlmProvider for Signed {
    fn model_name(&self) -> &str {
        &self.id
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.calls.lock().expect("calls poisoned").push((
            self.id.clone(),
            options.reasoning_effort.map(|e| e.as_str().to_string()),
        ));
        let events = vec![
            StreamEvent::TextDelta(format!("ran on {}", self.id)),
            StreamEvent::Usage(TokenUsage {
                prompt: 10,
                completion: 2,
                ..Default::default()
            }),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// The parent: delegates once with the given arguments, then answers.
struct Lead {
    args: String,
    calls: Calls,
    round: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for Lead {
    fn model_name(&self) -> &str {
        "lead-model"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.calls.lock().expect("calls poisoned").push((
            "lead-model".into(),
            options.reasoning_effort.map(|e| e.as_str().to_string()),
        ));
        let n = self.round.fetch_add(1, Ordering::SeqCst);
        let events = if n == 0 {
            vec![StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                id: "c1".into(),
                name: "task".into(),
                arguments: self.args.clone(),
            })]
        } else {
            vec![StreamEvent::TextDelta("done".into())]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

// ---- a catalog, and the rows that carry these two into a tree ------------

struct TestCatalog {
    offer: Vec<ModelInfo>,
    current: Option<String>,
    calls: Calls,
}

#[async_trait]
impl Models for TestCatalog {
    fn list(&self) -> Vec<ModelInfo> {
        self.offer.clone()
    }
    fn current(&self) -> Option<String> {
        self.current.clone()
    }
    async fn provider(&self, id: &str) -> Result<Arc<dyn LlmProvider>, String> {
        Ok(Arc::new(Signed {
            id: id.to_string(),
            calls: self.calls.clone(),
        }))
    }
}

fn model(id: &str, rank: i64) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        display_name: format!("{id} (display)"),
        context_window: 128_000,
        supports_vision: false,
        capable_rank: Some(rank),
        effort_levels: Vec::new(),
        note: None,
    }
}

struct ProvideCatalog(Arc<TestCatalog>);

#[async_trait]
impl Plugin for ProvideCatalog {
    fn name(&self) -> &'static str {
        "test-models"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["models"]
    }
    fn description(&self) -> &'static str {
        "a fixed catalog"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<ModelsSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct ProvideLead(Arc<Lead>);

#[async_trait]
impl Plugin for ProvideLead {
    fn name(&self) -> &'static str {
        "test-lead-model"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "the conversation's own model"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// A tree that delegates once, with `args`, over `offer`.
async fn delegate_with(
    tag: &str,
    args: &str,
    offer: Vec<ModelInfo>,
    current: Option<&str>,
) -> (App, Calls) {
    let root = scratch(tag);
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let catalog = Arc::new(TestCatalog {
        offer,
        current: current.map(str::to_string),
        calls: calls.clone(),
    });
    let lead = Arc::new(Lead {
        args: args.to_string(),
        calls: calls.clone(),
        round: std::sync::atomic::AtomicUsize::new(0),
    });

    let scoped = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 6, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"approval\"\nconfig = {{ mode = \"yolo\" }}\n\n\
         [[patch]]\nid = \"subagent-in-process\"\ndisabled = false\n\n\
         [[patch]]\nid = \"llm\"\nname = \"test-lead-model\"\nconfig = {{}}\n\n\
         [[insert]]\nname = \"test-models\"\n\n\
         [[insert]]\nname = \"model-catalog\"\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy(),
    );
    let tree = ConfigTree::from_layers(vec![
        bundle::base().unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ])
    .expect("tree");

    let mut registry = plugins::catalog();
    registry.register(Arc::new(ProvideCatalog(catalog)));
    registry.register(Arc::new(ProvideLead(lead)));
    let mut app = App::new(registry, tree);
    app.start().await.expect("must mount");
    atomcode_harness::run_turn(&app, "delegate it")
        .await
        .expect("a turn");
    (app, calls)
}

fn transcript(app: &App) -> String {
    use atomcode_harness::agent::OnlySession;
    app.context()
        .only_session()
        .map(|log| {
            log.derive_messages()
                .iter()
                .map(|m| m.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

// ---- the criteria --------------------------------------------------------

#[tokio::test]
async fn a_named_model_is_the_one_the_child_talks_to() {
    let (app, calls) = delegate_with(
        "named",
        r#"{"task":"look around","model":"cheap"}"#,
        vec![model("cheap", 10), model("lead-model", 30)],
        Some("lead-model"),
    )
    .await;

    let calls = who(&calls);
    assert!(
        calls.iter().any(|c| c == "cheap"),
        "the child never opened a stream on `cheap`; who was called: {calls:?}"
    );
    // And the lead kept its own: a child's realm is the child's.
    assert!(
        calls.iter().filter(|c| *c == "lead-model").count() >= 2,
        "the lead must still be running on its own model, before and after: {calls:?}"
    );
    let said = transcript(&app);
    assert!(
        said.contains("ran on cheap"),
        "the child's report should carry the model that produced it:\n{said}"
    );
    drop(app);
}

#[tokio::test]
async fn an_id_the_catalog_does_not_offer_is_refused_with_the_list() {
    let (app, calls) = delegate_with(
        "unknown",
        r#"{"task":"look around","model":"something-else"}"#,
        vec![model("cheap", 10), model("lead-model", 30)],
        Some("lead-model"),
    )
    .await;

    let calls = who(&calls);
    assert!(
        !calls.iter().any(|c| c == "something-else" || c == "cheap"),
        "a model that was not on offer must not be reached, and must not be \
         silently swapped for one that was: {calls:?}"
    );
    let said = transcript(&app);
    assert!(
        said.contains("not available to delegate to") && said.contains("cheap"),
        "the refusal has to name what IS on offer, or the model can only guess \
         again:\n{said}"
    );
    drop(app);
}

#[tokio::test]
async fn nothing_stronger_than_this_conversation_is_on_offer() {
    // The ceiling, at the tool: `strong` outranks the conversation, so naming it
    // is refused exactly like a typo would be.
    let (app, calls) = delegate_with(
        "ceiling",
        r#"{"task":"look around","model":"strong"}"#,
        vec![
            model("cheap", 10),
            model("lead-model", 30),
            model("strong", 99),
        ],
        Some("lead-model"),
    )
    .await;

    let calls = who(&calls);
    assert!(
        !calls.iter().any(|c| c == "strong"),
        "a subagent must not be able to spend more than the person chose: {calls:?}"
    );
    let said = transcript(&app);
    assert!(
        !said.contains("`strong`,") && said.contains("not available to delegate to"),
        "`strong` must not appear in the list of what may be used:\n{said}"
    );
    drop(app);
}

/// A rank excludes; it never admits.
///
/// The first version of this rule required one, and on the shipped config —
/// where exactly one model of four carries a rank — that left the catalog empty
/// and the feature dead. The fixes on offer were "change what the gateway sends"
/// and "make the person edit config.toml", which is what a rule that cannot
/// degrade looks like from the outside.
#[test]
fn what_is_offered_when_the_deployment_has_not_said_much() {
    struct Fixed(Vec<ModelInfo>, Option<String>);
    #[async_trait]
    impl Models for Fixed {
        fn list(&self) -> Vec<ModelInfo> {
            self.0.clone()
        }
        fn current(&self) -> Option<String> {
            self.1.clone()
        }
        async fn provider(&self, _id: &str) -> Result<Arc<dyn LlmProvider>, String> {
            Err("not needed".into())
        }
    }
    let ids = |ms: Vec<ModelInfo>| ms.into_iter().map(|m| m.id).collect::<Vec<_>>();
    let unranked = |id: &str| {
        let mut m = model(id, 0);
        m.capable_rank = None;
        m
    };

    // Everything ranked: the ceiling, as before.
    let all = vec![model("cheap", 10), model("mid", 30), model("strong", 99)];
    assert_eq!(
        ids(delegatable(&Fixed(all.clone(), Some("mid".into())))),
        vec!["cheap", "mid"],
        "weakest first, the conversation included, nothing above it"
    );

    // The shipped shape: three models nobody ordered, one marked as the strong
    // tier, and the conversation on an unordered one.
    let real = vec![
        unranked("qwen"),
        unranked("deepseek"),
        unranked("glm-pro"),
        model("longyuan", 100),
    ];
    assert_eq!(
        ids(delegatable(&Fixed(real.clone(), Some("glm-pro".into())))),
        vec!["deepseek", "glm-pro", "qwen"],
        "no ranks anywhere to compare ⇒ they are siblings and all three are \
         offered; the one the deployment DID mark as a tier is withheld, because \
         a mark against an unmarked conversation cannot be placed below it"
    );
    assert!(
        cheapest(&Fixed(real.clone(), Some("glm-pro".into()))).is_none(),
        "and none of them can be SHOWN to be the cheap one, so the side-call slot \
         must stay empty rather than pick a favourite"
    );

    // The floor, stated on its own: whatever else is true, the model the person
    // chose is delegatable. Nothing about it needs deciding.
    for current in ["glm-pro", "longyuan"] {
        assert!(
            delegatable(&Fixed(real.clone(), Some(current.into())))
                .iter()
                .any(|m| m.id == current),
            "the conversation's own model must always be on the list ({current})"
        );
    }

    // A conversation the catalog does not contain: nothing to anchor to.
    assert!(
        delegatable(&Fixed(real, None)).is_empty(),
        "with no idea what this conversation runs on, there is no floor and no \
         ceiling — and `task` still runs, it just takes no `model`"
    );
}

#[tokio::test]
async fn the_prompt_says_nothing_that_moves_when_the_catalog_does() {
    // The catalog changes with every login, `/model` and config edit. The system
    // prompt is a CACHE PREFIX: a fragment carrying a count, a name or an
    // ordering would invalidate it each time the catalog moved, which is the
    // failure this whole design is shaped around.
    async fn fragment(tag: &str, offer: Vec<ModelInfo>) -> String {
        let (app, _) =
            delegate_with(tag, r#"{"task":"look around"}"#, offer, Some("lead-model")).await;
        let rendered = app
            .context()
            .service::<SystemPromptSvc>()
            .expect("system-prompt")
            .render();
        let line = rendered
            .lines()
            .find(|l| l.contains("delegated to a model"))
            .unwrap_or_default()
            .to_string();
        drop(app);
        line
    }

    let small = fragment(
        "prompt-small",
        vec![model("cheap", 10), model("lead-model", 30)],
    )
    .await;
    let large = fragment(
        "prompt-large",
        vec![
            model("a", 1),
            model("b", 2),
            model("c", 3),
            model("d", 4),
            model("lead-model", 30),
        ],
    )
    .await;

    assert!(
        !small.is_empty(),
        "the pointer must actually be in the prompt, or this proves nothing"
    );
    assert_eq!(
        small, large,
        "the system prompt moved with the catalog — that is a cache prefix \
         invalidated on every login, and a number that is wrong the moment it \
         is written"
    );
}

#[tokio::test]
async fn with_no_catalog_the_prompt_says_nothing_at_all() {
    let (app, _) = delegate_with(
        "empty",
        r#"{"task":"look around"}"#,
        // Ranked, but all above the conversation ⇒ nothing delegatable.
        vec![model("strong", 99)],
        Some("lead-model"),
    )
    .await;
    let rendered = app
        .context()
        .service::<SystemPromptSvc>()
        .expect("system-prompt")
        .render();
    assert!(
        !rendered.contains("delegated to a model"),
        "offering a choice that does not exist sends the model to a tool call \
         that can only answer `there is nothing`:\n{rendered}"
    );
    drop(app);
}

/// The floor, measured rather than asserted: **with no catalog at all, `task`
/// still runs, on the conversation's model.**
///
/// This is the thing that must never break, whatever the deployment has or has
/// not said about its models. Everything else in this file is about a choice;
/// this one is about there being no choice and the work happening anyway.
#[tokio::test]
async fn with_no_catalog_at_all_a_task_still_runs_on_the_conversation_model() {
    let root = scratch("no-catalog");
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let lead = Arc::new(Lead {
        args: r#"{"task":"look around"}"#.into(),
        calls: calls.clone(),
        round: std::sync::atomic::AtomicUsize::new(0),
    });
    let scoped = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 6, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\ndisabled = true\n\n\
         [[patch]]\nid = \"approval\"\nconfig = {{ mode = \"yolo\" }}\n\n\
         [[patch]]\nid = \"subagent-in-process\"\ndisabled = false\n\n\
         [[patch]]\nid = \"llm\"\nname = \"test-lead-model\"\nconfig = {{}}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy(),
    );
    // No `test-models` row, so the `models` seam is empty — the stock situation
    // for any host that has not built a catalog.
    let tree = ConfigTree::from_layers(vec![
        bundle::base().unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ])
    .expect("tree");
    let mut registry = plugins::catalog();
    registry.register(Arc::new(ProvideLead(lead)));
    let mut app = App::new(registry, tree);
    app.start().await.expect("must mount");
    atomcode_harness::run_turn(&app, "delegate it")
        .await
        .expect("a turn");

    // The judge is who was asked to open a stream, not what anyone said: the
    // lead opens one, the child opens one, the lead opens one more to answer.
    // Three on the conversation's own provider and nothing else anywhere.
    let calls = who(&calls);
    assert!(
        calls.len() >= 3 && calls.iter().all(|c| c == "lead-model"),
        "with no catalog the child must still run, on the conversation's own \
         model and on nothing else: {calls:?}"
    );
    let said = transcript(&app);
    assert!(
        !said.contains("no model catalog"),
        "and it must not have been told off about a catalog it never asked \
         for:\n{said}"
    );
    drop(app);
}

/// Asking for a model BY NAME with no catalog is still refused, and says why.
///
/// The floor above is about the default path. This is the other half: a refusal
/// that names the reason beats one that silently runs somewhere else.
#[tokio::test]
async fn naming_a_model_with_no_catalog_says_so() {
    let (app, calls) = delegate_with(
        "named-no-catalog",
        r#"{"task":"look around","model":"whatever"}"#,
        Vec::new(),
        None,
    )
    .await;
    let calls = who(&calls);
    assert!(
        !calls.iter().any(|c| c == "whatever"),
        "nothing should have been built: {calls:?}"
    );
    let said = transcript(&app);
    assert!(
        said.contains("not available to delegate to"),
        "the model has to learn that this id is not a thing here:\n{said}"
    );
    drop(app);
}

// ---- how hard the child thinks -------------------------------------------

fn thinking(id: &str, rank: i64, levels: &[&str]) -> ModelInfo {
    let mut m = model(id, rank);
    m.effort_levels = levels.iter().map(|l| l.to_string()).collect();
    m
}

/// A stated effort reaches the delegated request, and **only** it.
///
/// The level rides on the child's own realm, so the lead's next round is
/// untouched. Nothing in the tool's answer would show either fact.
#[tokio::test]
async fn an_effort_rides_with_the_child_and_not_the_lead() {
    let (app, calls) = delegate_with(
        "effort",
        r#"{"task":"look around","model":"cheap","effort":"low"}"#,
        vec![
            thinking("cheap", 10, &["low", "high"]),
            thinking("lead-model", 30, &["low", "high"]),
        ],
        Some("lead-model"),
    )
    .await;

    let calls = calls.lock().expect("calls poisoned").clone();
    let child: Vec<_> = calls.iter().filter(|(id, _)| id == "cheap").collect();
    assert!(
        child
            .iter()
            .all(|(_, effort)| effort.as_deref() == Some("low")),
        "the level the delegation asked for has to reach the wire: {calls:?}"
    );
    assert!(!child.is_empty(), "the child never ran: {calls:?}");
    assert!(
        calls
            .iter()
            .filter(|(id, _)| id == "lead-model")
            .all(|(_, effort)| effort.is_none()),
        "and it must not leak onto the lead's own rounds — the level is a fact \
         about the delegated job, not about the conversation: {calls:?}"
    );
    drop(app);
}

/// A level the chosen model does not advertise is refused, with its levels.
///
/// The alternative is worse than it sounds: the adapter drops an unsupported
/// `reasoning_effort` silently, so the child runs at whatever it would have
/// anyway and the person reads the transcript and concludes the level did not
/// help. A refusal that names the accepted levels is the only outcome that
/// teaches anyone anything.
#[tokio::test]
async fn an_effort_the_model_does_not_take_is_refused_with_its_levels() {
    let (app, calls) = delegate_with(
        "effort-unsupported",
        r#"{"task":"look around","model":"cheap","effort":"max"}"#,
        vec![
            thinking("cheap", 10, &["low", "high"]),
            thinking("lead-model", 30, &["low", "high"]),
        ],
        Some("lead-model"),
    )
    .await;

    assert!(
        !who(&calls).iter().any(|c| c == "cheap"),
        "it must not run anyway: {:?}",
        calls.lock().expect("calls poisoned")
    );
    let said = transcript(&app);
    assert!(
        said.contains("does not take `max`") && said.contains("low, high"),
        "the refusal has to say what this model DOES take:\n{said}"
    );
    drop(app);
}

/// A model that advertises no levels is not second-guessed.
///
/// Silence from the deployment means "nobody said", not "it accepts nothing" —
/// the same rule the rank filter runs on. The level goes out and the endpoint
/// decides; a client that refused here would make a thinking model unusable
/// wherever the catalog is thin, which is everywhere this feature has to work.
#[tokio::test]
async fn a_model_that_advertises_no_levels_still_takes_one() {
    let (app, calls) = delegate_with(
        "effort-unknown",
        r#"{"task":"look around","model":"cheap","effort":"max"}"#,
        vec![model("cheap", 10), model("lead-model", 30)],
        Some("lead-model"),
    )
    .await;

    let calls = calls.lock().expect("calls poisoned").clone();
    assert!(
        calls
            .iter()
            .any(|(id, effort)| id == "cheap" && effort.as_deref() == Some("max")),
        "an unstated level list is not a refusal: {calls:?}"
    );
    drop(app);
}

/// And something that is not a level at all is refused before anything runs.
#[tokio::test]
async fn a_word_that_is_not_a_level_is_refused() {
    let (app, calls) = delegate_with(
        "effort-nonsense",
        r#"{"task":"look around","effort":"very hard please"}"#,
        vec![thinking("lead-model", 30, &["low", "high"])],
        Some("lead-model"),
    )
    .await;

    // Only the lead's own two rounds; no child ever started.
    assert!(
        who(&calls).iter().all(|c| c == "lead-model"),
        "{:?}",
        calls.lock().expect("calls poisoned")
    );
    let said = transcript(&app);
    assert!(
        said.contains("is not a thinking level"),
        "the model has to learn the vocabulary, not just be told no:\n{said}"
    );
    drop(app);
}
