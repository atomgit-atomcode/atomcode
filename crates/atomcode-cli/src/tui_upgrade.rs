//! `/upgrade` on the row-assembled screen, and the status row's "a newer build
//! is out".
//!
//! **Why this is here now.** `/upgrade` used to be answered with "run
//! `atomcode upgrade`" (`tui_elsewhere::IN_THE_CLI`), on the reasoning that the
//! binary replaces itself and doing it again in the UI only repeats the CLI. The
//! user reversed that on 2026-09-29: the classic screen upgrades in place, with
//! progress, and restarts itself, and a person who moved to this screen should
//! not have to leave it for the same thing. The CLI subcommand stays.
//!
//! **Why a launcher row.** Downloading and replacing a binary, reading the
//! staged-upgrade pointer and restarting the process are all things the screen
//! cannot do (`gates/tui-layers.sh`). The screen draws; this row does. The
//! restart itself happens after the screen has put the terminal back:
//! [`take_restart`] is read by the launcher once `tui_front::run` returns.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_config::i18n::{t, Msg};
use atomcode_harness::seams::{UiSvc, UserInterface};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandOption, CommandSet, Outcome};
use atomcode_tui::plugin::{CommandsSvc, UpdateCheckSvc};
use atomcode_tui::update::UpdateCheck;
use atomcode_updater::UpgradeEvent;
use serde_json::Value;

/// The row's name.
pub const ROW: &str = "tui-upgrade";

/// The command this row answers.
pub const COMMAND: &str = "upgrade";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// What the launcher starts once the screen has closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    /// The binary now in place — captured before the swap (on Windows
    /// `current_exe()` names the renamed file afterwards).
    pub exe: PathBuf,
    /// A rollback's restart. The binary is then the OLDER build: it is started
    /// bare (`atomcode_updater::restart_fresh`) and not told it was upgraded.
    pub rolled_back: bool,
    /// Said by the launcher before the restart — what a rollback did to
    /// automatic updates, which the older build cannot say.
    pub notes: Vec<String>,
}

/// The binary to start again once the screen has closed, when an upgrade or a
/// rollback asked for it.
static RESTART: Mutex<Option<Restart>> = Mutex::new(None);

/// Take the restart an upgrade asked for, if any. The launcher calls this after
/// the screen has closed and re-executes into the path it gets.
pub fn take_restart() -> Option<Restart> {
    RESTART.lock().ok()?.take()
}

/// Whether an upgrade already installed a binary that is waiting for a restart.
fn restart_pending() -> bool {
    RESTART.lock().map(|slot| slot.is_some()).unwrap_or(false)
}

/// This build's version as the updater writes versions.
fn current() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// Mounts `/upgrade` and the update check.
pub struct UpgradeRow;

#[async_trait]
impl Plugin for UpgradeRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-update-check"]
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "upgrading this binary in place (`/upgrade`) and saying on the status row when a newer build is out"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UpdateCheckSvc>(Arc::new(Latest))
            .map_err(|e| e.to_string())?;
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        let ui = ctx.require::<UiSvc>().map_err(|e| e.to_string())?;
        commands.add(Arc::new(Upgrade {
            ui,
            running: Arc::new(AtomicBool::new(false)),
        }))?;
        Ok(())
    }
}

/// "Is a newer build out", as the classic screen's footer answers it.
struct Latest;

#[async_trait]
impl UpdateCheck for Latest {
    async fn available(&self) -> Option<String> {
        // A download already staged by an earlier run is news without asking
        // anyone; the network is only asked when there is none, and never when
        // offline.
        let version = match atomcode_updater::read_pending().ok().flatten() {
            Some(pending) => pending.version,
            None if atomcode_config::config::offline::is_offline_active() => return None,
            None => {
                atomcode_updater::fetch_manifest_if_newer(&current())
                    .await
                    .ok()
                    .flatten()?
                    .version
            }
        };
        Some(hint_for(&version, atomcode_updater::is_package_managed()))
    }
}

