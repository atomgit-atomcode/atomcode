//! What every launcher shares.
//!
//! A launcher resolves a profile into a config tree, mounts it, and hands
//! control to whichever plugin fills the `ui` slot. `harness` and `atui` are
//! two of them. What differs is the catalog each brings and a handful of flags
//! that only mean something with a screen; everything else — the overlay
//! flags, session lookup, the inspection switches, the mount-and-hand-over
//! sequence — is here once, so the two cannot drift apart flag by flag.
//!
//! ```text
//! let launch = Launch::new("oneshot", vec![]).parse(args, HELP, |_, _, _| Flag::NotMine)?;
//! let catalog = plugins::catalog();
//! if let Some(code) = launch.preflight(&profiles, &catalog) { return code; }
//! let mounted = launch.mount(catalog, &profiles).await?;
//! if let Some(code) = mounted.inspect(&profiles, &[]) { return code; }
//! mounted.hand_over().await
//! ```

use std::process::ExitCode;
use std::sync::Arc;

use atomcode_plexus::{App, PluginRegistry};

use crate::bundle;
use crate::control::AppControl;
use crate::profile::Profiles;
use crate::seam_map;
use crate::seams::{ControlSvc, UiSvc};

/// What the command line asked for, before anything is resolved.
#[derive(Debug, Default)]
pub struct Launch {
    pub profile: String,
    /// Patch layers, stacked after the profile and the home patch. First in,
    /// lowest; a launcher's own defaults go in before parsing starts.
    pub overlays: Vec<String>,
    pub prompt: Option<String>,
    pub dump: bool,
    pub seams: bool,
    pub mermaid: bool,
    pub audit: bool,
    pub list_profiles: bool,
    pub resume_latest: bool,
    pub list_sessions: bool,
}

/// What a launcher's own flag handler says about one argument.
pub enum Flag {
    /// Handled; move on.
    Taken,
    /// Not one of mine; try the shared set.
    NotMine,
    /// Stop here with this code: help was printed, or a bad value reported.
    Exit(ExitCode),
}

/// The value after a flag, or the usual complaint.
pub fn value(
    args: &mut dyn Iterator<Item = String>,
    flag: &str,
    what: &str,
) -> Result<String, ExitCode> {
    args.next().ok_or_else(|| {
        eprintln!("{flag} needs {what}");
        ExitCode::from(2)
    })
}

impl Launch {
    pub fn new(profile: &str, overlays: Vec<String>) -> Self {
        Self {
            profile: profile.to_string(),
            overlays,
            ..Self::default()
        }
    }

