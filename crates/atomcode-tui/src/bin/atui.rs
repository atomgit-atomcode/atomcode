//! `atui` — the launcher for the plugin-composed TUI.
//!
//! It adds three rows to the harness catalog and hands over. It knows nothing
//! about panels, keys, streams or layout: those all arrive as config.
//!
//! ```text
//! atui                              # a session, on the model your config names
//! atui "fix the build"              # …starting with this prompt
//! atui --offline                    # a scripted model, no network
//! atui --mascot                     # bring the cat
//! atui --dump-config                # what would run
//! atui --audit                      # is the composition sound
//! ```

use std::process::ExitCode;
use std::sync::Arc;

use atomcode_harness::control::AppControl;
use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::{ControlSvc, UiSvc};
use atomcode_harness::{bundle, plugins, seam_map};
use atomcode_plexus::{App, PluginRegistry};
use atomcode_tui::plugin::{HeadlessSurfacePlugin, TerminalSurfacePlugin, TuiUiPlugin};
use atomcode_tui::rows;

/// The shipped catalog plus this crate's rows.
fn catalog() -> PluginRegistry {
    let mut c = plugins::catalog();
    c.register(Arc::new(TuiUiPlugin))
        .register(Arc::new(TerminalSurfacePlugin))
        .register(Arc::new(HeadlessSurfacePlugin));
    for row in rows::catalog() {
        c.register(row);
    }
    c
}

/// The overlay that swaps the shipped front end for this one.
const TUI2: &str = r#"
[[insert]]
id = "surface"
name = "surface-terminal"

[[patch]]
id = "ui"
name = "ui-tui2"
# Explicit and empty. A patch that names a new plugin keeps the old one's
# config, so without this the `repl` profile's `banner`/`prompt` — knobs that
# belong to the line-based REPL — arrive at a row that has no knobs at all.
config = {}


# The screen is the output; nothing else may write to it.
[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }

# This front end owns the screen, so it is the one that asks — and the
# standalone asker must stand down, because two rows filling one slot is an
# error rather than a preference.
[[patch]]
id = "user-questions-unattended"
disabled = true

# Ask before a risky call rather than refusing it outright. That is the whole
# point of having a person there.
[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false
"#;

/// Paint into memory. Auditing a composition, or running under CI, must not
/// require a screen — a check that needs a tty is a check that cannot run where
/// it matters most.
const HEADLESS: &str = r#"
[[patch]]
id = "surface"
name = "surface-headless"
"#;

const MASCOT: &str = r#"
[[patch]]
id = "tui-panel-mascot"
disabled = false
"#;

#[tokio::main]
async fn main() -> ExitCode {
    let mut overlays: Vec<String> = vec![TUI2.to_string(), rows::SCREEN.to_string()];
    let mut prompt: Option<String> = None;
    let mut profile = "repl".to_string();
    let mut dump = false;
    let mut audit = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" | "-p" => match args.next() {
                Some(name) => profile = name,
                None => {
                    eprintln!("--profile needs a name");
                    return ExitCode::from(2);
                }
            },
            "--offline" => overlays.push(bundle::OFFLINE.to_string()),
            // Without this the env vars are simply not read — the config row
            // is a different row, and it will happily send whatever key your
            // config has, which is how "I exported three variables and got a
            // 401" happens.
            "--env-model" | "--env" => overlays.push(bundle::ENV_MODEL.to_string()),
            "--mascot" => overlays.push(MASCOT.to_string()),
            "--yolo" => overlays.push(bundle::YOLO.to_string()),
            "--read-only" => overlays.push(bundle::READ_ONLY.to_string()),
            "--full" => overlays.push(bundle::FULL.to_string()),
            "--model" | "-m" => match args.next() {
                Some(name) => overlays.push(format!(
                    "[[patch]]\nid = \"llm\"\nconfig = {{ model = {name:?} }}\n"
                )),
                None => {
                    eprintln!("--model needs a selection id");
                    return ExitCode::from(2);
                }
            },
            "--dump-config" => dump = true,
            "--headless" => overlays.push(HEADLESS.to_string()),
            "--audit" => {
                audit = true;
                overlays.push(HEADLESS.to_string());
            }
            "-h" | "--help" => {
                println!("{}", HELP);
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown flag `{other}` — try --help");
                return ExitCode::from(2);
            }
            other => prompt = Some(other.to_string()),
        }
    }

    let refs: Vec<&str> = overlays.iter().map(String::as_str).collect();
    let tree = match Profiles::builtin().with_home().resolve(&profile, &refs) {
        Ok(tree) => tree,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::from(2);
        }
    };

    if dump {
        print!("{}", tree.dump());
        return ExitCode::SUCCESS;
    }

    let mut app = App::new(catalog(), tree);
    if let Err(e) = app.start().await {
        eprintln!("failed to mount: {e}");
        return ExitCode::FAILURE;
    }

    if audit {
        // `tui-modules` is read by the launcher and by tests — its consumer is
        // outside the tree by construction, like `ui` and `agent-handle`.
        let mut consumed: Vec<&str> = seam_map::HOST_CONSUMED.to_vec();
        consumed.push("tui-modules");
        consumed.push("tui-commands");
        let findings = app.audit_with(&consumed, seam_map::HOST_PROVIDED);
        let defects = findings.iter().filter(|f| f.is_defect()).count();
        for f in &findings {
            println!("· {f}");
        }
        println!(
            "{}",
            if findings.is_empty() {
                "composition is consistent".to_string()
            } else {
                format!("{defects} defect(s), {} note(s)", findings.len() - defects)
            }
        );
        return if defects == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    let ctx = app.context();
    let ui = match ctx.require::<UiSvc>() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("no front end mounted: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Reconfiguring the running tree needs the `App`, which only the launcher
    // holds — so it puts the capability in the tree for every front end to use.
    let app = Arc::new(tokio::sync::Mutex::new(app));
    let _control = ctx.provide::<ControlSvc>(Arc::new(AppControl::new(app.clone())));

    match ui.run(&ctx, prompt).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

const HELP: &str = "\
atui — a full-screen terminal UI assembled from plugin rows

USAGE
    atui [FLAGS] [PROMPT]

FLAGS
    -p, --profile <name>   which composition to mount (default: repl)
    -m, --model <id>       a model selection from your atomcode config
        --offline          a scripted model; no network
        --env-model        use ATOMCODE_BASE_URL / _MODEL / _API_KEY
        --mascot           show the cat
        --yolo             approve every tool call
        --read-only        no writes, no shell
        --full             code graph, web access, delegation
        --dump-config      print the tree that would run
        --headless         paint into memory; no terminal needed
        --audit            check the composition and exit
    -h, --help             this

KEYS
    enter        send            esc / ctrl-c   stop the turn
    ctrl-d       quit            ctrl-u         clear the line
    ctrl-w       delete a word   ctrl-r         fold or unfold reasoning
    ctrl-t       fold tool calls ctrl-n         show or hide the mascot
    pgup/pgdn    scroll          up/down        scroll by a line
";