/// The status row's words for a newer build. A package-managed install is
/// upgraded by its package manager, so it is told that instead of `/upgrade`.
fn hint_for(version: &str, package_managed: bool) -> String {
    if package_managed {
        t(Msg::StatusUpgradeHintPm { version }).into_owned()
    } else {
        t(Msg::StatusUpgradeHint { version }).into_owned()
    }
}

/// What `/upgrade` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Upgrade {
        force: bool,
    },
    /// `confirmed` only from the confirmation (or typed out in full): a
    /// rollback restarts into an older build and stops automatic updates, so it
    /// is asked about first.
    Rollback {
        confirmed: bool,
    },
}

/// What the confirmation dispatches when it is taken.
const ROLLBACK_CONFIRMED: &str = "/upgrade rollback --yes";

impl Asked {
    /// `None` for an argument it does not take.
    fn of(args: &str) -> Option<Self> {
        let words: Vec<String> = args
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        match words
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            // `latest` is the sub-menu's row for a plain upgrade.
            [] | ["latest"] => Some(Asked::Upgrade { force: false }),
            ["--force"] | ["-f"] => Some(Asked::Upgrade { force: true }),
            ["rollback"] => Some(Asked::Rollback { confirmed: false }),
            ["rollback", "--yes"] => Some(Asked::Rollback { confirmed: true }),
            _ => None,
        }
    }
}

/// One step of an upgrade, as the person reads it — the classic screen's words,
/// step for step — and whether it ends in a restart.
///
/// Download progress is said at the quarter marks only, as tuix does: a line per
/// chunk would be hundreds of lines. `last_pct` carries that between events.
fn said_for(event: UpgradeEvent, last_pct: &mut i32) -> (Option<String>, Option<PathBuf>) {
    match event {
        UpgradeEvent::ManifestFetched { version } => {
            *last_pct = -1;
            (
                Some(t(Msg::UpgradeManifestFetched { version: &version }).into_owned()),
                None,
            )
        }
        UpgradeEvent::Downloading { bytes, total } => {
            let pct = if total == 0 {
                0
            } else {
                ((bytes * 100) / total) as i32
            };
            if pct == *last_pct {
                return (None, None);
            }
            *last_pct = pct;
            let said = matches!(pct, 25 | 50 | 75 | 100)
                .then(|| t(Msg::UpgradeDownloading { pct, bytes, total }).into_owned());
            (said, None)
        }
        UpgradeEvent::Verifying => (Some(t(Msg::UpgradeVerifying).into_owned()), None),
        UpgradeEvent::Replacing => (Some(t(Msg::UpgradeReplacing).into_owned()), None),
        UpgradeEvent::Done {
            version,
            backup,
            exe,
        } => (
            Some(
                t(Msg::UpgradeDone {
                    version: &version,
                    backup: &backup.display().to_string(),
                })
                .into_owned(),
            ),
            Some(exe),
        ),
        UpgradeEvent::Failed(message) => (Some(failed(&message)), None),
        UpgradeEvent::RolledBack { exe, backup, .. } => (
            Some(
                t(Msg::UpgradeRolledBack {
                    exe: &exe.display().to_string(),
                    backup: &backup.display().to_string(),
                })
                .into_owned(),
            ),
            Some(exe),
        ),
    }
}

/// A failed upgrade, told apart the way the classic screen tells it apart:
/// "a package manager owns this install" and "already on the latest" are not
/// failures a person should read in red.
fn failed(message: &str) -> String {
    if message.contains(atomcode_updater::PACKAGE_MANAGED) {
        t(Msg::UpgradePackageManaged).into_owned()
    } else if message.contains(atomcode_updater::ALREADY_LATEST) {
        let (current, latest) =
            atomcode_updater::already_latest_versions(message).unwrap_or(("?", "?"));
        t(Msg::UpgradeAlreadyLatest { current, latest }).into_owned()
    } else {
        t(Msg::UpgradeFailed { error: message }).into_owned()
    }
}