    /// Parse the command line.
    ///
    /// `own` sees every argument first, so a launcher can claim a flag the
    /// shared set also knows: `atui` takes `--headless` for its surface where
    /// `harness` takes it for a profile. A flag neither claims is an error, not
    /// a prompt — a prompt that starts with `-` is rarer than a typo does.
    pub fn parse(
        mut self,
        args: impl IntoIterator<Item = String>,
        help: &str,
        mut own: impl FnMut(&str, &mut dyn Iterator<Item = String>, &mut Launch) -> Flag,
    ) -> Result<Launch, ExitCode> {
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match own(&arg, &mut args, &mut self) {
                Flag::Taken => continue,
                Flag::Exit(code) => return Err(code),
                Flag::NotMine => {}
            }
            match arg.as_str() {
                // Every one of these is shorthand for a profile or a patch
                // layer; none of them is a branch in a launcher.
                "--profile" | "-p" => self.profile = value(&mut args, "--profile", "a name")?,
                // The front end is orthogonal to everything else in the tree:
                // the same agent behind a browser is a --ui away, not a
                // second profile.
                "--ui" => {
                    let name = value(&mut args, "--ui", &bundle::UI_NAMES.join(", "))?;
                    if !bundle::UI_NAMES.contains(&name.as_str()) {
                        eprintln!(
                            "unknown ui `{name}`; known: {}",
                            bundle::UI_NAMES.join(", ")
                        );
                        return Err(ExitCode::from(2));
                    }
                    self.overlays.push(bundle::ui_overlay(&name));
                }
                "--repl" | "-i" => self.overlays.push(bundle::ui_overlay("repl")),
                "--web" => self.overlays.push(bundle::ui_overlay("web")),
                "--sdk" => self.overlays.push(bundle::ui_overlay("sdk")),
                // The full-screen front end is its own crate with its own
                // launcher; this catalog has no row for it.
                "--tui" => {
                    eprintln!("the full-screen front end is `atui` (crates/atomcode-tui)");
                    return Err(ExitCode::from(2));
                }
                "--headless" => self.profile = "headless".into(),
                "--offline" => self.overlays.push(bundle::OFFLINE.to_string()),
                // Without this the env vars are simply not read — the config
                // row is a different row, and it will happily send whatever key
                // your config has, which is how "I exported three variables and
                // got a 401" happens.
                "--env-model" | "--env" => self.overlays.push(bundle::ENV_MODEL.to_string()),
                // The web front end's address, without writing a patch file
                // for the one thing people change about it.
                "--port" => {
                    let port = value(&mut args, "--port", "a number")?;
                    self.overlays.push(format!(
                        "[[patch]]\nid = \"ui\"\nconfig = {{ addr = \"127.0.0.1:{port}\" }}\n"
                    ));
                }
                // A selection from the user's config, without writing a patch
                // file for the most common one-line override there is.
                "--model" | "-m" => {
                    let name = value(
                        &mut args,
                        "--model",
                        "a selection id (see `atomcode` config)",
                    )?;
                    self.overlays.push(format!(
                        "[[patch]]\nid = \"llm\"\nconfig = {{ model = {name:?} }}\n"
                    ));
                }
                "--read-only" => self.overlays.push(bundle::READ_ONLY.to_string()),
                "--native-tools" => self.overlays.push(bundle::NATIVE_TOOLS.to_string()),
                "--full" => self.overlays.push(bundle::FULL.to_string()),
                "--plan" => self.overlays.push(bundle::PLAN.to_string()),
                "--interactive" => self.overlays.push(bundle::INTERACTIVE.to_string()),
                "--yolo" => self.overlays.push(bundle::YOLO.to_string()),
                "--dump-config" => self.dump = true,
                "--dump-seams" => self.seams = true,
                "--dump-seams-mermaid" => {
                    self.seams = true;
                    self.mermaid = true;
                }
                "--audit" => self.audit = true,
                "--list-profiles" => self.list_profiles = true,
                // Continue an existing conversation. The id is required for
                // `--resume`: which conversation to continue is not something
                // a launcher should guess.
                "--resume" => {
                    let id = value(&mut args, "--resume", "a session id (see --list-sessions)")?;
                    self.overlays.push(bundle::resume_overlay(&id));
                }
                "--continue" | "-c" => self.resume_latest = true,
                "--list-sessions" => self.list_sessions = true,
                "--patch" => {
                    let path = value(&mut args, "--patch", "a file path")?;
                    match std::fs::read_to_string(&path) {
                        Ok(text) => self.overlays.push(text),
                        Err(e) => {
                            eprintln!("cannot read {path}: {e}");
                            return Err(ExitCode::from(2));
                        }
                    }
                }
                "-h" | "--help" => {
                    println!("{help}");
                    return Err(ExitCode::SUCCESS);
                }
                other if other.starts_with('-') => {
                    eprintln!("unknown flag `{other}` — try --help");
                    return Err(ExitCode::from(2));
                }
                other => self.prompt = Some(other.to_string()),
            }
        }
        Ok(self)
    }

    /// Everything answerable before a tree exists: session lookups, the
    /// profile list, the seam map. `Some` means the launcher is done.
    pub fn preflight(&mut self, profiles: &Profiles, catalog: &PluginRegistry) -> Option<ExitCode> {
        // Session ids are read off disk before anything mounts, because
        // choosing which conversation to continue has to happen before the
        // tree that would hold it exists.
        if self.list_sessions || self.resume_latest {
            let dir = crate::home().join("sessions");
            let ids = sessions_on_disk(&dir);
            if self.list_sessions {
                if ids.is_empty() {
                    println!("no sessions under {}", dir.display());
                }
                for (modified, id) in ids.iter().take(20) {
                    let age = modified.elapsed().map(|d| d.as_secs()).unwrap_or(0);
                    println!("  {id:<24} {}", human_age(age));
                }
                return Some(ExitCode::SUCCESS);
            }
            match ids.first() {
                Some((_, id)) => self.overlays.push(bundle::resume_overlay(id)),
                None => {
                    eprintln!("no session to continue under {}", dir.display());
                    return Some(ExitCode::from(2));
                }
            }
        }

        if self.list_profiles {
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
            return Some(ExitCode::SUCCESS);
        }

        // The seam map is a property of the build, not of a running tree, so
        // it is answerable before anything mounts.
        if self.seams {
            let rows = seam_map::seam_map(catalog);
            if self.mermaid {
                print!("{}", seam_map::render_mermaid(&rows));
            } else {
                print!("{}", seam_map::render_table(&rows));
            }
            let undeclared = seam_map::undeclared_services(catalog);
            if !undeclared.is_empty() {
                eprintln!("\nnot on the map: {}", undeclared.join(", "));
                return Some(ExitCode::from(1));
            }
            return Some(ExitCode::SUCCESS);
        }
        None
    }

    /// Resolve the tree and mount it on `catalog`. Errors are already reported.
    pub async fn mount(
        self,
        catalog: PluginRegistry,
        profiles: &Profiles,
    ) -> Result<Mounted, ExitCode> {
        let refs: Vec<&str> = self.overlays.iter().map(String::as_str).collect();
        let tree = match profiles.resolve(&self.profile, &refs) {
            Ok(tree) => tree,
            Err(e) => {
                eprintln!("{e}");
                return Err(ExitCode::from(2));
            }
        };
        let mut app = App::new(catalog, tree);
        if let Err(e) = app.start().await {
            eprintln!("failed to mount: {e}");
            return Err(ExitCode::from(1));
        }
        Ok(Mounted { app, launch: self })
    }
}

