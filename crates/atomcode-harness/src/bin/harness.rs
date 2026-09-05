//! `harness` — the launcher.
//!
//! It resolves a profile into a config tree, mounts it, and hands control to
//! whichever plugin fills the `ui` slot. It knows nothing about models, tools,
//! turns, or terminals: those all arrive as rows.
//!
//! ```text
//! harness "fix the build"                  # the default profile
//! harness --profile repl                   # an interactive terminal session
//! harness --profile tui                    # a full-screen terminal UI
//! harness --profile web                    # an HTTP server with a live stream
//! harness --profile sdk                    # JSON-RPC on stdio, for a program
//! harness --list-profiles                  # what is available, and from where
//! harness --profile web --dump-config      # what would run
//! ```

use std::process::ExitCode;

use atomcode_harness::control::AppControl;
use atomcode_harness::profile::Profiles;
use atomcode_harness::seams::{ControlSvc, UiSvc};
use atomcode_harness::{bundle, plugins, seam_map};
use atomcode_plexus::App;

#[tokio::main]
async fn main() -> ExitCode {
    let mut overlays: Vec<String> = Vec::new();
    let mut prompt: Option<String> = None;
    let mut profile = "oneshot".to_string();
    let mut dump = false;
    let mut seams = false;
    let mut mermaid = false;
    let mut audit = false;
    let mut list = false;
    let mut resume_latest = false;
    let mut list_sessions = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Every one of these is shorthand for a profile or a patch layer;
            // none of them is a branch in this file.
            "--profile" | "-p" => match args.next() {
                Some(name) => profile = name,
                None => {
                    eprintln!("--profile needs a name");
                    return ExitCode::from(2);
                }
            },
            // The front end is orthogonal to the specialization: `code-review`
            // behind a browser is a --ui away, not a second profile.
            "--ui" => match args.next() {
                Some(name) if bundle::UI_NAMES.contains(&name.as_str()) => {
                    overlays.push(bundle::ui_overlay(&name))
                }
                Some(name) => {
                    eprintln!(
                        "unknown ui `{name}`; known: {}",
                        bundle::UI_NAMES.join(", ")
                    );
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("--ui needs a name: {}", bundle::UI_NAMES.join(", "));
                    return ExitCode::from(2);
                }
            },
            "--repl" | "-i" => overlays.push(bundle::ui_overlay("repl")),
            "--tui" => overlays.push(bundle::ui_overlay("tui")),
            "--web" => overlays.push(bundle::ui_overlay("web")),
            "--sdk" => overlays.push(bundle::ui_overlay("sdk")),
            "--headless" => profile = "headless".into(),
            "--offline" => overlays.push(bundle::OFFLINE.to_string()),
            "--env-model" => overlays.push(bundle::ENV_MODEL.to_string()),
            // The web front end's address, without writing a patch file for the
            // one thing people change about it.
            "--port" => match args.next() {
                Some(port) => overlays.push(format!(
                    "[[patch]]\nid = \"ui\"\nconfig = {{ addr = \"127.0.0.1:{port}\" }}\n"
                )),
                None => {
                    eprintln!("--port needs a number");
                    return ExitCode::from(2);
                }
            },
            // A selection from the user's config, without writing a patch file
            // for the most common one-line override there is.
            "--model" | "-m" => match args.next() {
                Some(name) => overlays.push(format!(
                    "[[patch]]\nid = \"llm\"\nconfig = {{ model = {name:?} }}\n"
                )),
                None => {
                    eprintln!("--model needs a selection id (see `atomcode` config)");
                    return ExitCode::from(2);
                }
            },
            "--read-only" => overlays.push(bundle::READ_ONLY.to_string()),
            "--native-tools" => overlays.push(bundle::NATIVE_TOOLS.to_string()),
            "--full" => overlays.push(bundle::FULL.to_string()),
            "--plan" => overlays.push(bundle::PLAN.to_string()),
            "--interactive" => overlays.push(bundle::INTERACTIVE.to_string()),
            "--yolo" => overlays.push(bundle::YOLO.to_string()),
            "--dump-config" => dump = true,
            "--dump-seams" => seams = true,
            "--dump-seams-mermaid" => {
                seams = true;
                mermaid = true;
            }
            "--audit" => audit = true,
            "--list-profiles" => list = true,
            // Continue an existing conversation. The id is required for
            // `--resume`: which conversation to continue is not something a
            // harness should guess.
            "--resume" => match args.next() {
                Some(id) => overlays.push(bundle::resume_overlay(&id)),
                None => {
                    eprintln!("--resume needs a session id (see --list-sessions)");
                    return ExitCode::from(2);
                }
            },
            "--continue" | "-c" => resume_latest = true,
            "--list-sessions" => list_sessions = true,
            "--patch" => {
                let Some(path) = args.next() else {
                    eprintln!("--patch needs a file path");
                    return ExitCode::from(2);
                };
                match std::fs::read_to_string(&path) {
                    Ok(text) => overlays.push(text),
                    Err(e) => {
                        eprintln!("cannot read {path}: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            "-h" | "--help" => {
                println!("{}", HELP);
                return ExitCode::SUCCESS;
            }
            other => prompt = Some(other.to_string()),
        }
    }

    let profiles = Profiles::builtin().with_home();

    // Session ids are read off disk before anything mounts, because choosing
    // which conversation to continue has to happen before the tree that would
    // hold it exists.
    if list_sessions || resume_latest {
        let dir = atomcode_harness::home().join("sessions");
        let mut ids: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension()? != "jsonl" {
                    return None;
                }
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, path.file_stem()?.to_str()?.to_string()))
            })
            .collect();
        ids.sort_by(|a, b| b.0.cmp(&a.0));

        if list_sessions {
            if ids.is_empty() {
                println!("no sessions under {}", dir.display());
            }
            for (modified, id) in ids.iter().take(20) {
                let age = modified.elapsed().map(|d| d.as_secs()).unwrap_or(0);
                println!("  {id:<24} {}", human_age(age));
            }
            return ExitCode::SUCCESS;
        }

        match ids.first() {
            Some((_, id)) => overlays.push(bundle::resume_overlay(id)),
            None => {
                eprintln!("no session to continue under {}", dir.display());
                return ExitCode::from(2);
            }
        }
    }

    if list {
        println!("profiles:");
        for name in profiles.names() {
            let description = profiles
                .get(name)
                .map(|p| p.description.clone())
                .unwrap_or_default();
            println!("  {name:<14} {description}");
        }
        println!("\nbundles: {}", profiles.bundle_names().join(", "));
        println!(
            "\nlayer order: bundles -> the profile's patch -> {} -> --patch overlays",
            profiles.home_patch_path().display()
        );
        return ExitCode::SUCCESS;
    }

    // The seam map is a property of the build, not of a running tree, so it is
    // answerable before anything mounts.
    if seams {
        let rows = seam_map::seam_map(&plugins::catalog());
        if mermaid {
            print!("{}", seam_map::render_mermaid(&rows));
        } else {
            print!("{}", seam_map::render_table(&rows));
        }
        let undeclared = seam_map::undeclared_services(&plugins::catalog());
        if !undeclared.is_empty() {
            eprintln!("\nnot on the map: {}", undeclared.join(", "));
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    let refs: Vec<&str> = overlays.iter().map(String::as_str).collect();
    let tree = match profiles.resolve(&profile, &refs) {
        Ok(tree) => tree,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let mut app = App::new(plugins::catalog(), tree);
    if let Err(e) = app.start().await {
        eprintln!("failed to mount: {e}");
        return ExitCode::from(1);
    }

    if audit {
        let findings = app.audit_with(seam_map::HOST_CONSUMED, seam_map::HOST_PROVIDED);
        if findings.is_empty() {
            println!("composition is consistent: every declared role matches the running tree");
            return ExitCode::SUCCESS;
        }
        for finding in &findings {
            let mark = if finding.is_defect() { "✗" } else { "·" };
            println!("{mark} {finding}");
        }
        // Only a broken declaration fails the check. Dead weight is reported so
        // it can be cleaned up, not so it can break a pipeline.
        let defects = findings.iter().filter(|f| f.is_defect()).count();
        if defects == 0 {
            println!("\nno defects; {} note(s) above", findings.len());
            return ExitCode::SUCCESS;
        }
        return ExitCode::from(1);
    }

    if dump {
        print!("{}", profiles.explain(&profile));
        println!();
        print!("{}", app.dump_runtime());
        return ExitCode::SUCCESS;
    }

    // Hand the tree the ability to reconfigure itself. The `App` lives here, so
    // this is the only place that can offer it; every front end reaches it
    // through the seam without knowing that.
    let ctx = app.context();
    let front_end = match ctx.require::<UiSvc>() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("no front end mounted: {e}");
            return ExitCode::from(1);
        }
    };
    let app = std::sync::Arc::new(tokio::sync::Mutex::new(app));
    let _control = ctx.provide::<ControlSvc>(std::sync::Arc::new(AppControl::new(app.clone())));

    // The launcher does not run turns. It resolves the front end the profile
    // asked for and hands over — which is why "one prompt", "a terminal
    // session", "a web server" and "a JSON-RPC peer" are rows, not branches.
    match front_end.run(&ctx, prompt).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\n{e}");
            ExitCode::from(1)
        }
    }
}

