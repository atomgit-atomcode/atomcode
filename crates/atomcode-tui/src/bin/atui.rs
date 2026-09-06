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
    let mut demo = false;

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
            // The palette follows the terminal's own background unless told
            // otherwise; this is the "otherwise", for a terminal that will not
            // answer (some tmux and ssh setups) or answers wrongly.
            "--theme" => match args.next() {
                Some(name) => overlays.push(format!(
                    "[[patch]]\nid = \"surface\"\nconfig = {{ theme = {name:?} }}\n"
                )),
                None => {
                    eprintln!("--theme needs auto, dark or light");
                    return ExitCode::from(2);
                }
            },
            // Give the mouse back to the terminal: click-drag selects text
            // again, and folding goes back to being a keyboard gesture.
            "--no-mouse" => overlays
                .push("[[patch]]\nid = \"surface\"\nconfig = { mouse = false }\n".to_string()),
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
            // What the terminal answered, and what each role resolved to. The
            // first thing to run when something still looks wrong: it says
            // whether the terminal answered at all.
            "--probe-terminal" => {
                print!("{}", atomcode_tui::surface::probe_report());
                return ExitCode::SUCCESS;
            }
            "--dump-config" => dump = true,
            "--headless" => overlays.push(HEADLESS.to_string()),
            "--audit" => {
                audit = true;
                overlays.push(HEADLESS.to_string());
            }
            "--demo" => {
                demo = true;
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

    if demo {
        // One composed frame, printed and gone: what the screen looks like,
        // with no tty, no model and no keyboard. It goes through the real
        // host, the real modules and the real encoder — a mock-up that did not
        // would be a picture of something that does not exist.
        let ctx = app.context();
        let Some(surface) = ctx.service::<atomcode_tui::plugin::SurfaceSvc>() else {
            eprintln!("--demo needs a surface row");
            return ExitCode::FAILURE;
        };
        let Some(modules) = ctx.service::<atomcode_tui::plugin::ModulesSvc>() else {
            eprintln!("--demo needs the module rows");
            return ExitCode::FAILURE;
        };
        let host = atomcode_tui::host::Host::new(modules, atomcode_tui::host::default_layout());
        for fact in atomcode_tui::conformance::facts() {
            host.absorb(&fact);
        }
        {
            let mut moment = host.moment.write().expect("moment poisoned");
            moment.input = "再帮我看看 crates/ 的结构".into();
            moment.caret = moment.input.len();
            moment.caps = atomcode_tui::caps::Caps::detect();
            moment.cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
        }
        let (w, h) = surface.size();
        let frame = host.compose((w, h));
        let violations = frame.containment_violations();
        print!(
            "{}",
            atomcode_tui::ansi::encode_with(&frame, atomcode_tui::caps::Caps::detect())
        );
        println!("\x1b[{};1H", h + 1);
        if !violations.is_empty() {
            for v in &violations {
                eprintln!("containment: {v}");
            }
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
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
        --theme <t>        auto (ask the terminal), dark or light
        --no-mouse         leave the pointer to the terminal
        --yolo             approve every tool call
        --read-only        no writes, no shell
        --full             code graph, web access, delegation
        --probe-terminal   what this terminal answered, and how it resolves
        --dump-config      print the tree that would run
        --headless         paint into memory; no terminal needed
        --audit            check the composition and exit
        --demo             print one composed frame and exit（不需要终端）
    -h, --help             this

KEYS
    enter        send            shift-enter    a line break (or ctrl-j)
    esc          back out one layer: the selection, then what you typed,
                 then the turn.  ctrl-c always stops the turn.
    ctrl-d       quit            ctrl-u         clear the line
    ctrl-w       delete a word   ctrl-r         fold or unfold reasoning
    ctrl-t       fold tool calls ctrl-n         show or hide the mascot
    ctrl-o       hand the mouse back to the terminal
    ctrl-l       repaint everything (for when something else wrote here)
    ctrl-f       focus layout      ctrl-z         undo the last layout change
    up/down      move the caret in what you are typing; at its top or bottom
                 edge, step back and forward through what you have said
    pgup/pgdn    scroll the conversation (so does the wheel, a line a notch)

MOUSE
    click in the composer to put the caret there. Drag to select; the
    selection is copied to the clipboard on release
    (OSC 52, so it works over ssh and tmux) and esc clears it. Click a tool
    call to fold or unfold that one — ctrl-t still does every one at once.
    The wheel scrolls the conversation.

    ctrl-o (or /mouse) hands the pointer back to the terminal, for when you
    want its own selection instead — across scrollback, say. --no-mouse
    starts that way.
";