/// A mounted tree, waiting to be inspected or handed to its front end.
pub struct Mounted {
    app: App,
    launch: Launch,
}

impl Mounted {
    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn launch(&self) -> &Launch {
        &self.launch
    }

    /// `--audit` and `--dump-config`, if asked for. `Some` means the launcher
    /// is done. `consumed` names roles the launcher itself reads, on top of
    /// the ones every host does.
    pub fn inspect(&self, profiles: &Profiles, consumed: &[&str]) -> Option<ExitCode> {
        if self.launch.audit {
            let mut all: Vec<&str> = seam_map::HOST_CONSUMED.to_vec();
            all.extend_from_slice(consumed);
            let findings = self.app.audit_with(&all, seam_map::HOST_PROVIDED);
            if findings.is_empty() {
                println!("composition is consistent: every declared role matches the running tree");
                return Some(ExitCode::SUCCESS);
            }
            for finding in &findings {
                let mark = if finding.is_defect() { "✗" } else { "·" };
                println!("{mark} {finding}");
            }
            // Only a broken declaration fails the check. Dead weight is
            // reported so it can be cleaned up, not so it can break a pipeline.
            let defects = findings.iter().filter(|f| f.is_defect()).count();
            if defects == 0 {
                println!("\nno defects; {} note(s) above", findings.len());
                return Some(ExitCode::SUCCESS);
            }
            return Some(ExitCode::from(1));
        }

        if self.launch.dump {
            print!("{}", profiles.explain(&self.launch.profile));
            println!();
            print!("{}", self.app.dump_runtime());
            return Some(ExitCode::SUCCESS);
        }
        None
    }

