//! Getting a machine to the point where it can do a turn: the row, and the
//! steps it puts on screen.
//!
//! Here and not in `atomcode-tui` (决策 2 of
//! `docs/plans/2026-09-19-remaining-gaps.md`): credentials, what a provider is,
//! what counts as being set up and what to do about it are all this launcher's,
//! and the screen owns none of them. What crosses is a list of steps and a
//! picture — the screen draws them, says which step it is on, and hands back
//! the answers. It is the same division `tui_settings` makes, for the same
//! reason (`docs/adr/0022` §3).
//!
//! **Nothing here prints.** A line on stdout under a full-screen UI lands in
//! the middle of the frame. Everything this flow has to say goes into the step
//! it is on.

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_harness::seams::UiSvc;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::overlay::Choice;
use atomcode_tui::plugin::{AgentClientSvc, CommandsSvc, Repaint, RepaintSvc};
use atomcode_tui::wizard::{StepDef, StepKind, Wizard};
use serde_json::Value;

/// The row's name, shared by the plugin and the layer that names it.
pub const ROW: &str = "tui-onboarding";

/// The command readiness names when nothing is configured, and the one a person
/// can type to run it again.
pub const COMMAND: &str = "onboarding";

/// What the wizard closes with. Hidden: nobody types it, and it would be noise
/// in the menu.
const FINISHED: &str = "onboarding-finished";

/// The modal's id, for the frame's border and for closing it by name.
const MODAL: &str = "onboarding";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// Mounts the steps as a command.
pub struct OnboardingRow {
    pub config_path: PathBuf,
    pub telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
}

#[async_trait]
impl Plugin for OnboardingRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "getting a machine ready to work: language, signing in, and what was set up"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        let telemetry = self.telemetry.clone();
        let path = self.config_path.clone();
        // The runtime's end of the connection, so the wizard can hand it a
        // reload once the login has written the configuration: writing the file
        // is not making it so (see `apply_setting` in `atomcode-tui`).
        let client = ctx.require::<AgentClientSvc>().map_err(|e| e.to_string())?;
        // The handle now, in an async context: `sign_in` runs on a blocking
        // thread, where `tokio::spawn` would panic — the spawn for the reload
        // has to be handed one taken from the runtime itself.
        let runtime_handle = tokio::runtime::Handle::current();
        // The screen, so the wizard can hand it `/clear` once the reload is
        // through: opening the session the login made possible is the screen's
        // own dispatch, the same road a person's `/clear` takes.
        let ui = ctx.require::<UiSvc>().map_err(|e| e.to_string())?;
        let screen = ui.clone();
        let set = Arc::new(Onboarding {
            config_path: self.config_path.clone(),
            screen: Some(screen),
            start_sign_in: Arc::new(move |wizard, repaint| {
                sign_in(
                    wizard,
                    repaint,
                    telemetry.clone(),
                    path.clone(),
                    client.clone(),
                    ui.clone(),
                    runtime_handle.clone(),
                );
            }),
            live: Mutex::new(None),
        });
        commands.add(set)?;
        Ok(())
    }
}

/// What the waiting step sets going.
///
/// A seam because it is the one thing here that talks to a server: production
/// signs in, and a criterion hands over something that does not. Without it
/// every criterion about the steps would open a browser.
type StartSignIn = Arc<dyn Fn(Arc<Wizard>, Option<Arc<dyn Repaint>>) + Send + Sync>;

/// The steps, and the one that is on screen.
struct Onboarding {
    config_path: PathBuf,
    start_sign_in: StartSignIn,
    /// The screen, for the one answer that is not a sentence: "configure one
    /// myself" ends by opening the provider panel, which is the screen's own
    /// `/provider` and not a second road to it.
    screen: Option<Arc<dyn atomcode_harness::seams::UserInterface>>,
    /// Kept so the answers can be read when it closes: a modal closes with one
    /// value and these are several.
    live: Mutex<Option<Arc<Wizard>>>,
}

