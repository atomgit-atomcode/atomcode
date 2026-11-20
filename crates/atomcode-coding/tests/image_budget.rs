//! `image-budget`: a request carries at most so many image bytes, the oldest
//! left out first; one refused for its size is sent again with half, one
//! refused for its pictures is sent again without them, and a picture that
//! does not decode is never sent. All of it is judged on what the agent loop
//! actually sends — a row nearer the provider may rebuild the request from the
//! log. The log keeps every picture — only what goes on the wire changes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::{MessageOrigin, OnlySession};
use atomcode_harness::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use atomcode_harness::plugins::image_budget::{IMAGE_LEFT_OUT, IMAGE_REFUSED};
use atomcode_harness::seams::StopReason;
use atomcode_harness::{bundle, create_agent, drive};
use atomcode_kernel::message::{ImageContent, Message};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_plexus::{App, ConfigTree, Layer, Next, Waterfall};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("image-budget-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn tree(root: &std::path::Path, budget: usize) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let base = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"image-gateway\"\nconfig = {{}}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 6, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"image-budget\"\nconfig = {{ max_bytes = {budget} }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    ConfigTree::from_layers(vec![
        atomcode_coding::on_harness::base_layer(),
        atomcode_coding::on_harness::headless_patch(),
        Layer::from_toml(bundle::ONESHOT_APP).unwrap(),
        Layer::from_toml(&base).unwrap(),
    ])
    .unwrap()
}

