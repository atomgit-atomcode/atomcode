//! The properties the plugin runtime exists to provide, asserted end to end.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_plexus::{
    plexus_event, plexus_service, App, ConfigTree, Context, Layer, Listener, Next, Plugin,
    PluginRegistry, Waterfall,
};
use serde_json::{json, Value};

// ---- a seam: one service definition, two interchangeable providers ------

pub trait Greeter: Send + Sync {
    fn greet(&self, who: &str) -> String;
}

plexus_service!(GreeterSvc => dyn Greeter, "greeter", Seam, "Greeting style");

struct Polite;
impl Greeter for Polite {
    fn greet(&self, who: &str) -> String {
        format!("Good evening, {who}.")
    }
}

struct Terse;
impl Greeter for Terse {
    fn greet(&self, who: &str) -> String {
        format!("yo {who}")
    }
}

struct PolitePlugin;
#[async_trait]
impl Plugin for PolitePlugin {
    fn name(&self) -> &'static str {
        "greeter-polite"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["greeter"]
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<GreeterSvc>(Arc::new(Polite))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct TersePlugin;
#[async_trait]
impl Plugin for TersePlugin {
    fn name(&self) -> &'static str {
        "greeter-terse"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["greeter"]
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<GreeterSvc>(Arc::new(Terse))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Where plugins record what they did, so a test can observe them without global
/// state. A service whose face is a concrete type rather than a trait object —
/// not every slot is a swappable seam.
pub struct SinkSvc;
impl atomcode_plexus::ServiceKey for SinkSvc {
    const NAME: &'static str = "sink";
    const MODE: atomcode_plexus::SeamMode = atomcode_plexus::SeamMode::Core;
    const TITLE: &'static str = "Where plugins record what they did";
    type Face = Mutex<Vec<String>>;
}

plexus_event!(Greet, "test/greet", Serial, String => String);

/// Resolves the seam **per call**, which is the behaviour that makes swapping a
/// provider invisible to consumers.
struct GreetListener {
    ctx: Context,
}

#[async_trait]
impl Listener<Greet> for GreetListener {
    async fn call(&self, who: &String) -> Option<String> {
        let greeter = self.ctx.service::<GreeterSvc>()?;
        Some(greeter.greet(who))
    }
}

struct ConsumerPlugin;
#[async_trait]
impl Plugin for ConsumerPlugin {
    fn name(&self) -> &'static str {
        "consumer"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["greeter", "sink"]
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let who = config.get("who").and_then(Value::as_str).unwrap_or("world");
        let greeter = ctx.require::<GreeterSvc>().map_err(|e| e.to_string())?;
        ctx.require::<SinkSvc>()
            .map_err(|e| e.to_string())?
            .lock()
            .unwrap()
            .push(greeter.greet(who));
        let _ = ctx.on_serial::<Greet>(Arc::new(GreetListener { ctx: ctx.clone() }));
        Ok(())
    }
}

/// Build an app with a fresh sink already in the root fiber's service table.
fn app_with_sink(toml: &str) -> (App, Arc<Mutex<Vec<String>>>) {
    let layer = Layer::from_toml(toml).unwrap();
    let app = App::new(registry(), ConfigTree::from_layers([layer]).unwrap());
    let sink = Arc::new(Mutex::new(Vec::new()));
    let _ = app.context().provide::<SinkSvc>(sink.clone()).unwrap();
    (app, sink)
}

fn registry() -> PluginRegistry {
    let mut r = PluginRegistry::new();
    r.register(Arc::new(PolitePlugin))
        .register(Arc::new(TersePlugin))
        .register(Arc::new(ConsumerPlugin))
        .register(Arc::new(EventsPlugin));
    r
}

#[tokio::test]
async fn dependencies_drive_activation_not_file_order() {
    // The consumer is listed FIRST, above the provider it injects.
    let (mut app, sink) = app_with_sink(
        r#"
        [[insert]]
        name = "consumer"
        config = { who = "Ada" }

        [[insert]]
        name = "greeter-polite"
        "#,
    );
    app.start().await.unwrap();
    assert_eq!(sink.lock().unwrap().as_slice(), ["Good evening, Ada."]);
}

#[tokio::test]
async fn a_patch_swaps_a_provider_under_a_running_consumer() {
    let (mut app, _sink) = app_with_sink(
        r#"
        [[insert]]
        id = "greeter"
        name = "greeter-polite"

        [[insert]]
        name = "consumer"
        config = { who = "Ada" }
        "#,
    );
    app.start().await.unwrap();
    let ctx = app.context();

    assert_eq!(
        ctx.serial::<Greet>(&"Ada".to_string()).await.as_deref(),
        Some("Good evening, Ada.")
    );

    // Swap the implementation behind the row id, while the process runs.
    app.patch(
        &Layer::from_toml(
            r#"
            [[patch]]
            id = "greeter"
            name = "greeter-terse"
            "#,
        )
        .unwrap(),
    )
    .await
    .unwrap();

    // The consumer was never unloaded, re-applied, or told anything happened.
    // This is the whole claim of a seam: replace one provider, change the product.
    assert_eq!(
        ctx.serial::<Greet>(&"Ada".to_string()).await.as_deref(),
        Some("yo Ada")
    );
}

#[tokio::test]
async fn unloading_reverts_exactly_what_the_plugin_registered() {
    let base = Layer::from_toml(
        r#"
        [[insert]]
        id = "greeter"
        name = "greeter-polite"
        "#,
    )
    .unwrap();
    let mut app = App::new(registry(), ConfigTree::from_layers([base]).unwrap());
    app.start().await.unwrap();
    assert!(app.context().service::<GreeterSvc>().is_some());

    app.patch(&Layer::from_toml("[[remove]]\nid = \"greeter\"").unwrap())
        .await
        .unwrap();
    assert!(
        app.context().service::<GreeterSvc>().is_none(),
        "the service slot must empty when its provider unloads"
    );
}

#[tokio::test]
async fn a_stalled_mount_names_the_row_and_the_service_it_waits_for() {
    let (mut app, _sink) = app_with_sink("[[insert]]\nname = \"consumer\"");
    let err = app.start().await.unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("consumer"), "{rendered}");
    assert!(rendered.contains("greeter"), "{rendered}");
}