struct Upgrade {
    ui: Arc<dyn UserInterface>,
    /// One upgrade at a time: two would race to replace the same file.
    running: Arc<AtomicBool>,
}

#[async_trait]
impl CommandSet for Upgrade {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        vec![upgrade_command()]
    }

    async fn run(&self, name: &str, args: &str, _ctx: &Context) -> Outcome {
        if name != COMMAND {
            return Outcome::Quiet;
        }
        let Some(asked) = Asked::of(args) else {
            return Outcome::Refused(t(Msg::UpgradeUnknownArg { arg: args.trim() }).into_owned());
        };
        // A new binary is already in place and only its restart is waiting —
        // the person was asked about background sessions and chose to stay.
        // Upgrading again would download the same release and move the new
        // binary into `.bak`, over the only copy of the old one, so a rollback
        // could no longer reach it. The restart still happens at the next quit.
        if restart_pending() {
            return Outcome::Said(t(Msg::UpgradeRestartPending).into_owned());
        }
        // Asked first — and only when there is something to roll back to.
        if asked == (Asked::Rollback { confirmed: false }) {
            return match rollback_target() {
                Ok(_) => Outcome::Open(confirmation()),
                Err(why) => Outcome::Refused(why),
            };
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Outcome::Quiet;
        }
        let ui = self.ui.clone();
        let running = self.running.clone();
        match asked {
            Asked::Rollback { .. } => {
                // With the pause on automatic updates: the older build is the
                // live `atomcode` now, and its own startup would upgrade again.
                let (event, notes) = match atomcode_updater::rollback_and_pause() {
                    Ok(summary) => {
                        let notes = atomcode_updater::rollback_notes(&summary.updates);
                        (
                            UpgradeEvent::RolledBack {
                                exe: summary.exe,
                                backup: summary.backup,
                                updates: summary.updates,
                            },
                            notes,
                        )
                    }
                    Err(e) => (UpgradeEvent::Failed(format!("{e:#}")), Vec::new()),
                };
                running.store(false, Ordering::SeqCst);
                let (said, exe) = said_for(event, &mut -1);
                // Said before the screen is asked to close, as the upgrade does:
                // returned as this command's answer instead, it raced the quit
                // and could be lost with the screen.
                if let Some(said) = said {
                    ui.say(&said);
                }
                finish(
                    ui.as_ref(),
                    exe.map(|exe| Restart {
                        exe,
                        rolled_back: true,
                        notes,
                    }),
                );
                Outcome::Quiet
            }
            Asked::Upgrade { force } => {
                tokio::spawn(async move {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UpgradeEvent>();
                    let failures = tx.clone();
                    let driver = tokio::spawn(async move {
                        if let Err(e) = atomcode_updater::run_upgrade(current(), force, tx).await {
                            let _ = failures.send(UpgradeEvent::Failed(format!("{e:#}")));
                        }
                    });
                    let mut last_pct = -1;
                    let mut restart = None;
                    while let Some(event) = rx.recv().await {
                        let (said, then) = said_for(event, &mut last_pct);
                        if let Some(said) = said {
                            ui.say(&said);
                        }
                        if then.is_some() {
                            restart = then;
                        }
                    }
                    let _ = driver.await;
                    running.store(false, Ordering::SeqCst);
                    finish(
                        ui.as_ref(),
                        restart.map(|exe| Restart {
                            exe,
                            rolled_back: false,
                            notes: Vec::new(),
                        }),
                    );
                });
                Outcome::Said(t(Msg::CmdCheckingUpdate).into_owned())
            }
        }
    }
}

/// `/upgrade` as the menu lists it. The options are rows: taking `/upgrade`
/// there opens them instead of starting a download, so `rollback` is seen
/// before anything happens.
fn upgrade_command() -> Command {
    Command::said_taking(
        COMMAND,
        "[--force | rollback]".into(),
        t(Msg::CmdDescUpgrade),
    )
    .selecting(vec![
        CommandOption::new("latest", t(Msg::UpgradeOptLatest)),
        CommandOption::new("--force", t(Msg::UpgradeOptForce)),
        CommandOption::new("rollback", t(Msg::UpgradeOptRollback)),
    ])
}

