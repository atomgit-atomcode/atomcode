//! Front ends as rows: five of them, one slot, and nothing below them changes.

use atomcode_harness::agent::OnlySession;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::{ToolsSvc, UiSvc, UserQuestionsSvc};
use atomcode_harness::{bundle, plugins};
use atomcode_plexus::{App, ConfigTree};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("plexus-front-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Resolve a profile with the model scripted and the world pointed at `root`.
fn tree(profile: &str, root: &std::path::Path, extra: &[&str]) -> ConfigTree {
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 8, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n\
         config = {{ script = [ {{ text = \"answered\" }} ] }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy()
    );
    let mut overlays: Vec<&str> = vec![quiet, scoped.as_str()];
    overlays.extend(extra);
    Profiles::builtin()
        .resolve(profile, &overlays)
        .unwrap_or_else(|e| panic!("`{profile}`: {e}"))
}

async fn start(tree: ConfigTree) -> App {
    let mut app = App::new(plugins::catalog(), tree);
    app.start().await.expect("must mount");
    app
}

#[tokio::test]
async fn every_front_end_fills_the_same_slot() {
    let dir = scratch("slots");
    for (profile, expected) in [
        ("oneshot", "one prompt, one turn, exit"),
        ("embed", "no front end; the embedder drives"),
        ("repl", "interactive terminal session"),
        ("sdk", "line-delimited JSON-RPC on stdio"),
    ] {
        let app = start(tree(profile, &dir, &[])).await;
        let ui = app
            .context()
            .service::<UiSvc>()
            .unwrap_or_else(|| panic!("`{profile}` mounted no front end"));
        assert!(
            ui.describe().contains(expected),
            "`{profile}` describes itself as `{}`",
            ui.describe()
        );
    }
}

#[tokio::test]
async fn the_web_front_end_reports_where_it_listens() {
    let dir = scratch("web-addr");
    let app = start(tree(
        "web",
        &dir,
        &["[[patch]]\nid = \"ui\"\nconfig = { addr = \"127.0.0.1:9999\" }"],
    ))
    .await;
    assert_eq!(
        app.context().service::<UiSvc>().unwrap().describe(),
        "http://127.0.0.1:9999",
        "the address is config, not a constant"
    );
}

#[tokio::test]
async fn the_agent_underneath_is_identical_across_front_ends() {
    let dir = scratch("same-agent");
    let mut catalogs = Vec::new();
    let mut prompts = Vec::new();
    for profile in ["oneshot", "web", "sdk", "embed"] {
        let app = start(tree(profile, &dir, &[])).await;
        let ctx = app.context();
        let mut names = ctx.service::<ToolsSvc>().unwrap().names();
        names.sort();
        catalogs.push(names);
        prompts.push(
            ctx.service::<atomcode_harness::seams::SystemPromptSvc>()
                .unwrap()
                .render(),
        );
    }
    assert!(
        catalogs.windows(2).all(|w| w[0] == w[1]),
        "swapping the front end must not change what the model can do: {catalogs:?}"
    );
    assert!(
        prompts.windows(2).all(|w| w[0] == w[1]),
        "nor what it is told"
    );
}

#[tokio::test]
async fn a_front_end_with_a_terminal_can_ask_and_one_without_cannot() {
    let dir = scratch("questions");

    // The unattended provider is what a headless tree gets: it declines.
    let headless = start(tree("headless", &dir, &[])).await;
    let questions = headless.context().service::<UserQuestionsSvc>();
    if let Some(questions) = questions {
        assert!(
            questions.describe().contains("unattended"),
            "headless must not claim it can ask: {}",
            questions.describe()
        );
    }
    drop(headless);

    // The terminal front ends fill the same slot with something that can.
    {
        let profile = "repl";
        let app = start(tree(profile, &dir, &[])).await;
        let questions = app
            .context()
            .service::<UserQuestionsSvc>()
            .unwrap_or_else(|| panic!("`{profile}` should be able to ask"));
        assert!(
            questions.describe().contains("terminal"),
            "`{profile}`: {}",
            questions.describe()
        );
    }
}

#[tokio::test]
async fn the_quiet_front_end_returns_without_doing_anything() {
    let dir = scratch("quiet");
    let app = start(tree("embed", &dir, &[])).await;
    let ctx = app.context();
    ctx.service::<UiSvc>()
        .unwrap()
        .run(&ctx, Some("ignored".into()))
        .await
        .expect("the embedder drives, so this is a no-op");
    assert!(
        ctx.only_session().is_none_or(|log| log.is_empty()),
        "it must not have started a turn behind the embedder's back"
    );
}

#[tokio::test]
async fn the_one_shot_front_end_runs_the_prompt_it_is_given() {
    let dir = scratch("oneshot");
    let app = start(tree("oneshot", &dir, &[])).await;
    let ctx = app.context();
    ctx.service::<UiSvc>()
        .unwrap()
        .run(&ctx, Some("do the thing".into()))
        .await
        .unwrap();

    let transcript = ctx
        .only_session()
        .unwrap()
        .derive_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(transcript.contains("do the thing"));
    assert!(transcript.contains("answered"));
}

#[tokio::test]
async fn the_one_shot_front_end_says_so_when_given_nothing() {
    let dir = scratch("oneshot-empty");
    let app = start(tree("oneshot", &dir, &[])).await;
    let ctx = app.context();
    let err = ctx
        .service::<UiSvc>()
        .unwrap()
        .run(&ctx, None)
        .await
        .unwrap_err();
    assert!(err.contains("needs a prompt"), "{err}");
}

#[tokio::test]
async fn a_front_end_can_be_swapped_on_a_running_tree() {
    let dir = scratch("swap");
    let mut app = App::new(plugins::catalog(), tree("oneshot", &dir, &[]));
    app.start().await.unwrap();
    assert!(app
        .context()
        .service::<UiSvc>()
        .unwrap()
        .describe()
        .contains("one prompt"));

    // The front end is a row like any other.
    app.patch(
        &atomcode_plexus::Layer::from_toml("[[patch]]\nid = \"ui\"\nname = \"ui-quiet\"").unwrap(),
    )
    .await
    .unwrap();
    assert!(app
        .context()
        .service::<UiSvc>()
        .unwrap()
        .describe()
        .contains("no front end"));
}

#[tokio::test]
async fn the_offline_overlay_composes_with_every_front_end() {
    for profile in Profiles::builtin().names() {
        let quiet = "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
        let tree = Profiles::builtin()
            .resolve(profile, &[bundle::OFFLINE, quiet])
            .unwrap();
        let mut app = App::new(plugins::catalog(), tree);
        app.start()
            .await
            .unwrap_or_else(|e| panic!("`{profile}` + offline: {e}"));
        assert!(app.context().service::<UiSvc>().is_some());
    }
}