/// The three answers to "where does the provider come from".
///
/// Named rather than spelled at each use: the same word is a choice's value, a
/// branch in the callback that decides whether to sign in, and a branch in
/// `finish` — three places, and a typo in any one of them is a silent fallback
/// to the CodingPlan path.
const CODINGPLAN: &str = "codingplan";
const BY_HAND: &str = "manual";
const NOT_NOW: &str = "skip";

/// What each step asks.
///
/// `warn_it_clears` prepends the one step that is not about setting anything
/// up: `/welcome` run in the middle of a conversation ends, if the sign-in
/// works, by opening a fresh session — and the conversation on screen is not in
/// it. Asked before the walkthrough rather than at the moment it happens,
/// because by then the person has signed in and the answer can only be "too
/// late". A first launch has nothing to lose and is not asked.
fn steps(warn_it_clears: bool) -> Vec<StepDef> {
    let mut steps = Vec::new();
    if warn_it_clears {
        // `Note`, so leaving is `esc` — the same key the intro already tells
        // people about, rather than a second idiom for the same "no".
        steps.push(
            StepDef::new(
                "would-clear",
                tr(SMsg::OnboardWouldClearTitle),
                StepKind::Note,
            )
            .saying(vec![
                tr(SMsg::OnboardWouldClearLine1).into_owned(),
                tr(SMsg::OnboardWouldClearLine2).into_owned(),
            ]),
        );
    }
    steps.extend([
        StepDef::new("intro", tr(SMsg::OnboardIntroTitle), StepKind::Note).saying(vec![
            tr(SMsg::OnboardIntroLine1).into_owned(),
            tr(SMsg::OnboardIntroLine2).into_owned(),
            tr(SMsg::OnboardIntroLine3).into_owned(),
        ]),
        StepDef::new(
            "language",
            tr(SMsg::OnboardLanguageTitle),
            StepKind::Choose(vec![
                // The two language names are each written in their own
                // language: a person looking for English is looking for the
                // word "English", whichever language the screen is in.
                Choice::new("zh_CN", tr(SMsg::OnboardLanguageChinese)),
                Choice::new("en", "English"),
                Choice::new("auto", tr(SMsg::OnboardLanguageFollowSystem))
                    .about(tr(SMsg::OnboardLanguageFollowSystemAbout)),
            ]),
        ),
        // Three answers, not two. Skipping used to be the only alternative to
        // signing in, which left everyone with an API key of their own — or a
        // model they host — finishing the walkthrough on a machine that still
        // had no provider, with nothing said about where to put one.
        StepDef::new(
            "setup",
            tr(SMsg::OnboardSetupTitle),
            StepKind::Choose(vec![
                Choice::new(CODINGPLAN, tr(SMsg::OnboardSetupCodingPlan))
                    .about(tr(SMsg::OnboardSetupCodingPlanAbout)),
                Choice::new(BY_HAND, tr(SMsg::OnboardSetupManual))
                    .about(tr(SMsg::OnboardSetupManualAbout)),
                Choice::new(NOT_NOW, tr(SMsg::OnboardSetupSkip))
                    .about(tr(SMsg::OnboardSetupSkipAbout)),
            ]),
        ),
        StepDef::new(
            "login",
            tr(SMsg::OnboardLoginTitle),
            StepKind::Wait { skippable: true },
        )
        .saying(vec![tr(SMsg::OnboardFetchingLoginUrl).into_owned()]),
        StepDef::new("confirm", tr(SMsg::OnboardConfirmTitle), StepKind::Note),
    ]);
    steps
}