/// A new binary is in place: leave it to the launcher to start it once this
/// screen has closed, and close the screen the way `/quit` does.
fn finish(ui: &dyn UserInterface, restart: Option<Restart>) {
    let Some(restart) = restart else {
        return;
    };
    if let Ok(mut slot) = RESTART.lock() {
        *slot = Some(restart);
    }
    ui.run_slash("/quit");
}

/// The file a rollback would go back to; `Err` says why there is none.
fn rollback_target() -> Result<PathBuf, String> {
    if atomcode_updater::is_package_managed() {
        return Err(t(Msg::UpgradePackageManaged).into_owned());
    }
    let exe = atomcode_updater::current_exe_path().map_err(|error| format!("{error:#}"))?;
    let backup = atomcode_updater::backup_path(&exe);
    // As `run_rollback` does: a previous version a swap left in a rename slot
    // is kept as `.bak` first, rather than answering "nothing to roll back to".
    atomcode_updater::settle_leftover_slots(&exe);
    if backup.exists() {
        Ok(backup)
    } else {
        Err(
            atomcode_i18n::screen::t(atomcode_i18n::screen::Msg::RollbackNothingToRollBackTo {
                path: &backup.display().to_string(),
            })
            .into_owned(),
        )
    }
}

/// The question before a rollback: what happens, Enter to go, Esc to stay.
fn confirmation() -> Arc<atomcode_tui::wizard::Wizard> {
    use atomcode_i18n::screen::{t as tr, Msg as SMsg};
    use atomcode_tui::wizard::{StepDef, StepKind, Wizard};
    let title = t(Msg::UpgradeOptRollback).into_owned();
    let step = StepDef::new("rollback", title.clone(), StepKind::Note).saying(vec![
        tr(SMsg::RollbackConfirmSwitch).into_owned(),
        tr(SMsg::RollbackConfirmNoUpdates).into_owned(),
        t(Msg::RollbackSessionsNote).into_owned(),
        String::new(),
        tr(SMsg::RollbackConfirmKeys).into_owned(),
    ]);
    Wizard::new(
        "upgrade-rollback",
        title,
        vec![step],
        Box::new(|_| {}),
        ROLLBACK_CONFIRMED,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arguments the classic screen takes, and nothing else.
    #[test]
    fn upgrade_takes_force_and_rollback() {
        assert_eq!(Asked::of(""), Some(Asked::Upgrade { force: false }));
        assert_eq!(Asked::of(" --force "), Some(Asked::Upgrade { force: true }));
        assert_eq!(Asked::of("-f"), Some(Asked::Upgrade { force: true }));
        assert_eq!(Asked::of("latest"), Some(Asked::Upgrade { force: false }));
        assert_eq!(
            Asked::of("ROLLBACK"),
            Some(Asked::Rollback { confirmed: false })
        );
        assert_eq!(
            Asked::of("rollback --yes"),
            Some(Asked::Rollback { confirmed: true })
        );
        assert_eq!(Asked::of("now"), None);
    }

    /// Taken from the menu, `/upgrade` opens its options instead of starting
    /// a download: the rows are what it can do, `rollback` among them.
    #[test]
    fn upgrade_is_picked_through_its_options() {
        let options: Vec<String> = ["latest", "--force", "rollback"]
            .iter()
            .map(|v| v.to_string())
            .collect();
        let command = upgrade_command();
        let offered: Vec<String> = command
            .options
            .iter()
            .map(|o| o.value.to_string())
            .collect();
        assert_eq!(offered, options);
        // Each row is something `run` accepts.
        for value in &offered {
            assert!(Asked::of(value).is_some(), "{value}");
        }
        // And the one that restarts into an older build is asked about first.
        assert_eq!(
            Asked::of("rollback"),
            Some(Asked::Rollback { confirmed: false })
        );
        assert_eq!(
            Asked::of(ROLLBACK_CONFIRMED.trim_start_matches("/upgrade ")),
            Some(Asked::Rollback { confirmed: true })
        );
    }

    /// Progress at the quarter marks only, each said once; a finished upgrade
    /// or rollback asks for a restart into the binary it names; nothing else
    /// does.
    #[test]
    fn each_step_is_said_once_and_only_a_finished_one_restarts() {
        let mut last = -1;
        let (said, restart) = said_for(
            UpgradeEvent::ManifestFetched {
                version: "v9.9.9".into(),
            },
            &mut last,
        );
        assert!(said.is_some_and(|s| s.contains("v9.9.9")));
        assert!(restart.is_none());

        let mut quarters = 0;
        for bytes in [0u64, 10, 25, 25, 26, 50, 75, 99, 100] {
            let (said, restart) =
                said_for(UpgradeEvent::Downloading { bytes, total: 100 }, &mut last);
            quarters += usize::from(said.is_some());
            assert!(restart.is_none());
        }
        assert_eq!(quarters, 4, "25, 50, 75 and 100 — each once");

        let exe = PathBuf::from("/opt/atomcode/bin/atomcode");
        let (said, restart) = said_for(
            UpgradeEvent::Done {
                version: "v9.9.9".into(),
                backup: PathBuf::from("/opt/atomcode/bin/atomcode.bak"),
                exe: exe.clone(),
            },
            &mut last,
        );
        assert!(said.is_some());
        assert_eq!(restart, Some(exe.clone()));

        let (_, restart) = said_for(
            UpgradeEvent::RolledBack {
                exe: exe.clone(),
                backup: PathBuf::from("/opt/atomcode/bin/atomcode.bak"),
                updates: atomcode_updater::UpdatesAfterRollback::Paused,
            },
            &mut last,
        );
        assert_eq!(restart, Some(exe));

        let (_, restart) = said_for(UpgradeEvent::Failed("boom".into()), &mut last);
        assert!(restart.is_none());
    }

    /// "Already on the latest" and "a package manager owns this" are told in
    /// their own words, not as a failure.
    #[test]
    fn the_calm_outcomes_are_not_failures() {
        let latest = failed(&format!(
            "{}: already on v5.2.0 (latest is v5.2.0). Pass --force to reinstall.",
            atomcode_updater::ALREADY_LATEST
        ));
        assert_eq!(
            latest,
            t(Msg::UpgradeAlreadyLatest {
                current: "v5.2.0",
                latest: "v5.2.0"
            })
            .into_owned()
        );
        assert_eq!(
            failed(atomcode_updater::PACKAGE_MANAGED),
            t(Msg::UpgradePackageManaged).into_owned()
        );
        assert_eq!(
            failed("disk full"),
            t(Msg::UpgradeFailed { error: "disk full" }).into_owned()
        );
    }

    /// Once a new binary waits for its restart, the pending state is seen — the
    /// check `/upgrade` makes before it would download and replace again.
    #[test]
    fn a_waiting_restart_is_seen_until_it_is_taken() {
        assert!(!restart_pending());
        let restart = Restart {
            exe: PathBuf::from("/opt/atomcode/bin/atomcode"),
            rolled_back: false,
            notes: Vec::new(),
        };
        *RESTART.lock().expect("restart poisoned") = Some(restart.clone());
        assert!(restart_pending());
        assert_eq!(take_restart(), Some(restart));
        assert!(!restart_pending());
    }

    /// A package-managed install is pointed at its package manager, not `/upgrade`.
    #[test]
    fn the_hint_names_what_will_actually_upgrade_this_install() {
        assert_eq!(
            hint_for("v9.9.9", false),
            t(Msg::StatusUpgradeHint { version: "v9.9.9" }).into_owned()
        );
        assert_eq!(
            hint_for("v9.9.9", true),
            t(Msg::StatusUpgradeHintPm { version: "v9.9.9" }).into_owned()
        );
    }
}
