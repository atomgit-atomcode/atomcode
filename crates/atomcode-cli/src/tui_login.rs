//! `/login` on the new screen: the same flow tuix's `/login` ran, on the row
//! the new architecture assembles.
//!
//! Here and not in `atomcode-tui`, for the same reason `tui_onboarding` is
//! (`docs/plans/2026-09-19-remaining-gaps.md` 决策 2): credentials, what a
//! provider is, what counts as set up — none of that is the screen's. The
//! screen only draws what this hands it and takes back what it answers.
//!
//! tuix 的语义（`run_login_flow`, `atomcode-tuix/src/event_loop/commands.rs`）:
//!
//! 1. 未登录 → OAuth（QR + 轮询），凭据落盘；
//! 2. 然后**总是**跑 codingplan setup（claim/models/status，会更新已登录账号的
//!    codingplan 配置——「/login 是唯一的规范入口，/codingplan 已并入它」）;
//! 3. setup 报 `auth_expired`（本地 token 还没过期但服务端拒了）→ 再走**一次**
//!    OAuth，重跑 setup；
//! 4. 配置要落盘才写盘（半途而废的 setup 不写），并给活着的东西发 reload。
//!
//! The old screen folded `/codingplan` into `/login` for exactly this reason:
//! a person asking to log in wants a *working* provider afterwards, and a
//! login that leaves the config half-wired answers "logged in" about the
//! smaller half.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::plugin::{AgentClientSvc, CommandsSvc, Repaint, RepaintSvc};
use atomcode_tui::wizard::{StepDef, StepKind, Wizard};
use serde_json::Value;

/// The row's name.
pub const ROW: &str = "tui-login";

/// The command this row takes over from the screen's shipped one.
pub const COMMAND: &str = "login";

/// The overlay's id, for the frame's border and for closing it by name.
const MODAL: &str = "login";

/// What closes the wizard — hidden, like onboarding's.
const FINISHED: &str = "login-finished";

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
        // Taken now, in an async context: the flow runs on a blocking thread,
        // which has no runtime of its own to spawn the reload onto.
        let runtime_handle = tokio::runtime::Handle::current();
        let telemetry = self.telemetry.clone();
        let path = self.config_path.clone();
        let set = Arc::new(LoginCommands {
            start: Arc::new(move |wizard, repaint| {
                let world = World::production(
                    telemetry.clone(),
                    path.clone(),
                    client.clone(),
                    runtime_handle.clone(),
                    repaint.clone(),
                );
                let hook = repaint;
                let painted = move || {
                    if let Some(repaint) = hook.as_ref() {
                        repaint.now();
                    }
                };
                tokio::task::spawn_blocking(move || {
                    run_login_flow_with(&world, &wizard, &painted);
                });
            }),
        });
        commands.add(set)?;
        Ok(())
    }
}

/// What the command sets going — a seam, so a criterion hands over something
/// that does not open a browser (the same shape [`crate::tui_onboarding`] uses).
type StartFlow = Arc<dyn Fn(Arc<Wizard>, Option<Arc<dyn Repaint>>) + Send + Sync>;

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
    /// got "requires login — run /login" from the very command they ran.
    fn overrides(&self) -> Vec<&'static str> {
        vec![COMMAND]
    }
    async fn run(&self, _name: &str, _args: &str, ctx: &Context) -> Outcome {
        // One note-step wizard: it exists to hold what the flow says while it
        // runs, the way onboarding's login step does.
        let wizard = Wizard::new(
            MODAL,
            "登录",
            vec![
                StepDef::new("flow", "登录", StepKind::Note).saying(vec!["正在看登录状态…".into()])
            ],
            Box::new(|_| {}),
            FINISHED,
        );
        (self.start)(wizard.clone(), ctx.service::<RepaintSvc>());
        Outcome::Open(wizard)
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
    /// One OAuth round: URL + QR, poll, exchange, save. Draws onto the wizard.
    login: Arc<dyn Fn(&Arc<Wizard>, &dyn Fn()) -> Result<(), String> + Send + Sync>,
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
    reload: Arc<dyn Fn(&Arc<Wizard>, &dyn Fn()) + Send + Sync>,
}