impl Onboarding {
    /// Build the modal and start whatever each step needs as it opens.
    fn open(&self, repaint: Option<Arc<dyn Repaint>>, warn_it_clears: bool) -> Arc<Wizard> {
        // The callback needs the wizard the callback is being built for, so it
        // takes a weak handle filled in immediately after. The first step opens
        // inside `Wizard::new`, before this is set — which is why the first step
        // is one that needs nothing started.
        let slot: Arc<OnceLock<Weak<Wizard>>> = Arc::new(OnceLock::new());
        let here = slot.clone();
        let start = self.start_sign_in.clone();
        let wizard = Wizard::new(
            MODAL,
            tr(SMsg::OnboardModalTitle),
            steps(warn_it_clears),
            Box::new(move |id| {
                if id != "login" {
                    return;
                }
                let Some(wizard) = here.get().and_then(Weak::upgrade) else {
                    return;
                };
                // Only the CodingPlan answer has anything to wait for. The
                // other two answer this step where it stands — inside the
                // `advance` that opened it, so the step never reaches a frame
                // and nobody watches a "sign in" they said no to.
                if answered(&wizard, "setup").as_deref() == Some(CODINGPLAN) {
                    start(wizard, repaint.clone());
                } else {
                    wizard.resolve(String::new());
                }
            }),
            FINISHED,
        );
        let _ = slot.set(Arc::downgrade(&wizard));
        *self.live.lock().expect("onboarding poisoned") = Some(wizard.clone());
        wizard
    }

    /// What the answers add up to, once the last step is done.
    fn finish(&self) -> String {
        let Some(wizard) = self.live.lock().expect("onboarding poisoned").take() else {
            return tr(SMsg::OnboardAlreadyFinished).into_owned();
        };
        let answers = wizard.answers();
        let answer = |id: &str| {
            answers
                .iter()
                .find(|(had, _)| had == id)
                .and_then(|(_, answer)| answer.clone())
        };

        let mut said = Vec::new();
        if let Some(language) = answer("language") {
            match set_setting(&self.config_path, "language", &language) {
                Ok(()) => said.push(
                    tr(SMsg::OnboardLanguageSet {
                        language: &language,
                    })
                    .into_owned(),
                ),
                Err(error) => {
                    said.push(tr(SMsg::OnboardLanguageNotWritten { error: &error }).into_owned())
                }
            }
        }
        // Which of the three was chosen decides what "done" means. Read here
        // and not from the login step's answer: the two that do not sign in
        // answer it where it stands, and an empty answer would read as a
        // sign-in that returned nothing.
        match answer("setup").as_deref() {
            Some(BY_HAND) => {
                said.push(tr(SMsg::OnboardSetupByHand).into_owned());
                // The panel the person needs, opened for them — through the
                // screen's own `/provider`, the same road a person's hand
                // takes. A second way in is a second thing to keep in step.
                if let Some(screen) = self.screen.as_ref() {
                    screen.run_slash("provider");
                }
            }
            Some(NOT_NOW) => said.push(tr(SMsg::OnboardLoginSkipped).into_owned()),
            _ => match answer("login") {
                Some(detail) => said.push(detail),
                // A skip is the absence of an answer, which is why it is worth
                // saying out loud: the machine is still not ready.
                None => said.push(tr(SMsg::OnboardLoginSkipped).into_owned()),
            },
        }
        said.join("\n")
    }
}

/// One step's answer, by id.
fn answered(wizard: &Arc<Wizard>, step: &str) -> Option<String> {
    wizard
        .answers()
        .into_iter()
        .find(|(had, _)| had == step)
        .and_then(|(_, answer)| answer)
}

/// Whether this screen already has a conversation on it.
///
/// A user message is the marker rather than "the log is not empty": a session
/// carries facts of its own from the moment it opens, and clearing a screen
/// nobody has said anything on costs nothing.
fn conversation_has_begun(events: &[atomcode_kernel::session::LoggedEvent]) -> bool {
    events.iter().any(|logged| {
        matches!(
            logged.event,
            atomcode_kernel::session::SessionEvent::UserMessage { .. }
        )
    })
}

/// Write one setting, the way the settings panel writes one.
fn set_setting(path: &PathBuf, id: &str, value: &str) -> Result<(), String> {
    let spec = atomcode_config::settings::SETTINGS
        .iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| tr(SMsg::NoSuchSetting { id }).into_owned())?;
    atomcode_config::ConfigStore::new(path.clone())
        .update_document(|document| spec.patch(document, value))
        .map_err(|error| format!("{error:#}"))?;
    Ok(())
}

