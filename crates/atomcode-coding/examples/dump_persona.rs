//! Print the `persona-atomcode` fragment exactly as the real row list renders it.
//!
//! Not a hand-built string: the tree is mounted the way `prompt_fragments.rs`
//! mounts it (base layer + the product overlay + the scoped patches), and the
//! text comes out of the live `system-prompt` registry.
//!
//! ```bash
//! cargo run -p atomcode-coding --example dump_persona            # glm-5.3-flash
//! cargo run -p atomcode-coding --example dump_persona -- deepseek-flash
//! ```

use std::path::{Path, PathBuf};

use atomcode_harness::plugins;
use atomcode_harness::seams::SystemPromptSvc;
use atomcode_plexus::{App, ConfigTree, Layer};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dump-persona-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn tree(root: &Path, model: &str) -> ConfigTree {
    let empty_home = root.join("__no_user_skills__");
    let _ = std::fs::create_dir_all(&empty_home);
    let scoped = format!(
        "[[patch]]\nid = \"trace\"\nconfig = {{ stream = false, tools = false, summary = false }}\n\n\
         [[patch]]\nid = \"fs\"\nconfig = {{ root = {root:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ max_rounds = 20, working_dir = {root:?} }}\n\n\
         [[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n\n\
         [[patch]]\nid = \"project-instructions\"\nconfig = {{ project_root = {root:?}, home = {home:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ root = {store:?} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-replay\"\nconfig = {{ script = [{{ text = \"ok\" }}] }}\n\n\
         [[patch]]\nid = \"persona-atomcode\"\nconfig = {{ model = {model:?}, language = \"zh_CN\" }}\n",
        root = root.to_string_lossy(),
        home = empty_home.to_string_lossy(),
        store = root.join("sessions").to_string_lossy(),
        model = model,
    );
    let overlay = atomcode_coding::on_harness::coding_overlay(
        root,
        &root.join("artifacts"),
        atomcode_coding::on_harness::Presence::Attended,
        model,
    );
    ConfigTree::from_layers(vec![
        atomcode_coding::on_harness::base_layer(),
        Layer::from_toml(&overlay).unwrap(),
        Layer::from_toml(&scoped).unwrap(),
    ])
    .unwrap()
}

async fn mounted(dir: &Path, model: &str) -> App {
    let mut catalog = plugins::catalog();
    for plugin in atomcode_coding::on_harness::plugins() {
        catalog.register(plugin);
    }
    let mut app = App::new(catalog, tree(dir, model));
    app.start().await.expect("must mount");
    app
}

fn registry(app: &App) -> std::sync::Arc<atomcode_harness::seams::PromptRegistry> {
    app.context().service::<SystemPromptSvc>().unwrap()
}

#[tokio::main]
async fn main() {
    // arg 2 = "full" prints every fragment, not just the persona.
    let mut args = std::env::args().skip(1);
    let model = args.next().unwrap_or_else(|| "glm-5.3-flash".to_string());
    let whole = args.next().as_deref() == Some("full");
    let dir = scratch("dump");
    std::env::set_var("ATOMCODE_HOME", dir.join("home"));
    std::fs::create_dir_all(dir.join("home")).unwrap();

    // Mount ONCE: the same tree, read twice — before and after the persona row
    // is patched off, which is what isolates its text from every other fragment.
    let mut app = mounted(&dir, &model).await;
    let full = registry(&app).render();
    let ids = registry(&app).ids();

    app.patch(
        &Layer::from_toml("[[patch]]\nid = \"persona-atomcode\"\ndisabled = true\n").unwrap(),
    )
    .await
    .expect("disable the persona row");
    let without = registry(&app).render();

    eprintln!("model: {model}");
    eprintln!("fragment ids in rank order: {ids:#?}");
    eprintln!(
        "persona bytes: {}  |  whole prompt bytes: {}",
        full.len().saturating_sub(without.len() + 2),
        full.len()
    );

    if whole {
        print!("{full}");
    } else if full.ends_with(without.as_str()) && full.len() > without.len() + 2 {
        let cut = full.len() - without.len() - 2;
        print!("{}", &full[..cut]);
    } else {
        eprintln!("!! the persona is not the first fragment — printing everything");
        print!("{full}");
    }
}