#[tokio::test]
async fn two_providers_for_one_slot_is_an_error_not_a_silent_overwrite() {
    let layer = Layer::from_toml(
        r#"
        [[insert]]
        id = "a"
        name = "greeter-polite"

        [[insert]]
        id = "b"
        name = "greeter-terse"
        "#,
    )
    .unwrap();
    let mut app = App::new(registry(), ConfigTree::from_layers([layer]).unwrap());
    let err = app.start().await.unwrap_err().to_string();
    assert!(err.contains("greeter"), "{err}");
}

#[tokio::test]
async fn a_realm_overrides_one_slot_without_disturbing_its_parent() {
    let layer =
        Layer::from_toml("[[insert]]\nid = \"greeter\"\nname = \"greeter-polite\"").unwrap();
    let mut app = App::new(registry(), ConfigTree::from_layers([layer]).unwrap());
    app.start().await.unwrap();

    let root = app.context();
    let isolated = root.isolate();
    let _ = isolated.provide::<GreeterSvc>(Arc::new(Terse)).unwrap();

    assert_eq!(
        root.service::<GreeterSvc>().unwrap().greet("x"),
        "Good evening, x."
    );
    assert_eq!(isolated.service::<GreeterSvc>().unwrap().greet("x"), "yo x");
}

// ---- events: all five dispatch modes -----------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    prompt: String,
    tags: Vec<String>,
}

plexus_event!(Observed, "test/observed", Emit, Request);
plexus_event!(Fanout, "test/fanout", Parallel, Request);
plexus_event!(Decide, "test/decide", Serial, Request => String);
plexus_event!(DecideSync, "test/decide-sync", Bail, Request => String);
plexus_event!(Around, "test/around", Waterfall, Request => String);

static EMITTED: AtomicUsize = AtomicUsize::new(0);
static FANNED: AtomicUsize = AtomicUsize::new(0);

struct FanoutListener;
#[async_trait]
impl Listener<Fanout> for FanoutListener {
    async fn call(&self, _args: &Request) -> Option<()> {
        FANNED.fetch_add(1, Ordering::SeqCst);
        None
    }
}