/// Rough, and deliberately so: the point is "is this the one I was just in".
fn human_age(secs: u64) -> String {
    match secs {
        0..=90 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

const HELP: &str = "\
harness — AtomCode's coding agent as a plugin tree

The launcher resolves a profile into a config tree, mounts it, and hands control
to whichever plugin fills the `ui` slot. Every flag below picks a profile or
stacks a patch layer; none of them is a code path.

USAGE:
    harness [OPTIONS] [PROMPT]

PROFILES:
    -p, --profile <NAME>   named assembly (default: oneshot)
    -i, --repl             shorthand for --profile repl
        --tui              shorthand for --profile tui
        --web              shorthand for --profile web
        --port <N>         where the web front end listens (default 7878)
        --sdk              shorthand for --profile sdk
        --headless         shorthand for --profile headless
        --list-profiles    what is available, and the layer order

OVERLAYS (stacked last, after the profile and your home patch):
    --offline        swap the `llm` row for a scripted provider (no API key)
    -m, --model <ID> pick a `[models.*]` selection from ~/.atomcode/config.toml
    --env-model      take the model from ATOMCODE_BASE_URL/_MODEL/_API_KEY
                     instead of ~/.atomcode/config.toml
    --plan           read-only exploration: refuse every mutating tool
    --read-only      swap the `fs` world for a read-only one
    --native-tools   use the production local tools instead of seam-routed ones
    --full           also mount the code graph, web access and delegation
    --yolo           allow every tool call without asking (sandboxes, evals)
    --interactive    ask before risky calls instead of refusing them
    --patch <FILE>   your own layer (repeatable, last wins)

SESSIONS:
    -c, --continue         resume the most recent session
        --resume <ID>      resume a named session
        --list-sessions    what is on disk

INSPECTION:
    --dump-config          how the profile assembles, and what is running
    --dump-seams           the capability map (definition / providers / consumers)
    --dump-seams-mermaid   the same map as a mermaid graph
    --audit                check every declared role against the running tree

MODEL:
    By default the `llm` row reads ~/.atomcode/config.toml — the provider you
    already configured for AtomCode. Use --env-model to take it from
    ATOMCODE_BASE_URL / ATOMCODE_MODEL / ATOMCODE_API_KEY instead, or --offline
    for a scripted model that needs no credentials at all.

ENV:
    ATOMCODE_HOME    config.toml, profiles/ and harness.patch.toml are read from here";