/// Sign in, and tell the step what is happening while it happens.
///
/// On a blocking thread throughout: the OAuth flow builds a `reqwest::blocking`
/// client, which stands up a runtime of its own and panics when dropped on an
/// async worker — the same reason `run()` moves the command-line login onto
/// one.
fn sign_in(
    wizard: Arc<Wizard>,
    repaint: Option<Arc<dyn Repaint>>,
    telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
    config_path: PathBuf,
    client: std::sync::Arc<atomcode_tui::plugin::AgentClient>,
    ui: std::sync::Arc<dyn atomcode_harness::seams::UserInterface>,
    runtime_handle: tokio::runtime::Handle,
) {
    let painted = move || {
        if let Some(repaint) = repaint.as_ref() {
            repaint.now();
        }
    };
    tokio::task::spawn_blocking(move || {
        let session = match atomcode_auth::oauth::start_login() {
            Ok(session) => session,
            Err(error) => {
                wizard.say(vec![
                    tr(SMsg::LoginCouldNotStart {
                        error: &format!("{error:#}"),
                    })
                    .into_owned(),
                    skip_hint(),
                ]);
                painted();
                return;
            }
        };
        let url = session.url().to_string();
        wizard.say(vec![
            tr(SMsg::ScanOrOpen).into_owned(),
            url.clone(),
            String::new(),
            skip_hint(),
        ]);
        wizard.show(atomcode_tui::qr::code(&url));
        painted();

        let ticks = session.spawn_poller(Duration::from_secs(2));
        loop {
            match ticks.recv() {
                Ok(Ok(atomcode_auth::oauth::PollOutcome::Authorized)) => break,
                Ok(Ok(atomcode_auth::oauth::PollOutcome::Pending)) => continue,
                Ok(Err(error)) => {
                    wizard.say(vec![
                        tr(SMsg::LoginFailed {
                            error: &format!("{error:#}"),
                        })
                        .into_owned(),
                        skip_hint(),
                    ]);
                    painted();
                    return;
                }
                // The poller stopped without an answer.
                Err(_) => {
                    wizard.say(vec![tr(SMsg::LoginNoAnswer).into_owned(), skip_hint()]);
                    painted();
                    return;
                }
            }
        }

        wizard.say(vec![tr(SMsg::SignedInSettingUpProvider).into_owned()]);
        wizard.show(None);
        painted();

        let detail = match finish_login(session, telemetry.as_ref(), &config_path) {
            Ok(detail) => detail,
            Err(error) => {
                wizard.say(vec![tr(SMsg::OnboardSignedInConfigNotWritten {
                    error: &format!("{error:#}"),
                })
                .into_owned()]);
                painted();
                return;
            }
        };
        // The configuration is on disk, but the running graph was assembled
        // before it existed — the same "wrote the file is not making it so"
        // gap the settings panel closes with `HostCommand::Reload`. The
        // command is a rebuild of what changed, not a restart, and the session
        // stays. On its own task, like every other host command from off the
        // loop; a failure is said out loud rather than swallowed.
        if let Some(control) = client.control() {
            let root = client.root();
            let wizard = wizard.clone();
            // On the runtime's handle, not `tokio::spawn`: this runs on a
            // blocking thread, which has no runtime context to spawn onto.
            runtime_handle.spawn(async move {
                if let Err(error) = control
                    .call(atomcode_host_api::HostCommand::Reload { session: root })
                    .await
                {
                    wizard.say(vec![tr(SMsg::ConfigWrittenReloadFailedCli {
                        error: &format!("{error:?}"),
                    })
                    .into_owned()]);
                    return;
                }
                // The reload rebuilt the runtime; `/clear` is the transition a
                // person's own hand proves opens the new session — fresh turn,
                // welcome block and all — so the wizard takes it for them
                // rather than leaving a screen that still says it cannot work.
                ui.run_slash("clear");
            });
        }
        wizard.resolve(detail);
        painted();
    });
}

