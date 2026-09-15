//! `atui` — the launcher for the plugin-composed TUI.
//!
//! It brings two things the harness does not: this crate's rows (a surface and
//! one row per panel) and coding's. Which of them run is the config's call —
//! `atomcode_tui::product` is the module that decides, and this file only
//! handles the flags that need a screen.
//!
//! ```text
//! atui                              # the product: a coding agent, on a screen
//! atui "fix the build"              # …starting with this prompt
//! atui --offline                    # a scripted model, no network
//! atui --mascot                     # bring the cat
//! atui --dump-config                # what would run
//! atui --audit                      # is the composition sound
//! ```

use std::process::ExitCode;

use atomcode_harness::launch::{value, Flag, Launch, HELP_SHARED};
use atomcode_tui::product;

/// The overlay that swaps the launcher's own defaults in.
///
/// One row, and only for `--headless`: `product::Assembly` has already laid
/// down every other layer, including the profile's own edits, and an overlay
/// runs last — after the user's home patch — so it is the right place for a
/// flag and the wrong place for an assembly.
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
    let assembly = product::assembly();
    let parsed = Launch::new(product::PROFILE, vec![]).parse(
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

    let profiles = assembly.profiles().with_home();
    let catalog = assembly.catalog();
    if let Some(code) = launch.preflight(&profiles, &catalog) {
        return code;
    }

    // **A `-p` this front end cannot draw on, said in words a person can act on.**
    //
    // `atui` ships one composition. `-p plan`, `-p full` and `-p repl` name the
    // harness's own assemblies, none of which contains this product's
    // `tui-panels` bundle — so there is no `surface` row, and every one of this
    // launcher's own flags (`--headless`, `--audit`, `--demo`, `--theme`,
    // `--no-mouse`, `--mascot`) is a patch *to* that row. The loader's own
    // complaint is `patch targets row `surface`, which no earlier layer
    // inserted` — true, and about a row the person never wrote and has no way to
    // place.
    //
    // Checked before mounting rather than after: the failure is not "this
    // profile is missing something", it is "this launcher does not offer that
    // composition", and the sentence should be about the latter.
    if !product::can_draw(&profiles, &launch.profile) {
        eprintln!(
            "`atui` mounts the `{}` profile — it is the composition with a screen in \
                 front of it.\n`{}` is one of the harness's own assemblies and has no \
                 `surface` row, so this launcher's flags have nothing to patch.\nUse the \
                 `harness` binary for `-p {}`, or drop `-p` for the screen.",
            product::PROFILE,
            launch.profile,
            launch.profile
        );
        return ExitCode::from(2);
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
    -p, --profile <name>   which composition to mount (default: tui)
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
    ctrl-w       delete a word   ctrl-r         reasoning: one line, full, off
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
    every one at once, and a click on prose does nothing. Reasoning arrives
    hidden, so ctrl-r is what puts a row there to click.
    The wheel scrolls the conversation.

    ctrl-o (or /mouse) hands the pointer back to the terminal, for when you
    want its own selection instead — across scrollback, say. --no-mouse
    starts that way.
";
