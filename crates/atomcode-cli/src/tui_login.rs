//! `/login` on the new screen: the same flow tuix's `/login` ran.
//!
//! Here and not in `atomcode-tui`, for the same reason `tui_onboarding` is
//! (`docs/plans/2026-09-19-remaining-gaps.md` 决策 2): credentials, what a
//! provider is, what counts as set up — none of that is the screen's. The
//! screen only shows what this hands it.
//!
//! **No modal.** tuix's `/login` never took the screen over: the QR code, the
//! URL, each step and the setup report went into scrollback through
//! `UiLine::CommandOutput`, with the input box still there. The same here — the
//! lines go into the conversation as the flow walks, which is what
//! [`atomcode_harness::seams::UserInterface::say`] is for. A modal would make
//! the report a frame the person has to read before it closes, and put a
//! finished step between them and the input box.
//!
//! tuix 的语义（`run_login_flow`, `atomcode-tuix/src/event_loop/commands.rs`）:
//!
//! 1. 未登录 → OAuth（QR + 轮询），凭据落盘；
//! 2. 然后**总是**跑 codingplan setup（claim/models/status，会更新已登录账号的
//!    codingplan 配置——「/login 是唯一的规范入口，/codingplan 已并入它」）;
//! 3. setup 报 `auth_expired`（本地 token 还没过期但服务端拒了）→ 再走**一次**
//!    OAuth，重跑 setup；
//! 4. 配置要落盘才写盘（半途而废的 setup 不写），并给活着的东西发 reload。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_harness::seams::{UiSvc, UserInterface};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::plugin::{AgentClientSvc, CommandsSvc, Repaint, RepaintSvc};
use serde_json::Value;

/// The row's name.
pub const ROW: &str = "tui-login";

/// The command this row takes over from the screen's shipped one.
pub const COMMAND: &str = "login";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// Mounts the command.
pub struct LoginRow {
    pub config_path: PathBuf,
    pub telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
}

#[async_trait]
impl Plugin for LoginRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "signing in: OAuth when needed, and what the account is entitled to"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        let client = ctx.require::<AgentClientSvc>().map_err(|e| e.to_string())?;
        let repaint = ctx.service::<RepaintSvc>();
        // Taken now, in an async context: the flow runs on a blocking thread,
        // which has no runtime of its own to spawn the reload onto.
        let runtime_handle = tokio::runtime::Handle::current();
        let telemetry = self.telemetry.clone();
        let path = self.config_path.clone();
        let set = Arc::new(LoginCommands {
            start: Arc::new(move |ui| {
                let world = World::production(
                    telemetry.clone(),
                    path.clone(),
                    client.clone(),
                    runtime_handle.clone(),
                    repaint.clone(),
                    ui.clone(),
                );
                let painted = {
                    let repaint = repaint.clone();
                    move || {
                        if let Some(repaint) = repaint.as_ref() {
                            repaint.now();
                        }
                    }
                };
                // Off the loop, like every other host-reaching gesture a command
                // makes: the loop must keep painting and keep accepting keys
                // while a login is polled.
                tokio::task::spawn_blocking(move || {
                    run_login_flow_with(&world, &ui, &painted);
                });
            }),
        });
        commands.add(set)?;
        Ok(())
    }
}

/// What the command sets going — a seam, so a criterion hands over something
/// that does not open a browser (the same shape [`crate::tui_onboarding`] uses).
type StartFlow = Arc<dyn Fn(Arc<dyn UserInterface>) + Send + Sync>;

struct LoginCommands {
    start: StartFlow,
}

#[async_trait]
impl CommandSet for LoginCommands {
    fn id(&self) -> &'static str {
        "cmd-tui-login"
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::new(
            COMMAND,
            "登录并配好 provider；已登录则刷新 codingplan 配置",
        )]
    }
    /// Takes the screen's shipped `/login` over rather than colliding with it:
    /// that one only re-read configuration, so a person who had just logged out
    /// got "requires login — run `/login`" from the very command they ran.
    fn overrides(&self) -> Vec<&'static str> {
        vec![COMMAND]
    }
    async fn run(&self, _name: &str, _args: &str, ctx: &Context) -> Outcome {
        let ui = match ctx.require::<UiSvc>() {
            Ok(ui) => ui,
            Err(error) => return Outcome::Refused(error.to_string()),
        };
        (self.start)(ui);
        // Nothing to say yet: the flow says it as it goes, into the
        // conversation. A command that answered here would be answering before
        // the thing it was asked for has happened.
        Outcome::Quiet
    }
}

