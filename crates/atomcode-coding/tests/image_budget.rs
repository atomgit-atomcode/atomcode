//! `image-budget`: a request carries at most so many image bytes, the oldest
//! left out first; one refused for its size is sent again with half. The log
//! keeps every picture — only what goes on the wire changes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::agent::{MessageOrigin, OnlySession};
use atomcode_harness::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use atomcode_harness::plugins::image_budget::IMAGE_LEFT_OUT;
use atomcode_harness::seams::StopReason;
use atomcode_harness::{bundle, create_agent, drive};
use atomcode_kernel::message::ImageContent;
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
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [ {{ text = \"ok\" }} ] }}\n\n\
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

/// Stands where the provider is: says how many pictures and placeholders each
/// attempt carried, and refuses the first `refuse` as too large.
struct Gateway {
    refuse: u32,
    attempts: Arc<Mutex<Vec<(usize, usize)>>>,
}

#[async_trait]
impl Waterfall<AgentRequest> for Gateway {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        _next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let images = req.messages.iter().map(|m| m.images.len()).sum();
        let notes = req
            .messages
            .iter()
            .map(|m| m.text.matches(IMAGE_LEFT_OUT).count())
            .sum();
        let n = {
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push((images, notes));
            attempts.len() as u32
        };
        if n <= self.refuse {
            return Err(RequestError {
                http_status: Some(413),
                ..RequestError::message("HTTP 413: [ATOMCODE_BODY_TOO_LARGE] 请求体超过 20MB 限制")
            });
        }
        Ok(ModelResponse {
            text: "looked".into(),
            ..Default::default()
        })
    }
}

fn picture() -> ImageContent {
    ImageContent {
        media_type: "image/png".into(),
        data: "x".repeat(6),
    }
}

/// A request over the budget goes out with the newest picture and a line where
/// each older one was; refused for size anyway, it goes again with half the
/// budget, and the turn ends normally. The log still has all three.
#[tokio::test]
async fn the_oldest_pictures_are_left_out_and_a_413_is_sent_again_with_half() {
    let dir = scratch("budget");
    let mut app = App::new(atomcode_coding::on_harness::catalog(), tree(&dir, 10));
    app.start().await.expect("must mount");
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let _gateway = app.context().on_waterfall::<AgentRequest>(
        Arc::new(Gateway {
            refuse: 1,
            attempts: attempts.clone(),
        }),
        false,
    );

    let agent = create_agent(&app).await.unwrap();
    agent.send_full(
        "check these frames",
        MessageOrigin::User,
        vec![picture(), picture(), picture()],
    );
    let outcome = drive(&app, &agent).await.unwrap();

    assert_eq!(outcome.stop, StopReason::Stopped, "{:?}", outcome.error);
    assert_eq!(
        *attempts.lock().unwrap(),
        vec![(1, 1), (0, 1)],
        "first: the newest only; again after the 413: none, each note in its message"
    );
    let logged: usize = app
        .context()
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.images.len())
        .sum();
    assert_eq!(logged, 3, "the log keeps every picture");
}

/// Under the budget the request is what the log says.
#[tokio::test]
async fn under_the_budget_every_picture_goes() {
    let dir = scratch("under");
    let mut app = App::new(atomcode_coding::on_harness::catalog(), tree(&dir, 1000));
    app.start().await.expect("must mount");
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let _gateway = app.context().on_waterfall::<AgentRequest>(
        Arc::new(Gateway {
            refuse: 0,
            attempts: attempts.clone(),
        }),
        false,
    );
    let agent = create_agent(&app).await.unwrap();
    agent.send_full("look", MessageOrigin::User, vec![picture(), picture()]);
    drive(&app, &agent).await.unwrap();
    assert_eq!(*attempts.lock().unwrap(), vec![(2, 0)]);
}