/// Stands where the provider is: records how many pictures and notes each
/// attempt carried, and answers the first ones with the scripted refusals.
struct Gateway {
    refusals: Vec<(u16, &'static str)>,
    attempts: Mutex<Vec<(usize, usize)>>,
}

impl Gateway {
    fn new(refusals: Vec<(u16, &'static str)>) -> Arc<Self> {
        Arc::new(Self {
            refusals,
            attempts: Mutex::new(Vec::new()),
        })
    }
    fn attempts(&self) -> Vec<(usize, usize)> {
        self.attempts.lock().unwrap().clone()
    }
}

#[async_trait]
impl atomcode_kernel::provider::LlmProvider for Gateway {
    fn model_name(&self) -> &str {
        "image-gateway"
    }
    fn supports_vision(&self) -> bool {
        true
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        let images = messages.iter().map(|m| m.images.len()).sum();
        let notes = messages
            .iter()
            .map(|m| m.text.matches(IMAGE_LEFT_OUT).count() + m.text.matches(IMAGE_REFUSED).count())
            .sum();
        let attempt = {
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push((images, notes));
            attempts.len() - 1
        };
        if let Some((status, message)) = self.refusals.get(attempt) {
            return Err(ProviderError {
                http_status: Some(*status),
                message: (*message).into(),
                ..Default::default()
            });
        }
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("looked".into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// Mounts a [`Gateway`] as the model.
struct GatewayRow(Arc<Gateway>);

#[async_trait]
impl atomcode_plexus::Plugin for GatewayRow {
    fn name(&self) -> &'static str {
        "image-gateway"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _: &serde_json::Value,
    ) -> Result<(), String> {
        ctx.provide::<atomcode_harness::seams::LlmSvc>(self.0.clone())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

async fn app(dir: &std::path::Path, budget: usize, gateway: Arc<Gateway>) -> App {
    let mut registry = atomcode_coding::on_harness::catalog();
    registry.register(Arc::new(GatewayRow(gateway)));
    let mut app = App::new(registry, tree(dir, budget));
    app.start().await.expect("must mount");
    app
}

/// A real 2×1 PNG, 96 bytes encoded.
fn picture() -> ImageContent {
    ImageContent {
        media_type: "image/png".into(),
        data: "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjdAAAADUlEQVR4nGP4zwAE/wEHAAH/4iOeWQAAAABJRU5ErkJgggAA"
            .into(),
    }
}

/// The same PNG cut short: its header parses, its pixels do not.
fn broken_picture() -> ImageContent {
    ImageContent {
        media_type: "image/png".into(),
        data: "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjd".into(),
    }
}

/// A request over the budget goes out with the newest pictures and a line where
/// the older ones were (a step of four at a time); refused for size anyway, it
/// goes again with half the budget, and the turn ends normally. The log still
/// has all six.
#[tokio::test]
async fn the_oldest_pictures_are_left_out_and_a_413_is_sent_again_with_half() {
    let dir = scratch("budget");
    let gateway = Gateway::new(vec![(
        413,
        "HTTP 413: [ATOMCODE_BODY_TOO_LARGE] 请求体超过 20MB 限制",
    )]);
    let app = app(&dir, 320, gateway.clone()).await;
    let agent = create_agent(&app).await.unwrap();
    agent.send_full(
        "check these frames",
        MessageOrigin::User,
        (0..6).map(|_| picture()).collect(),
    );
    let outcome = drive(&app, &agent).await.unwrap();

    assert_eq!(outcome.stop, StopReason::Stopped, "{:?}", outcome.error);
    assert_eq!(
        gateway.attempts(),
        vec![(2, 1), (0, 1)],
        "first: the newest two (576 bytes over 320: a step of four goes); again after \
         the 413, at half: none — one note in the message each time"
    );
    let logged: usize = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.images.len())
        .sum();
    assert_eq!(logged, 6, "the log keeps every picture");
}

/// Under the budget the request is what the log says.
#[tokio::test]
async fn under_the_budget_every_picture_goes() {
    let dir = scratch("under");
    let gateway = Gateway::new(vec![]);
    let app = app(&dir, 1000, gateway.clone()).await;
    let agent = create_agent(&app).await.unwrap();
    agent.send_full("look", MessageOrigin::User, vec![picture(), picture()]);
    drive(&app, &agent).await.unwrap();
    assert_eq!(gateway.attempts(), vec![(2, 0)]);
}

/// The reported failure, end to end: DeepSeek refused a picture cut short, and
/// the session could not go on. It never reaches the provider now — not even
/// when a row nearer the provider rebuilds the request from the log, the way
/// compaction does after `image-budget` has run.
#[tokio::test]
async fn a_broken_picture_never_reaches_the_provider_even_from_a_rebuilt_request() {
    struct Rebuild;
    #[async_trait]
    impl Waterfall<AgentRequest> for Rebuild {
        async fn handle(
            &self,
            req: &mut ModelRequest,
            next: Next<'_, AgentRequest>,
        ) -> Result<ModelResponse, RequestError> {
            let system: Vec<Message> = req
                .messages
                .iter()
                .take_while(|m| m.role == atomcode_kernel::message::Role::System)
                .cloned()
                .collect();
            req.messages = system;
            req.messages.push(Message::user_with_images(
                "after compaction",
                vec![broken_picture()],
            ));
            next.run(req).await
        }
    }
    let dir = scratch("rebuilt");
    let gateway = Gateway::new(vec![]);
    let app = app(&dir, 1000, gateway.clone()).await;
    // Registered after every row mounted: nearer the provider than `image-budget`.
    let _rebuild = app
        .context()
        .on_waterfall::<AgentRequest>(Arc::new(Rebuild), false);
    let agent = create_agent(&app).await.unwrap();
    agent.send_full("look", MessageOrigin::User, vec![picture()]);
    let outcome = drive(&app, &agent).await.unwrap();
    assert_eq!(outcome.stop, StopReason::Stopped, "{:?}", outcome.error);
    assert_eq!(gateway.attempts(), vec![(0, 1)], "no picture, one note");
}

/// A picture the provider refuses, by its words: the turn goes on without it.
#[tokio::test]
async fn a_refused_picture_does_not_end_the_turn() {
    let dir = scratch("refused");
    let gateway = Gateway::new(vec![(
        400,
        "HTTP 400: .messages[3].content[0]: You have uploaded an unsupported image.",
    )]);
    let app = app(&dir, 1000, gateway.clone()).await;
    let agent = create_agent(&app).await.unwrap();
    agent.send_full("look", MessageOrigin::User, vec![picture()]);
    let outcome = drive(&app, &agent).await.unwrap();
    assert_eq!(outcome.stop, StopReason::Stopped, "{:?}", outcome.error);
    assert_eq!(
        gateway.attempts().iter().map(|a| a.0).collect::<Vec<_>>(),
        [1, 0]
    );
}