/// Everything the flow reaches outside itself, as one value.
///
/// A struct rather than free calls because the sequence *is* the claim — "no
/// login prompt when already signed in", "one more OAuth when the server
/// disowned the token", "write the configuration only if setup got through" —
/// and none of those can be pinned while the flow talks to a server. Production
/// fills it with the real world ([`World::production`]); a criterion fills it
/// with a recording.
struct World {
    /// Read once, where `is_logged_in()` would be read.
    logged_in: bool,
    /// The configuration as it stands, where setup would read it.
    load: Arc<dyn Fn() -> atomcode_config::config::Config + Send + Sync>,
    /// One OAuth round: URL + QR, poll, exchange, save. Says what it is doing.
    login: Arc<dyn Fn(&dyn Say) -> Result<(), String> + Send + Sync>,
    /// claim/models/status.
    setup: Arc<
        dyn Fn(
                atomcode_config::config::Config,
            ) -> Result<
                (
                    atomcode_config::config::Config,
                    atomcode_codingplan::SetupReport,
                ),
                String,
            > + Send
            + Sync,
    >,
    /// Persist what setup worked out. Called only when the report says to.
    save: Arc<
        dyn Fn(
                &atomcode_config::config::Config,
                &atomcode_codingplan::SetupReport,
            ) -> Result<(), String>
            + Send
            + Sync,
    >,
    /// Tell whatever is alive that the configuration changed.
    reload: Arc<dyn Fn(&dyn Say) + Send + Sync>,
}

/// Where the flow's lines go.
///
/// A trait rather than the `UserInterface` itself so a criterion can read them:
/// "the QR went to the conversation, not to a modal" is exactly the claim, and
/// it is not assertable through a screen.
trait Say: Send + Sync {
    fn say(&self, text: &str);
}

/// The screen's front end, seen as somewhere lines go.
struct ScreenSay(Arc<dyn UserInterface>);

impl Say for ScreenSay {
    fn say(&self, text: &str) {
        self.0.say(text);
    }
}

impl World {
    /// The real one, wired for this launcher.
    fn production(
        telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
        config_path: PathBuf,
        client: Arc<atomcode_tui::plugin::AgentClient>,
        runtime_handle: tokio::runtime::Handle,
        repaint: Option<Arc<dyn Repaint>>,
        ui: Arc<dyn UserInterface>,
    ) -> Self {
        let load_path = config_path.clone();
        let load =
            Arc::new(move || atomcode_config::config::Config::load(&load_path).unwrap_or_default());

        let login_telemetry = telemetry.clone();
        let login = Arc::new(move |say: &dyn Say| run_oauth(say, &login_telemetry));

        // codingplan's `Client` wraps `reqwest::blocking::Client`, which stands
        // up a tokio runtime of its own and panics when dropped on an async
        // worker — the same reason the TUI's slash commands run on blocking
        // threads. `std::thread` (not `spawn_blocking`) because this whole flow
        // is already on one, and nesting is how that panic comes back.
        let setup_telemetry = telemetry.clone();
        let setup = Arc::new(move |config: atomcode_config::config::Config| {
            let tel = setup_telemetry.clone();
            std::thread::spawn(move || {
                let mut cfg = config;
                let report = atomcode_codingplan::run(
                    &mut cfg,
                    tel.as_ref(),
                    atomcode_codingplan::DefaultModelPolicy::AdoptServerDefault,
                );
                (cfg, report)
            })
            .join()
            .map_err(|_| "codingplan setup 线程崩了".to_string())
            .and_then(|(cfg, report)| report.map(|r| (cfg, r)).map_err(|e| format!("{e:#}")))
        });

        let save_path = config_path.clone();
        let save = Arc::new(
            move |config: &atomcode_config::config::Config,
                  report: &atomcode_codingplan::SetupReport| {
                atomcode_config::ConfigStore::new(save_path.clone())
                    .update(|latest| {
                        atomcode_codingplan::merge_successful_config(
                            latest,
                            config,
                            report,
                            atomcode_codingplan::DefaultModelPolicy::AdoptServerDefault,
                        )
                    })
                    .map_err(|error| format!("{error:#}"))?;
                // Non-fatal: the configuration already landed, and only the
                // staleness hint would be miscounted, which the next run
                // corrects.
                let _ = atomcode_codingplan::write_last_sync_now();
                Ok(())
            },
        );

        let reload = Arc::new(move |say: &dyn Say| {
            let Some(control) = client.control() else {
                return;
            };
            let root = client.root();
            let repaint = repaint.clone();
            // The screen is where the words go, so the failure is reported
            // through it — the same road the successful lines take. Nothing
            // borrows across the spawn: the failure is said through the front
            // end the closure already reaches, not through the caller's frame.
            let _ = say;
            let screen = ui.clone();
            runtime_handle.spawn(async move {
                if let Err(error) = control
                    .call(atomcode_host_api::HostCommand::Reload { session: root })
                    .await
                {
                    screen.say(&format!("配置已写入，但重新加载失败：{error:?}"));
                    if let Some(repaint) = repaint.as_ref() {
                        repaint.now();
                    }
                }
            });
        });

        Self {
            logged_in: atomcode_auth::is_logged_in(),
            load,
            login,
            setup,
            save,
            reload,
        }
    }
}