    /// Hand the tree to whichever plugin fills `ui`, and the ability to
    /// reconfigure itself along with it. The `App` lives here, so this is the
    /// only place that can offer it; every front end reaches it through the
    /// seam without knowing that.
    pub async fn hand_over(self) -> ExitCode {
        let ctx = self.app.context();
        let front_end = match ctx.require::<UiSvc>() {
            Ok(ui) => ui,
            Err(e) => {
                eprintln!("no front end mounted: {e}");
                return ExitCode::from(1);
            }
        };
        let app = Arc::new(tokio::sync::Mutex::new(self.app));
        let _control = ctx.provide::<ControlSvc>(Arc::new(AppControl::new(app.clone())));

        // The launcher does not run turns. It resolves the front end the
        // profile asked for and hands over — which is why "one prompt", "a
        // terminal session", "a web server" and "a JSON-RPC peer" are rows,
        // not branches.
        match front_end.run(&ctx, self.launch.prompt).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\n{e}");
                ExitCode::from(1)
            }
        }
    }
}

/// Session ids under `dir`, newest first.
fn sessions_on_disk(dir: &std::path::Path) -> Vec<(std::time::SystemTime, String)> {
    let mut ids: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(dir)
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
    ids
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

/// The flags every launcher takes, for each one's `--help` to append.
pub const HELP_SHARED: &str = "\
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
    --list-profiles        what is available, and the layer order

MODEL:
    By default the `llm` row reads ~/.atomcode/config.toml — the provider you
    already configured for AtomCode. Use --env-model to take it from
    ATOMCODE_BASE_URL / ATOMCODE_MODEL / ATOMCODE_API_KEY instead, or --offline
    for a scripted model that needs no credentials at all.

ENV:
    ATOMCODE_HOME    config.toml, profiles/ and harness.patch.toml are read from here";

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn shared(s: &str) -> Result<Launch, ExitCode> {
        Launch::new("oneshot", vec![]).parse(argv(s), "help", |_, _, _| Flag::NotMine)
    }

    #[test]
    fn the_shared_set_fills_the_launch() {
        let l = shared("-p repl --offline --yolo -c fix the build").unwrap();
        assert_eq!(l.profile, "repl");
        assert_eq!(l.overlays, vec![bundle::OFFLINE, bundle::YOLO]);
        assert!(l.resume_latest);
        // The last bare word wins, as before.
        assert_eq!(l.prompt.as_deref(), Some("build"));
    }

    #[test]
    fn a_flag_nobody_owns_is_an_error_not_a_prompt() {
        assert_eq!(shared("--mascot").err(), Some(ExitCode::from(2)));
        assert_eq!(shared("--model").err(), Some(ExitCode::from(2)));
        assert_eq!(shared("--ui nope").err(), Some(ExitCode::from(2)));
    }

    #[test]
    fn the_full_screen_front_end_is_not_a_row_here() {
        assert_eq!(shared("--tui").err(), Some(ExitCode::from(2)));
        assert_eq!(shared("--ui tui").err(), Some(ExitCode::from(2)));
    }

    #[test]
    fn a_launcher_claims_a_flag_before_the_shared_set_sees_it() {
        let mut demo = false;
        let l = Launch::new("repl", vec!["first".into()])
            .parse(
                argv("--headless --demo --theme dark"),
                "help",
                |flag, args, l| {
                    match flag {
                        // The shared meaning of --headless would switch profiles;
                        // this launcher wants its surface swapped instead.
                        "--headless" => {
                            l.overlays.push("surface".into());
                            Flag::Taken
                        }
                        "--demo" => {
                            demo = true;
                            Flag::Taken
                        }
                        "--theme" => match value(args, "--theme", "a name") {
                            Ok(t) => {
                                l.overlays.push(t);
                                Flag::Taken
                            }
                            Err(code) => Flag::Exit(code),
                        },
                        _ => Flag::NotMine,
                    }
                },
            )
            .unwrap();
        assert!(demo);
        assert_eq!(
            l.profile, "repl",
            "the launcher's own --headless kept the profile"
        );
        assert_eq!(l.overlays, vec!["first", "surface", "dark"]);
    }
}