impl World {
    /// The real one, wired for this launcher.
    fn production(
        telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
        config_path: PathBuf,
        client: Arc<atomcode_tui::plugin::AgentClient>,
        runtime_handle: tokio::runtime::Handle,
        repaint: Option<Arc<dyn Repaint>>,
    ) -> Self {
        let load_path = config_path.clone();
        let load =
            Arc::new(move || atomcode_config::config::Config::load(&load_path).unwrap_or_default());

        let login_telemetry = telemetry.clone();
        let login = Arc::new(move |wizard: &Arc<Wizard>, painted: &dyn Fn()| {
            run_oauth(wizard, &login_telemetry, painted)
        });

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

        let reload = Arc::new(move |wizard: &Arc<Wizard>, _painted: &dyn Fn()| {
            let Some(control) = client.control() else {
                return;
            };
            let root = client.root();
            let wizard = wizard.clone();
            // The repaint hook rides along as the `Arc` it already is: the
            // spawn is `'static`, so it cannot borrow the caller's frame.
            let repaint = repaint.clone();
            runtime_handle.spawn(async move {
                if let Err(error) = control
                    .call(atomcode_host_api::HostCommand::Reload { session: root })
                    .await
                {
                    wizard.say(vec![format!("配置已写入，但重新加载失败：{error:?}")]);
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
fn run_login_flow_with(world: &World, wizard: &Arc<Wizard>, painted: &dyn Fn()) {
    let say = |lines: Vec<String>| {
        wizard.say(lines);
        painted();
    };

    // Phase 1: 未登录才走 OAuth。已登录的账号不动，Phase 2 会用它的 token 把
    // codingplan 配置刷新一遍 —— 再弹一次登录是要人重做已经做过的事。
    if !world.logged_in {
        if let Err(error) = (world.login)(wizard, painted) {
            say(vec![error]);
            return;
        }
        say(vec!["登录成了，正在配 provider…".into()]);
        wizard.show(None);
        painted();
    } else {
        say(vec!["已登录，正在刷新 codingplan 配置…".into()]);
    }

    // Phase 2: claim/models/status。
    let (mut prepared_config, mut report) = match (world.setup)((world.load)()) {
        Ok(pair) => pair,
        Err(error) => {
            say(vec![format!("internal error：{error}")]);
            return;
        }
    };

    // Phase 3: auth_expired —— 本地 token 还没过期但服务端拒了（被吊销、
    // refresh 死了）。再走一次 OAuth，重跑 setup。只有一次，不循环。
    if report.auth_expired {
        say(vec!["服务端不认这个 token 了，重新登录一次…".into()]);
        if let Err(error) = (world.login)(wizard, painted) {
            say(vec![error]);
            return;
        }
        say(vec!["重新登录成了，重跑 setup…".into()]);
        wizard.show(None);
        painted();
        match (world.setup)(prepared_config.clone()) {
            Ok((cfg, r)) => {
                prepared_config = cfg;
                report = r;
            }
            Err(error) => {
                say(vec![format!("internal error：{error}")]);
                return;
            }
        }
    }

    // Phase 4: 配置要落盘才写盘 —— 半途而废的 setup 不写。
    if report.should_persist_config() {
        if let Err(error) = (world.save)(&prepared_config, &report) {
            say(vec![format!("配置没写成：{error}")]);
            return;
        }
    }

    // The configuration is on disk, but the running graph was assembled before
    // it existed — the same "wrote the file is not making it so" gap
    // `tui_onboarding` closes with a reload. The session stays.
    (world.reload)(wizard, painted);

    // Said into the step, not resolved: the step is a note, so the person
    // reads the report and presses enter — `resolve` only closes a *waiting*
    // step, and calling it here would be a close that never happens.
    say(vec![report.render()]);
}

/// One OAuth round: URL + QR, poll to the end, exchange and save.
///
/// The same road `tui_onboarding::sign_in` walks: a failure is the words, an
/// `Ok` is credentials on disk.
fn run_oauth(
    wizard: &Arc<Wizard>,
    telemetry: &Option<Arc<atomcode_telemetry::Telemetry>>,
    painted: &dyn Fn(),
) -> Result<(), String> {
    let session =
        atomcode_auth::oauth::start_login().map_err(|error| format!("登录起不来：{error:#}"))?;
    let url = session.url().to_string();
    wizard.say(vec!["用手机扫码，或在浏览器里打开：".into(), url.clone()]);
    wizard.show(atomcode_tui::qr::code(&url));
    painted();

    let ticks = session.spawn_poller(Duration::from_secs(2));
    loop {
        match ticks.recv() {
            Ok(Ok(atomcode_auth::oauth::PollOutcome::Authorized)) => break,
            Ok(Ok(atomcode_auth::oauth::PollOutcome::Pending)) => continue,
            Ok(Err(error)) => return Err(format!("登录没成：{error:#}")),
            Err(_) => return Err("登录没了回音".into()),
        }
    }

    session
        .finish(telemetry.as_ref())
        .map(|_| ())
        .map_err(|error| format!("登录成了，但凭据没写成：{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A world that records what was touched and never reaches a server.
    struct Recording {
        logged_in: bool,
        auth_expired: bool,
        logins: AtomicUsize,
        setups: AtomicUsize,
        saves: AtomicUsize,
        reloads: AtomicUsize,
    }

    impl Recording {
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

        /// The same sequence the flow walks, with every step recorded.
        fn world(self: &Arc<Self>) -> World {
            let me = self.clone();
            let login = {
                let me = me.clone();
                Arc::new(move |_: &Arc<Wizard>, _: &dyn Fn()| {
                    me.logins.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            };
            let setup = {
                let me = me.clone();
                Arc::new(move |config: atomcode_config::config::Config| {
                    me.setups.fetch_add(1, Ordering::SeqCst);
                    let report = report_with(me.auth_expired);
                    Ok((config, report))
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
                Arc::new(move |_: &Arc<Wizard>, _: &dyn Fn()| {
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

    fn overlay() -> Arc<Wizard> {
        Wizard::new(
            MODAL,
            "登录",
            vec![
                StepDef::new("flow", "登录", StepKind::Note).saying(vec!["正在看登录状态…".into()])
            ],
            Box::new(|_| {}),
            FINISHED,
        )
    }

    /// **The bug this command had.** `/logout` then `/login` said "AtomGit
    /// gateway requires login — run `/login` first": the shipped `/login` only
    /// re-read configuration, so it never asked anyone to sign in. Not signed
    /// in is exactly when the prompt must appear.
    #[test]
    fn not_being_signed_in_is_what_opens_the_login() {
        let world = Recording::new(false, false);
        let wizard = overlay();

        run_login_flow_with(&world.world(), &wizard, &|| {});

        assert_eq!(
            world.logins.load(Ordering::SeqCst),
            1,
            "signed out, the flow signs in"
        );
    }

    /// **The other half: signed in, don't ask again.** Running the login again
    /// on a machine that is already signed in makes the person redo what they
    /// have done; what they wanted was the entitlement refresh below it.
    #[test]
    fn being_signed_in_skips_the_login_and_still_runs_setup() {
        let world = Recording::new(true, false);
        let wizard = overlay();

        run_login_flow_with(&world.world(), &wizard, &|| {});

        assert_eq!(
            world.logins.load(Ordering::SeqCst),
            0,
            "already signed in, no second prompt"
        );
        assert_eq!(
            world.setups.load(Ordering::SeqCst),
            1,
            "and the entitlement refresh still runs — that is what /login is for here"
        );
    }

    /// A server that disowned a locally-unexpired token: one more OAuth, one
    /// more setup. Exactly one, not a loop.
    #[test]
    fn a_disowned_token_is_replaced_once_and_setup_rerun() {
        let world = Recording::new(true, true);
        let wizard = overlay();

        run_login_flow_with(&world.world(), &wizard, &|| {});

        assert_eq!(
            world.logins.load(Ordering::SeqCst),
            1,
            "signed in but rejected, so one re-login"
        );
        assert_eq!(
            world.setups.load(Ordering::SeqCst),
            2,
            "and setup runs again against the fresh token"
        );
    }

    /// The configuration is written only when setup got through, and the reload
    /// follows the write — never the other way round.
    #[test]
    fn the_write_and_the_reload_wait_for_setup_to_get_through() {
        let world = Recording::new(true, false);
        let wizard = overlay();
        let saved_before_reload = Arc::new(AtomicBool::new(false));

        let mut w = world.world();
        let inner_save = w.save.clone();
        let flag = saved_before_reload.clone();
        w.save = Arc::new(move |config, report| {
            flag.store(true, Ordering::SeqCst);
            inner_save(config, report)
        });
        let inner_reload = w.reload.clone();
        let flag = saved_before_reload.clone();
        w.reload = Arc::new(move |wizard, painted| {
            assert!(
                flag.load(Ordering::SeqCst),
                "the reload happens after the write, not instead of it"
            );
            inner_reload(wizard, painted)
        });

        run_login_flow_with(&w, &wizard, &|| {});

        assert_eq!(world.saves.load(Ordering::SeqCst), 1);
        assert_eq!(world.reloads.load(Ordering::SeqCst), 1);
    }
}