/// The sequence, with the world handed in.
///
/// This is what the criteria drive: with a recording world, "already signed in
/// means no login prompt" is an assertion instead of a hope.
fn run_login_flow_with(world: &World, ui: &Arc<dyn UserInterface>, painted: &dyn Fn()) {
    let say = |lines: String| {
        ui.say(&lines);
        painted();
    };
    // The world's own closures talk to a `Say`; the front end is what that is.
    let sink = ScreenSay(ui.clone());

    // Phase 1: 未登录才走 OAuth。已登录的账号不动，Phase 2 会用它的 token 把
    // codingplan 配置刷新一遍 —— 再弹一次登录是要人重做已经做过的事。
    if !world.logged_in {
        match (world.login)(&sink) {
            Ok(()) => say("登录成了，正在配 provider…".into()),
            Err(error) => {
                say(error);
                return;
            }
        }
    } else {
        say("已登录，正在刷新 codingplan 配置…".into());
    }

    // Phase 2: claim/models/status。
    let (mut prepared_config, mut report) = match (world.setup)((world.load)()) {
        Ok(pair) => pair,
        Err(error) => {
            say(format!("internal error：{error}"));
            return;
        }
    };

    // Phase 3: auth_expired —— 本地 token 还没过期但服务端拒了（被吊销、
    // refresh 死了）。再走一次 OAuth，重跑 setup。只有一次，不循环。
    if report.auth_expired {
        say("服务端不认这个 token 了，重新登录一次…".into());
        if let Err(error) = (world.login)(&sink) {
            say(error);
            return;
        }
        say("重新登录成了，重跑 setup…".into());
        match (world.setup)(prepared_config.clone()) {
            Ok((cfg, r)) => {
                prepared_config = cfg;
                report = r;
            }
            Err(error) => {
                say(format!("internal error：{error}"));
                return;
            }
        }
    }

    // Phase 4: 配置要落盘才写盘 —— 半途而废的 setup 不写。
    if report.should_persist_config() {
        if let Err(error) = (world.save)(&prepared_config, &report) {
            say(format!("配置没写成：{error}"));
            return;
        }
    }

    // The configuration is on disk, but the running graph was assembled before
    // it existed — the same "wrote the file is not making it so" gap
    // `tui_onboarding` closes with a reload. The session stays.
    (world.reload)(&sink);

    // **The report, in the conversation** — the same lines tuix rendered, said
    // into scrollback rather than held in a frame that closes over them.
    say(report.render());
}