/// The line under every step that can be left: read from the table at the
/// moment it is shown, which is why it is a function and not a `const`.
fn skip_hint() -> String {
    tr(SMsg::OnboardSkipHint).into_owned()
}

/// Exchange the token, save it, and set up whatever the account is entitled to.
///
/// The same three things `atomcode login` does after the browser comes back —
/// save the credentials, run the setup, and persist what it worked out. The
/// answer carries the setup report rendered the way `/codingplan` renders it
/// (the same visual contract tuix's login shows), not just a name: what the
/// account is entitled to is what the confirm step is for.
fn finish_login(
    session: atomcode_auth::oauth::LoginSession,
    telemetry: Option<&Arc<atomcode_telemetry::Telemetry>>,
    path: &PathBuf,
) -> anyhow::Result<String> {
    let auth = session.finish(telemetry)?;
    atomcode_auth::save_auth(&auth)?;

    let mut config = atomcode_config::config::Config::load(path).unwrap_or_default();
    let report = atomcode_codingplan::run(
        &mut config,
        telemetry,
        atomcode_codingplan::DefaultModelPolicy::AdoptServerDefault,
    )?;
    if report.should_persist_config() {
        atomcode_config::ConfigStore::new(path.clone()).update(|latest| {
            atomcode_codingplan::merge_successful_config(
                latest,
                &config,
                &report,
                atomcode_codingplan::DefaultModelPolicy::AdoptServerDefault,
            )
        })?;
        // Non-fatal: the configuration already landed, and only the staleness
        // hint would be miscounted — which the next run corrects.
        let _ = atomcode_codingplan::write_last_sync_now();
    }
    Ok(report.render())
}