struct Decider(&'static str, bool);
#[async_trait]
impl Listener<Decide> for Decider {
    async fn call(&self, _args: &Request) -> Option<String> {
        self.1.then(|| self.0.to_string())
    }
}

/// Tags the request on the way in, delegates, and wraps the result on the way out.
struct Tagging(&'static str);
#[async_trait]
impl Waterfall<Around> for Tagging {
    async fn handle(&self, args: &mut Request, next: Next<'_, Around>) -> String {
        args.tags.push(self.0.to_string());
        let inner = next.run(args).await;
        format!("{}({})", self.0, inner)
    }
}

/// Owns the decision: never delegates.
struct ShortCircuit;
#[async_trait]
impl Waterfall<Around> for ShortCircuit {
    async fn handle(&self, _args: &mut Request, _next: Next<'_, Around>) -> String {
        "short".into()
    }
}

struct EventsPlugin;
#[async_trait]
impl Plugin for EventsPlugin {
    fn name(&self) -> &'static str {
        "events"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx.on_emit::<Observed>(|_| {
            EMITTED.fetch_add(1, Ordering::SeqCst);
        });
        let _ = ctx.on_parallel::<Fanout>(Arc::new(FanoutListener));
        let _ = ctx.on_parallel::<Fanout>(Arc::new(FanoutListener));
        let _ = ctx.on_serial::<Decide>(Arc::new(Decider("first", false)));
        let _ = ctx.on_serial::<Decide>(Arc::new(Decider("second", true)));
        let _ = ctx.on_serial::<Decide>(Arc::new(Decider("third", true)));
        let _ = ctx.on_bail::<DecideSync>(|r: &Request| {
            r.prompt.starts_with("!").then(|| "command".to_string())
        });
        let _ = ctx.on_waterfall::<Around>(Arc::new(Tagging("outer")), false);
        let _ = ctx.on_waterfall::<Around>(Arc::new(Tagging("inner")), false);
        Ok(())
    }
}

async fn events_app() -> App {
    let layer = Layer::from_toml("[[insert]]\nname = \"events\"").unwrap();
    let mut app = App::new(registry(), ConfigTree::from_layers([layer]).unwrap());
    app.start().await.unwrap();
    app
}

fn request() -> Request {
    Request {
        prompt: "hello".into(),
        tags: vec![],
    }
}

#[tokio::test]
async fn emit_and_parallel_reach_every_listener() {
    let app = events_app().await;
    let ctx = app.context();
    EMITTED.store(0, Ordering::SeqCst);
    FANNED.store(0, Ordering::SeqCst);

    ctx.emit::<Observed>(&request());
    assert_eq!(EMITTED.load(Ordering::SeqCst), 1);

    ctx.parallel::<Fanout>(&request()).await;
    assert_eq!(FANNED.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn serial_and_bail_stop_at_the_first_decision() {
    let app = events_app().await;
    let ctx = app.context();

    assert_eq!(
        ctx.serial::<Decide>(&request()).await.as_deref(),
        Some("second"),
        "the first listener with an opinion wins; later ones never run"
    );

    assert_eq!(
        ctx.bail::<DecideSync>(&request()),
        None,
        "a listener with no opinion must let the chain fall through"
    );
}

#[tokio::test]
async fn bail_matches_on_its_own_predicate() {
    let app = events_app().await;
    let ctx = app.context();
    let mut command = request();
    command.prompt = "!help".into();
    assert_eq!(ctx.bail::<DecideSync>(&command).as_deref(), Some("command"));
}

#[tokio::test]
async fn waterfall_wraps_the_terminal_in_registration_order() {
    let app = events_app().await;
    let ctx = app.context();
    let mut req = request();

    let out = ctx
        .waterfall::<Around, _>(&mut req, |args| {
            let prompt = args.prompt.clone();
            let tags = args.tags.join("+");
            Box::pin(async move { format!("[{prompt}|{tags}]") })
        })
        .await;

    // Registration order going in, reverse coming out — the shape of
    // around-middleware, and the reason a policy layer can both rewrite the
    // request and post-process the answer.
    assert_eq!(out, "outer(inner([hello|outer+inner]))");
    assert_eq!(req.tags, ["outer", "inner"]);
}

#[tokio::test]
async fn a_waterfall_listener_can_own_the_decision() {
    let app = events_app().await;
    let ctx = app.context();
    // Prepended, so it runs before the tagging pair and short-circuits them.
    let _guard = ctx.on_waterfall::<Around>(Arc::new(ShortCircuit), true);

    let mut req = request();
    let out = ctx
        .waterfall::<Around, _>(&mut req, |_| Box::pin(async { "terminal".to_string() }))
        .await;
    assert_eq!(out, "short");
    assert!(
        req.tags.is_empty(),
        "short-circuit must skip downstream rewrites"
    );
}

#[tokio::test]
async fn unloading_removes_the_listeners_a_plugin_attached() {
    let mut app = events_app().await;
    assert_eq!(app.context().listener_count::<Around>(), 2);
    app.patch(&Layer::from_toml("[[remove]]\nid = \"events\"").unwrap())
        .await
        .unwrap();
    assert_eq!(
        app.context().listener_count::<Around>(),
        0,
        "a plugin's listeners must go when the plugin does"
    );
}

// ---- spatial composability ---------------------------------------------

plexus_event!(Scoped, "test/scoped", Waterfall, Request => String);

/// Appends its label on the way out, so a result shows exactly which listeners
/// ran and in what order.
struct Tag(&'static str);

#[async_trait]
impl Waterfall<Scoped> for Tag {
    async fn handle(&self, args: &mut Request, next: Next<'_, Scoped>) -> String {
        let inner = next.run(args).await;
        format!("{}({inner})", self.0)
    }
}

async fn scoped_app() -> App {
    let layer =
        Layer::from_toml("[[insert]]\nid = \"greeter\"\nname = \"greeter-polite\"").unwrap();
    let mut app = App::new(registry(), ConfigTree::from_layers([layer]).unwrap());
    app.start().await.unwrap();
    app
}

async fn dispatch(ctx: &Context) -> String {
    let mut req = request();
    ctx.waterfall::<Scoped, _>(&mut req, |_| Box::pin(async { "core".to_string() }))
        .await
}

#[tokio::test]
async fn a_listener_in_a_realm_is_invisible_to_its_parent() {
    let app = scoped_app().await;
    let root = app.context();
    let agent = root.isolate();

    let _guard = agent.on_waterfall::<Scoped>(Arc::new(Tag("agent")), false);

    assert_eq!(
        dispatch(&agent).await,
        "agent(core)",
        "the realm that registered it sees it"
    );
    assert_eq!(
        dispatch(&root).await,
        "core",
        "the parent must not be intercepted by something its child installed"
    );
}

#[tokio::test]
async fn a_listener_at_the_root_still_governs_every_realm() {
    let app = scoped_app().await;
    let root = app.context();
    let _guard = root.on_waterfall::<Scoped>(Arc::new(Tag("policy")), false);

    let agent = root.isolate();
    assert_eq!(
        dispatch(&agent).await,
        "policy(core)",
        "a root policy must not be escapable by isolating a realm"
    );
}

#[tokio::test]
async fn sibling_realms_do_not_see_each_other() {
    let app = scoped_app().await;
    let root = app.context();
    let left = root.isolate();
    let right = root.isolate();

    let _l = left.on_waterfall::<Scoped>(Arc::new(Tag("left")), false);
    let _r = right.on_waterfall::<Scoped>(Arc::new(Tag("right")), false);

    assert_eq!(dispatch(&left).await, "left(core)");
    assert_eq!(dispatch(&right).await, "right(core)");
    assert_eq!(dispatch(&root).await, "core");
}

#[tokio::test]
async fn root_policy_wraps_agent_policy_not_the_other_way_round() {
    let app = scoped_app().await;
    let root = app.context();
    let _outer = root.on_waterfall::<Scoped>(Arc::new(Tag("root")), false);

    let agent = root.isolate();
    let _inner = agent.on_waterfall::<Scoped>(Arc::new(Tag("agent")), false);

    // Registration order decides nesting, and the root registered first, so it
    // is the outer wrapper — a root gate inspects what an agent-local listener
    // produced, never the reverse.
    assert_eq!(dispatch(&agent).await, "root(agent(core))");
}

#[tokio::test]
async fn services_and_listeners_agree_on_what_a_realm_means() {
    // The bug this exists to prevent: isolating services while leaving
    // listeners global, so a subagent's tools are scoped and its policy is not.
    let app = scoped_app().await;
    let root = app.context();
    let agent = root.isolate();

    let _ = agent.provide::<GreeterSvc>(Arc::new(Terse)).unwrap();
    let _guard = agent.on_waterfall::<Scoped>(Arc::new(Tag("agent")), false);

    assert_eq!(agent.service::<GreeterSvc>().unwrap().greet("x"), "yo x");
    assert_eq!(dispatch(&agent).await, "agent(core)");

    assert_eq!(
        root.service::<GreeterSvc>().unwrap().greet("x"),
        "Good evening, x.",
        "the parent's service is untouched"
    );
    assert_eq!(
        dispatch(&root).await,
        "core",
        "and so is the parent's listener chain"
    );
}

#[tokio::test]
async fn listener_counts_are_reported_per_realm() {
    let app = scoped_app().await;
    let root = app.context();
    let agent = root.isolate();
    let _guard = agent.on_waterfall::<Scoped>(Arc::new(Tag("agent")), false);

    assert_eq!(agent.listener_count::<Scoped>(), 1);
    assert_eq!(root.listener_count::<Scoped>(), 0);
}

#[tokio::test]
async fn a_plugin_can_be_mounted_into_its_own_realm() {
    let (mut app, sink) = app_with_sink(
        r#"
        [[insert]]
        id = "greeter"
        name = "greeter-polite"
        "#,
    );
    app.start().await.unwrap();
    let root = app.context();

    // Give the subtree a different provider before mounting into it, then mount
    // the consumer there: it resolves the override, and the parent never sees it.
    let scoped = root.isolate();
    let _ = scoped.provide::<GreeterSvc>(Arc::new(Terse)).unwrap();
    let fiber = scoped
        .plugin("consumer", json!({ "who": "Ada" }))
        .await
        .unwrap();

    assert_eq!(sink.lock().unwrap().as_slice(), ["yo Ada"]);
    assert_eq!(
        root.service::<GreeterSvc>().unwrap().greet("Ada"),
        "Good evening, Ada.",
        "the root provider is undisturbed"
    );

    // And the realm's registrations go when its fiber does.
    root.unload(fiber);
    assert_eq!(
        root.serial::<Greet>(&"Ada".to_string()).await,
        None,
        "the subtree's listener is gone with it"
    );
}

#[tokio::test]
async fn plugin_isolated_mounts_and_hands_back_the_scope() {
    let (mut app, sink) = app_with_sink(
        r#"
        [[insert]]
        id = "greeter"
        name = "greeter-polite"
        "#,
    );
    app.start().await.unwrap();
    let root = app.context();
    let (_fiber, scoped) = root
        .plugin_isolated("consumer", json!({ "who": "Bo" }))
        .await
        .unwrap();

    assert_eq!(sink.lock().unwrap().as_slice(), ["Good evening, Bo."]);
    // The handed-back context addresses the child's realm, so a later override
    // lands there rather than at the root.
    let _ = scoped.provide::<GreeterSvc>(Arc::new(Terse)).unwrap();
    assert_eq!(scoped.service::<GreeterSvc>().unwrap().greet("x"), "yo x");
    assert_eq!(
        root.service::<GreeterSvc>().unwrap().greet("x"),
        "Good evening, x."
    );
}

// ---- temporal composability --------------------------------------------

#[tokio::test]
async fn a_row_that_cannot_mount_yet_waits_instead_of_being_lost() {
    // The consumer needs `greeter`, which no row provides.
    let (mut app, sink) =
        app_with_sink("[[insert]]\nname = \"consumer\"\nconfig = { who = \"Ada\" }");
    app.start_lenient().await.unwrap();

    assert!(sink.lock().unwrap().is_empty(), "it cannot have run yet");
    let waiting = app.pending();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].0, "consumer");
    assert_eq!(waiting[0].1, vec!["greeter".to_string()]);

    // Now supply what it was waiting for. The consumer was never re-listed;
    // the runtime remembered it.
    app.patch(&Layer::from_toml("[[insert]]\nname = \"greeter-polite\"").unwrap())
        .await
        .unwrap();

    assert_eq!(sink.lock().unwrap().as_slice(), ["Good evening, Ada."]);
    assert!(app.pending().is_empty());
}

#[tokio::test]
async fn strict_start_still_refuses_to_come_up_half_wired() {
    let (mut app, _sink) = app_with_sink("[[insert]]\nname = \"consumer\"");
    let err = app.start().await.unwrap_err().to_string();
    assert!(err.contains("consumer"), "{err}");
    assert!(err.contains("greeter"), "{err}");
    // The row is still remembered, so a host that chooses to continue can.
    assert_eq!(app.pending().len(), 1);
}