/// One OAuth round: QR + URL into the conversation, poll to the end, exchange
/// and save.
///
/// **The save is not optional.** `LoginSession::finish` performs the token
/// exchange and *hands the credentials back* — writing them down is the
/// caller's step, which is why both other callers spell it out
/// (`atomcode login`, tuix's `run_login_flow`). Leaving it out is what this
/// command did, and how it looks is not an error but a **second, worse login**:
/// `is_logged_in()` still answers no, so the codingplan setup below runs its own
/// `step_login`, which reaches for the stdout-driven `oauth::login()` and prints
/// an English URL banner straight onto the frame — the URL landing past the
/// border because stdout was never the screen.
fn run_oauth(
    say: &dyn Say,
    telemetry: &Option<Arc<atomcode_telemetry::Telemetry>>,
) -> Result<(), String> {
    let session =
        atomcode_auth::oauth::start_login().map_err(|error| format!("登录起不来：{error:#}"))?;
    say.say(&login_chrome(session.url()));

    let ticks = session.spawn_poller(Duration::from_secs(2));
    loop {
        match ticks.recv() {
            Ok(Ok(atomcode_auth::oauth::PollOutcome::Authorized)) => break,
            Ok(Ok(atomcode_auth::oauth::PollOutcome::Pending)) => continue,
            Ok(Err(error)) => return Err(format!("登录没成：{error:#}")),
            Err(_) => return Err("登录没了回音".into()),
        }
    }

    let auth = session
        .finish(telemetry.as_ref())
        .map_err(|error| format!("换 token 没成：{error:#}"))?;
    keep_credentials(&auth)
}

/// Write down what a login just earned.
///
/// Apart from [`run_oauth`] because it is the step that is easy to forget and
/// hard to notice: the exchange succeeds, the process holds a good `AuthInfo`,
/// and nothing on disk changes — so `is_logged_in()` keeps answering no and the
/// next thing that wants a token goes and asks for another login. Naming it and
/// testing it is what keeps that from being a silent gap again.
fn keep_credentials(auth: &atomcode_auth::AuthInfo) -> Result<(), String> {
    atomcode_auth::save_auth(auth).map_err(|error| format!("登录成了，但凭据没写成：{error:#}"))
}

