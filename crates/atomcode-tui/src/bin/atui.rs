//! `atui` — the launcher for the plugin-composed TUI.
//!
//! It adds three rows to the harness catalog and hands over. It knows nothing
//! about panels, keys, streams or layout: those all arrive as config. The
//! flags every launcher takes come from `atomcode_harness::launch`; only the
//! ones that need a screen are parsed here.
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

use atomcode_harness::launch::{value, Flag, Launch, HELP_SHARED};
use atomcode_harness::plugins;
use atomcode_harness::profile::Profiles;
use atomcode_plexus::PluginRegistry;
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

/// Paint into memory. Auditing a composition, dumping it, or running under
/// CI must not require a screen — a check that needs a tty is a check that
/// cannot run where it matters most.
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
    let help = format!("{HELP_HEAD}\n\n{HELP_SHARED}\n\n{HELP_KEYS}");
    let mut demo = false;
    let parsed = Launch::new("repl", vec![TUI2.to_string(), rows::SCREEN.to_string()]).parse(
        std::env::args().skip(1),
        &help,
        |flag, args, launch| match flag {
            "--mascot" => {
                launch.overlays.push(MASCOT.to_string());
                Flag::Taken
            }
            // The palette follows the terminal's own background unless told
            // otherwise; this is the "otherwise", for a terminal that will not
            // answer (some tmux and ssh setups) or answers wrongly.
            "--theme" => match value(args, "--theme", "auto, dark or light") {
                Ok(name) => {
                    launch.overlays.push(format!(
                        "[[patch]]\nid = \"surface\"\nconfig = {{ theme = {name:?} }}\n"
                    ));
                    Flag::Taken
                }
                Err(code) => Flag::Exit(code),
            },
            // Give the mouse back to the terminal: click-drag selects text
            // again, and folding goes back to being a keyboard gesture.
            "--no-mouse" => {
                launch
                    .overlays
                    .push("[[patch]]\nid = \"surface\"\nconfig = { mouse = false }\n".to_string());
                Flag::Taken
            }
            // What the terminal answered, and what each role resolved to. The
            // first thing to run when something still looks wrong: it says
            // whether the terminal answered at all.
            "--probe-terminal" => {
                print!("{}", atomcode_tui::surface::probe_report());
                Flag::Exit(ExitCode::SUCCESS)
            }
            // Here `--headless` swaps the surface, not the profile: the tree
            // is the same one a person would get, painted into memory. The
            // inspection switches ride on it, since mounting the terminal
            // surface takes the screen.
            "--headless" => {
                launch.overlays.push(HEADLESS.to_string());
                Flag::Taken
            }
            "--audit" | "--dump-config" | "--demo" => {
                launch.overlays.push(HEADLESS.to_string());
                match flag {
                    "--audit" => launch.audit = true,
                    "--dump-config" => launch.dump = true,
                    _ => demo = true,
                }
                Flag::Taken
            }
            // This launcher is the full-screen front end; the others have
            // `harness`.
            "--ui" | "--repl" | "-i" | "--web" | "--sdk" | "--tui" | "--port" => {
                eprintln!("`atui` is the full-screen front end; for `{flag}` use `harness`");
                Flag::Exit(ExitCode::from(2))
            }
            _ => Flag::NotMine,
        },
    );
    let mut launch = match parsed {
        Ok(launch) => launch,
        Err(code) => return code,
    };

    let profiles = Profiles::builtin().with_home();
    let catalog = catalog();
    if let Some(code) = launch.preflight(&profiles, &catalog) {
        return code;
    }
    let mounted = match launch.mount(catalog, &profiles).await {
        Ok(mounted) => mounted,
        Err(code) => return code,
    };
    // `tui-modules` and `tui-commands` are read by the launcher and by tests
    // — their consumer is outside the tree by construction, like `ui`.
    if let Some(code) = mounted.inspect(&profiles, &["tui-modules", "tui-commands"]) {
        return code;
    }

    if demo {
        // One composed frame, printed and gone: what the screen looks like,
        // with no tty, no model and no keyboard. It goes through the real
        // host, the real modules and the real encoder — a mock-up that did not
        // would be a picture of something that does not exist.
        let ctx = mounted.app().context();
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

    mounted.hand_over().await
}

const HELP_HEAD: &str = "\
atui — a full-screen terminal UI assembled from plugin rows

USAGE
    atui [FLAGS] [PROMPT]

SCREEN
    -p, --profile <name>   which composition to mount (default: repl)
        --mascot           show the cat
        --theme <t>        auto (ask the terminal), dark or light
        --no-mouse         leave the pointer to the terminal
        --probe-terminal   what this terminal answered, and how it resolves
        --headless         paint into memory; no terminal needed
        --demo             print one composed frame and exit（不需要终端）";

const HELP_KEYS: &str = "\
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
    call or a thought to fold or unfold that one — ctrl-t and ctrl-r still do
    every one at once, and a click on prose does nothing.
    The wheel scrolls the conversation.

    ctrl-o (or /mouse) hands the pointer back to the terminal, for when you
    want its own selection instead — across scrollback, say. --no-mouse
    starts that way.
";