#[async_trait]
impl CommandSet for Onboarding {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        // `/welcome` is what the classic screen called the same walkthrough; the
        // name people typed there keeps working here.
        vec![Command::said(COMMAND, tr(SMsg::CmdAboutOnboarding)).with_aliases(&["welcome"])]
    }

    fn hidden(&self) -> Vec<Command> {
        vec![Command::said(
            FINISHED,
            tr(SMsg::CmdAboutOnboardingFinished),
        )]
    }

    async fn run(&self, name: &str, _args: &str, ctx: &Context) -> Outcome {
        match name {
            COMMAND => {
                // Asked of the screen's own client, which is where a
                // conversation is: the walkthrough itself has no idea whether
                // anybody has said anything yet.
                let warn_it_clears = ctx
                    .service::<AgentClientSvc>()
                    .is_some_and(|client| conversation_has_begun(&client.events()));
                Outcome::Open(self.open(ctx.service::<RepaintSvc>(), warn_it_clears))
            }
            FINISHED => Outcome::Said(self.finish()),
            _ => Outcome::Quiet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_tui::overlay::{Overlay, Step};
    use atomcode_tui::surface::{Key, KeyPress};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("onboarding-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.join("config.toml")
    }

    /// Nothing here reaches a server.
    fn flow(path: PathBuf, start: StartSignIn) -> Onboarding {
        Onboarding {
            config_path: path,
            start_sign_in: start,
            // No screen: a criterion that opened the provider panel would be
            // asserting about a screen it does not have.
            screen: None,
            live: Mutex::new(None),
        }
    }

    fn enter(w: &Arc<Wizard>) -> Step {
        w.key(KeyPress::plain(Key::Enter))
    }

    /// Only the step that waits sets anything going, and only as it opens.
    ///
    /// The property that makes the rest of this testable, and a real one: a
    /// wizard that started signing in when it was built would open a browser
    /// at the person the moment the screen came up, before they had chosen
    /// anything — including before they could refuse.
    #[test]
    fn nothing_is_set_going_until_the_step_that_waits_opens() {
        let started = Arc::new(AtomicUsize::new(0));
        let count = started.clone();
        let flow = flow(
            scratch("starts"),
            Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let w = flow.open(None, false);
        assert_eq!(started.load(Ordering::SeqCst), 0, "not while it is built");

        enter(&w); // intro
        assert_eq!(started.load(Ordering::SeqCst), 0, "not on the way past");
        enter(&w); // language: the first choice
        assert_eq!(
            started.load(Ordering::SeqCst),
            0,
            "and not before the person has said where the provider comes from"
        );
        enter(&w); // setup: CodingPlan, the first choice
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "once, as the waiting step opens"
        );
    }

    /// What was answered is what is written.
    #[test]
    fn the_answers_are_applied_when_the_last_step_is_done() {
        let path = scratch("answers");
        let flow = flow(
            path.clone(),
            Arc::new(|wizard, _| {
                // Stand in for a login that landed.
                wizard.resolve("登录成了：someone");
            }),
        );
        let w = flow.open(None, false);
        enter(&w); // intro
        w.key(KeyPress::plain(Key::Down)); // English
        enter(&w); // language
        enter(&w); // setup: CodingPlan → the waiting step, which resolves itself
        assert_eq!(
            enter(&w),
            Step::Chose(FINISHED.into()),
            "the last step closes with the command this row owns"
        );

        let said = flow.finish();
        assert!(said.contains("en"), "{said}");
        assert!(said.contains("登录成了"), "{said}");
        let written = std::fs::read_to_string(&path).expect("the setting was written");
        assert!(written.contains("language"), "{written}");
        assert!(written.contains("en"), "{written}");
    }

    /// Skipping the login is not the same as having signed in.
    ///
    /// The distinction the wizard keeps (`None` is a skip, `Some("")` is an
    /// answer) only matters if someone reads it: a machine whose login was
    /// skipped still has no provider, and saying "done" would be a lie.
    #[test]
    fn a_skipped_login_says_the_machine_is_still_not_ready() {
        let flow = flow(
            scratch("skip"),
            // Never resolves: the person presses enter to pass it by.
            Arc::new(|_, _| {}),
        );
        let w = flow.open(None, false);
        enter(&w); // intro
        enter(&w); // language
        enter(&w); // setup: CodingPlan, so the wait really opens
        enter(&w); // skip the wait
        assert_eq!(enter(&w), Step::Chose(FINISHED.into()));

        let said = flow.finish();
        assert!(said.contains("跳过"), "{said}");
        assert!(said.contains(COMMAND), "it says how to come back: {said}");
    }

    /// "Configure one myself" does not sign in, and does not call it skipped.
    ///
    /// The gap this closes: the walkthrough had two answers, sign in or skip,
    /// so everyone with an API key of their own — or a model they host — came
    /// out of it on a machine with no provider and nothing said about where to
    /// put one. The branch has to do two things and both are easy to lose: not
    /// start a sign-in nobody asked for, and say where the settings are.
    #[test]
    fn configuring_by_hand_starts_no_login_and_says_where_to_go() {
        let started = Arc::new(AtomicUsize::new(0));
        let count = started.clone();
        let flow = flow(
            scratch("by-hand"),
            Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let w = flow.open(None, false);
        enter(&w); // intro
        enter(&w); // language
        w.key(KeyPress::plain(Key::Down)); // past CodingPlan
                                           // The waiting step is answered inside this same press, so the one
                                           // after it is the last one.
        enter(&w); // setup: configure one myself
        assert_eq!(
            started.load(Ordering::SeqCst),
            0,
            "no browser was opened at someone who said they would do it themselves"
        );
        assert_eq!(
            enter(&w),
            Step::Chose(FINISHED.into()),
            "and the wait did not stand between them and the end"
        );

        let said = flow.finish();
        assert!(
            said.contains("provider"),
            "it says where the key goes: {said}"
        );
        assert!(
            !said.contains(tr(SMsg::OnboardLoginSkipped).as_ref()),
            "and does not report it as a skip — nothing was skipped: {said}"
        );
    }

    /// Saying "not now" is still a skip, with the step in between.
    #[test]
    fn saying_not_now_is_reported_as_a_skip() {
        let started = Arc::new(AtomicUsize::new(0));
        let count = started.clone();
        let flow = flow(
            scratch("not-now"),
            Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let w = flow.open(None, false);
        enter(&w); // intro
        enter(&w); // language
        w.key(KeyPress::plain(Key::Down));
        w.key(KeyPress::plain(Key::Down)); // past both
        enter(&w); // setup: not now
        assert_eq!(started.load(Ordering::SeqCst), 0, "nothing was set going");
        assert_eq!(enter(&w), Step::Chose(FINISHED.into()));
        assert!(flow.finish().contains("跳过"));
    }

    /// `/welcome` in the middle of a conversation says so before it starts.
    ///
    /// A successful sign-in ends by opening a fresh session, which is right on
    /// a machine that could not work a minute ago and wrong on one that has
    /// been working all morning — the conversation on screen is not in the new
    /// session. Asked first, because asked afterwards the only honest answer
    /// is "too late". A first launch has nothing to lose and is not asked: a
    /// question with one sensible answer teaches people to stop reading them.
    #[test]
    fn a_walkthrough_that_would_clear_the_screen_says_so_first() {
        let flow = flow(scratch("warns"), Arc::new(|_, _| {}));

        let fresh = flow.open(None, false);
        assert_eq!(
            fresh.total(),
            5,
            "a first launch is not asked about a conversation it does not have"
        );

        let mid = flow.open(None, true);
        assert_eq!(mid.total(), 6, "and one that would lose something is");
        // Leaving is `esc`, the same key every other step takes — and it
        // answers nothing, so nothing was set up by asking.
        assert_eq!(
            mid.key(KeyPress::plain(Key::Esc)),
            Step::Cancelled,
            "saying no leaves, rather than going on to the next step"
        );
        assert!(
            mid.answers().is_empty(),
            "and nothing was answered on the way out: {:?}",
            mid.answers()
        );
    }

    /// What counts as "there is a conversation here".
    ///
    /// A user message, not a non-empty log: a session carries facts of its own
    /// from the moment it opens, and clearing a screen nobody has said anything
    /// on costs nothing. Asking anyway is the question people learn to skip.
    #[test]
    fn only_something_a_person_said_counts_as_a_conversation() {
        use atomcode_kernel::session::{LoggedEvent, SessionEvent};
        let logged = |event| LoggedEvent {
            seq: 1,
            at: 0,
            event,
        };
        assert!(!conversation_has_begun(&[]));
        assert!(!conversation_has_begun(&[logged(
            SessionEvent::TurnStart { turn: 1 }
        )]));
        assert!(conversation_has_begun(&[logged(
            SessionEvent::UserMessage {
                turn: 1,
                text: "hello".into(),
                images: Vec::new(),
            }
        )]));
    }

    /// The command readiness names is one this row actually contributes.
    ///
    /// The lesson from the ACP command table: a name advertised by one place
    /// and implemented in another drifts silently, and the person finds out by
    /// running it.
    #[test]
    fn the_command_readiness_names_is_one_this_row_runs() {
        let flow = flow(scratch("named"), Arc::new(|_, _| {}));
        let listed: Vec<String> = flow
            .commands()
            .into_iter()
            .chain(flow.hidden())
            .map(|c| c.name.to_string())
            .collect();
        assert!(listed.contains(&COMMAND.to_string()), "{listed:?}");
        assert!(listed.contains(&FINISHED.to_string()), "{listed:?}");

        match crate::host::readiness_for(Some(
            atomcode_coding::ProviderUnavailableReason::NotConfigured,
        )) {
            atomcode_host_api::HostReply::Readiness { fix, .. } => {
                assert_eq!(fix.as_deref(), Some(COMMAND));
            }
            other => panic!("{other:?}"),
        }
    }
}