/// What the person reads while they reach for their phone: the code, then the
/// URL — as lines in the conversation, since that is where they will be looking.
fn login_chrome(url: &str) -> String {
    let mut chrome = String::new();
    if let Some(qr) = atomcode_tui::qr::text_code(url) {
        chrome.push_str(&qr);
        chrome.push('\n');
    }
    chrome.push_str("用手机扫码，或在浏览器里打开：");
    chrome.push('\n');
    chrome.push_str(url);
    chrome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A front end that only keeps what it was told.
    ///
    /// The claim is about **where the lines went**, so the lines are what the
    /// criterion has to be able to read — which a real screen cannot offer.
    #[derive(Default)]
    struct Recording {
        said: std::sync::Mutex<Vec<String>>,
    }

    impl Say for Recording {
        fn say(&self, text: &str) {
            self.said
                .lock()
                .expect("recording poisoned")
                .push(text.to_string());
        }
    }

    impl Recording {
        fn all(&self) -> String {
            self.said.lock().expect("recording poisoned").join("\n")
        }
    }

    /// A world that records what was touched and never reaches a server.
    struct World2 {
        logged_in: bool,
        auth_expired: bool,
        logins: AtomicUsize,
        setups: AtomicUsize,
        saves: AtomicUsize,
        reloads: AtomicUsize,
    }

    impl World2 {
        fn new(logged_in: bool, auth_expired: bool) -> Arc<Self> {
            Arc::new(Self {
                logged_in,
                auth_expired,
                logins: AtomicUsize::new(0),
                setups: AtomicUsize::new(0),
                saves: AtomicUsize::new(0),
                reloads: AtomicUsize::new(0),
            })
        }

        fn world(self: &Arc<Self>) -> World {
            let me = self.clone();
            let login = {
                let me = me.clone();
                Arc::new(move |say: &dyn Say| {
                    me.logins.fetch_add(1, Ordering::SeqCst);
                    say.say("用手机扫码，或在浏览器里打开：");
                    Ok(())
                })
            };
            let setup = {
                let me = me.clone();
                Arc::new(move |config: atomcode_config::config::Config| {
                    me.setups.fetch_add(1, Ordering::SeqCst);
                    Ok((config, report_with(me.auth_expired)))
                })
            };
            let save = {
                let me = me.clone();
                Arc::new(
                    move |_: &atomcode_config::config::Config,
                          _: &atomcode_codingplan::SetupReport| {
                        me.saves.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )
            };
            let reload = {
                let me = me.clone();
                Arc::new(move |_: &dyn Say| {
                    me.reloads.fetch_add(1, Ordering::SeqCst);
                })
            };
            World {
                logged_in: self.logged_in,
                load: Arc::new(atomcode_config::config::Config::default),
                login,
                setup,
                save,
                reload,
            }
        }
    }

    /// A report whose steps all got through — what `should_persist_config`
    /// reads.
    fn report_with(auth_expired: bool) -> atomcode_codingplan::SetupReport {
        atomcode_codingplan::SetupReport {
            login: atomcode_codingplan::StepResult::Skipped("already logged in".into()),
            claim: atomcode_codingplan::StepResult::Ok(atomcode_codingplan::setup::ClaimInfo {
                message: "Pro 生效".into(),
                duplicate: false,
                plan_type: atomcode_codingplan::PlanType::Pro,
            }),
            claim_attempts: Vec::new(),
            models: atomcode_codingplan::StepResult::Skipped("nothing to change".into()),
            status: atomcode_codingplan::StepResult::Skipped("nothing to change".into()),
            auth_expired,
        }
    }

    /// A front end that does nothing but exists, so the flow has a sink.
    struct Nowhere;

    #[async_trait]
    impl UserInterface for Nowhere {
        fn describe(&self) -> String {
            "nowhere".into()
        }
        async fn run(&self, _ctx: &Context, _initial: Option<String>) -> Result<(), String> {
            Ok(())
        }
    }

    fn sink() -> Arc<dyn UserInterface> {
        Arc::new(Nowhere)
    }

    /// **The bug this command had.** `/logout` then `/login` said "AtomGit
    /// gateway requires login — run `/login` first": the shipped `/login` only
    /// re-read configuration, so it never asked anyone to sign in. Not signed
    /// in is exactly when the prompt must appear.
    #[test]
    fn not_being_signed_in_is_what_opens_the_login() {
        let world = World2::new(false, false);
        let ui = sink();

        run_login_flow_with(&world.world(), &ui, &|| {});

        assert_eq!(
            world.logins.load(Ordering::SeqCst),
            1,
            "signed out, the flow signs in"
        );
    }

    /// **Signed in, don't ask again** — but do refresh what the account is
    /// entitled to, which is why a person runs `/login` a second time.
    #[test]
    fn being_signed_in_skips_the_login_and_still_runs_setup() {
        let world = World2::new(true, false);
        let ui = sink();

        run_login_flow_with(&world.world(), &ui, &|| {});

        assert_eq!(
            world.logins.load(Ordering::SeqCst),
            0,
            "already signed in, no second prompt"
        );
        assert_eq!(
            world.setups.load(Ordering::SeqCst),
            1,
            "and the entitlement refresh runs — that is what /login is for here"
        );
    }

    /// A server that disowned a locally-unexpired token: one more OAuth, one
    /// more setup. Exactly one, not a loop.
    #[test]
    fn a_disowned_token_is_replaced_once_and_setup_rerun() {
        let world = World2::new(true, true);
        let ui = sink();

        run_login_flow_with(&world.world(), &ui, &|| {});

        assert_eq!(world.logins.load(Ordering::SeqCst), 1, "one re-login");
        assert_eq!(
            world.setups.load(Ordering::SeqCst),
            2,
            "and setup runs again against the fresh token"
        );
    }

    /// **The report goes into the conversation, line by line.**
    ///
    /// The screen it was first written for held it in a modal body, which is
    /// both a frame the person has to read before it closes and — because a
    /// wizard body is a *list of lines* and one string with newlines in it is
    /// not — a single row that ran past the border. tuix never did either: the
    /// report was scrollback, like any other command's output.
    #[test]
    fn the_report_is_said_into_the_conversation_not_held_in_a_frame() {
        let world = World2::new(true, false);
        let recorder = Arc::new(Recording::default());
        run_login_flow_with(&world.world(), &sink_with(recorder.clone()), &|| {});

        let said = recorder.all();
        assert!(
            said.contains("already logged in"),
            "the report reached the conversation: {said}"
        );
        assert!(
            said.contains("CodingPlan claimed"),
            "and its other lines did too — it arrived as a report, not a row: {said}"
        );
        assert!(
            said.contains('\n'),
            "with breaks between the lines, the way a report reads: {said:?}"
        );
    }

    /// A sink that records, wearing the front end the flow asks for.
    fn sink_with(recorder: Arc<Recording>) -> Arc<dyn UserInterface> {
        Arc::new(RecorderFront(recorder))
    }

    struct RecorderFront(Arc<Recording>);

    #[async_trait]
    impl UserInterface for RecorderFront {
        fn describe(&self) -> String {
            "recording".into()
        }
        async fn run(&self, _ctx: &Context, _initial: Option<String>) -> Result<(), String> {
            Ok(())
        }
        fn say(&self, text: &str) {
            self.0.say(text);
        }
    }

    /// The write and the reload wait for setup to get through.
    #[test]
    fn the_write_and_the_reload_wait_for_setup_to_get_through() {
        let world = World2::new(true, false);
        let ui = sink();
        let order = Arc::new(AtomicUsize::new(0));
        let saved_at = Arc::new(AtomicUsize::new(0));
        let reloaded_at = Arc::new(AtomicUsize::new(0));

        let mut w = world.world();
        let inner_save = w.save.clone();
        let step = order.clone();
        let at = saved_at.clone();
        w.save = Arc::new(move |config, report| {
            let n = step.fetch_add(1, Ordering::SeqCst);
            at.store(n, Ordering::SeqCst);
            inner_save(config, report)
        });
        let inner_reload = w.reload.clone();
        let step = order.clone();
        let at = reloaded_at.clone();
        w.reload = Arc::new(move |say| {
            let n = step.fetch_add(1, Ordering::SeqCst);
            at.store(n, Ordering::SeqCst);
            inner_reload(say)
        });

        run_login_flow_with(&w, &ui, &|| {});

        assert_eq!(world.saves.load(Ordering::SeqCst), 1);
        assert_eq!(world.reloads.load(Ordering::SeqCst), 1);
        assert!(
            saved_at.load(Ordering::SeqCst) < reloaded_at.load(Ordering::SeqCst),
            "the write comes before the reload, not instead of it"
        );
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    /// **A login that does not reach the disk is not a login.**
    ///
    /// `LoginSession::finish` exchanges the code and hands an `AuthInfo` back;
    /// writing it is a separate step, and skipping it is silent — nothing errors,
    /// the process simply has credentials nobody else can see. What that looked
    /// like in practice: `/login` said it succeeded, `is_logged_in()` still said
    /// no, and codingplan's setup ran its own login — the stdout-driven one —
    /// printing an English URL banner onto the frame, past the border.
    #[test]
    fn a_finished_login_reaches_the_disk() {
        let home = tempfile::tempdir().expect("tempdir");
        std::env::set_var("ATOMCODE_HOME", home.path());

        assert!(
            !atomcode_auth::is_logged_in(),
            "nothing stored to begin with"
        );

        let auth = atomcode_auth::AuthInfo {
            access_token: "token".into(),
            refresh_token: Some("refresh".into()),
            token_type: "Bearer".into(),
            expires_in: Some(3600),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            user: atomcode_auth::UserInfo {
                id: "1".into(),
                username: "someone".into(),
                name: Some("Some One".into()),
                email: Some("someone@example.com".into()),
                avatar_url: None,
            },
        };

        keep_credentials(&auth).expect("the credentials land");

        assert!(
            atomcode_auth::is_logged_in(),
            "after the save, the process sees the login — this is the answer \
             `step_login` reads, and answering no is what sent it to the \
             stdout-driven login"
        );
        assert!(atomcode_auth::auth_file_path().is_file(), "there is a file");
    }
}
