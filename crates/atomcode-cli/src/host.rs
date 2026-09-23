//! Host control for the coding runtime: the adapter behind [`HostControl`].
//!
//! `docs/adr/0021` §5 calls this "the host-side adapter that translates the two
//! contracts onto `CodingRuntimeHandle`", and §2.4 of
//! `docs/architecture-target.md` puts a host's own wiring in the Host layer —
//! which today is this binary. It lived in `atomcode-coding` until 2026-09-18,
//! and that is what made a Product crate depend on a front-end contract: the
//! wrong direction, and `atomcode-coding` is in the `legacy/` group that only
//! shrinks.
//!
//! Nothing here is reachable from a front end. A front end sees
//! [`atomcode_host_api::HostConnection`] and never learns whose runtime is
//! behind it.

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_harness::feed::Feed;
use atomcode_host_api::{
    HostCommand, HostConnection, HostControl, HostError, HostEvent, HostReply, StoredSession,
};
use atomcode_kernel::event::{AgentCommand, AgentEvent, CommandError, CommandId};
use tokio::sync::mpsc;

use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::runtime::{
    CodingRuntime, CodingRuntimeEvent, CodingRuntimeHandle, CompactionCompletion,
    ProviderUnavailableReason, RuntimeError, TurnCompletion, UserInput,
};
use atomcode_coding::CodingAgentConfig;

/// What a host knows about configuration that a front end asks it to act on:
/// the provider settings a model id means, and the settings as they are now
/// (`docs/adr/0021`, M5.4 addendum). The front end names the intent; the host
/// reads its own configuration.

pub trait HostConfig: Send + Sync {
    /// The agent configuration for `model`, as the host's configuration
    /// resolves that id now.
    fn for_model(&self, model: &str) -> Result<CodingAgentConfig, String>;
    /// The agent configuration as configured now — what signing in again reads.
    fn current(&self) -> Result<CodingAgentConfig, String>;

    /// What the configuration *is* right now, as a value that changes when it
    /// changes — a hash of the file, a modification time, a revision.
    ///
    /// A reload asks this before it does anything: with the configuration where
    /// it was, a reload is a re-read of what is on disk beside it (skills), and
    /// the session keeps running in the tree it is already in
    /// (`docs/adr/0022` §2). With the configuration changed, the whole graph is
    /// built again from it — permission rules, hooks and tools are all decided
    /// by it, and patching them one at a time is not a thing this host claims.
    ///
    /// `None` is "cannot tell", and a reload then rebuilds: a host that cannot
    /// say whether its configuration moved should not have a session assume it
    /// did not.
    fn fingerprint(&self) -> Option<String> {
        None
    }

    /// The settings a person may change, with what each is set to now
    /// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A3).
    ///
    /// The host's file, not the running graph: a screen that patched rows could
    /// turn approval off under a running turn, which is what 0022 §7 removed.
    /// Empty for a host that keeps no such file.
    fn settings(&self) -> Vec<atomcode_host_api::Setting> {
        Vec::new()
    }

    /// Set one, by the id [`Self::settings`] gave it. `Err` says why not — an
    /// id nobody offers, or a value the setting does not accept.
    fn set_setting(&self, id: &str, value: &str) -> Result<(), String> {
        let _ = (id, value);
        Err(tr(SMsg::HostConfigNotEditable).into_owned())
    }

    /// Put one back to what this build does when nobody has said.
    ///
    /// Removing the key, not writing today's default into it: a setting that
    /// was reset follows the build from then on, and one written with the
    /// default's current value stops following. Only the first is what a person
    /// asking for "default" means.
    fn reset_setting(&self, id: &str) -> Result<(), String> {
        let _ = id;
        Err(tr(SMsg::HostConfigNotEditable).into_owned())
    }

    /// The providers this host is configured with, for a person to pick between.
    ///
    /// Never a credential: see [`atomcode_host_api::ProviderChoice`]. Empty
    /// for a host that has no configuration file to read them from.
    fn providers(&self) -> Vec<atomcode_host_api::ProviderChoice> {
        Vec::new()
    }

    /// Make `model` the persisted default, so a `/model <id>` switch survives
    /// the next start instead of being a runtime-only pin.
    ///
    /// The default is a no-op: a host with no file to write keeps the switch
    /// live-only, which is the outcome those hosts already had. A host that
    /// does keep a file overrides this to write the selection (`ConfigFile`).
    fn set_default_model(&self, model: &str) -> Result<(), String> {
        let _ = model;
        Ok(())
    }

    /// Who is signed in, when anybody is.
    ///
    /// `None` is the ordinary answer for a host that runs on a key in a file:
    /// nobody signed in, nothing wrong. What comes back is what a person would
    /// put on a name badge — never a token, never a key, not even truncated
    /// (`docs/adr/0021`; a credential does not belong in an answer a screen
    /// prints and a log keeps).
    fn identity(&self) -> Option<Identity> {
        None
    }
}

/// Who is signed in, for [`HostConfig::identity`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The name to show. A username, an email, whatever the host signs in with.
    pub who: String,
    /// Anything worth showing beside it — an email, an organisation.
    pub detail: Option<String>,
}

/// Connect a front end to a runtime started with `front_end` in its prepare
/// options.
///
/// Takes the runtime whole: its event stream has one reader, and from here on
/// that reader is this connection.
pub fn connect(
    runtime: CodingRuntime,
    front_end: Arc<FrontEnd>,
    config: CodingAgentConfig,
    host_config: Option<Arc<dyn HostConfig>>,
) -> Result<HostConnection, String> {
    let events = front_end
        .take_receiver()
        .ok_or("this front end is already connected")?;
    let CodingRuntime {
        handle,
        events: mut runtime_events,
        task,
        session,
    } = runtime;
    let control = Arc::new(RuntimeControl {
        handle: handle.clone(),
        session: Mutex::new(session.map(|s| s.id).unwrap_or_default()),
        config: Mutex::new(config),
        fingerprint: Mutex::new(host_config.as_ref().and_then(|source| source.fingerprint())),
        watchers: Mutex::new(Vec::new()),
        front_end: front_end.clone(),
        host_config,
    });

    // Runtime events, as the handle protocol says them.
    let out = front_end.events();
    let watched = control.clone();
    tokio::spawn(async move {
        let _task = task;
        while let Some(sequenced) = runtime_events.recv().await {
            // 共享着的话,网页端照这条事件画。挂没挂在那一侧判断,这里不设第二个
            // 开关——两个「现在共享着吗」的答案迟早会不一致。
            crate::tui_share::publish(&sequenced);
            // 模式换了也要单独说一声,理由同 attach:远端的徽标读的是那个全局值。
            if let CodingRuntimeEvent::ModeChanged { mode } = &sequenced.event {
                crate::tui_share::mode_changed(*mode);
            }
            match sequenced.event {
                CodingRuntimeEvent::RuntimeStopped(_) => break,
                CodingRuntimeEvent::SessionChanged(changed) => {
                    let Some(session) = changed.session_id else {
                        continue;
                    };
                    let previous = std::mem::replace(
                        &mut *watched.session.lock().expect("session poisoned"),
                        session.clone(),
                    );
                    if previous != session {
                        watched.announce(HostEvent::SessionChanged {
                            session,
                            previous: (!previous.is_empty()).then_some(previous),
                        });
                    }
                }
                // Pushed, not polled. A status line that has to ask cannot
                // show a round counter moving, and a screen that polled would
                // be asking a busy runtime a question it already knows the
                // answer to. The payload is the one `Autonomy` answers with, so
                // the line and the command cannot disagree.
                CodingRuntimeEvent::GoalChanged(progress) => {
                    let session = watched.session.lock().expect("session poisoned").clone();
                    let ended = progress.terminal.is_some();
                    watched.announce(HostEvent::Autonomy {
                        session,
                        running: (!ended).then(|| running_of_goal(progress)),
                    });
                }
                CodingRuntimeEvent::LoopChanged(progress) => {
                    let session = watched.session.lock().expect("session poisoned").clone();
                    let stopped = !progress.active;
                    watched.announce(HostEvent::Autonomy {
                        session,
                        running: (!stopped).then(|| running_of_loop(progress)),
                    });
                }
                // The turn finished and its record did not. Both are said:
                // `TurnComplete` because the turn did finish, and this because
                // the log — the session's only authority — did not get it.
                // Before this, only ACP heard about it (as an internal error)
                // and the screen heard nothing at all.
                CodingRuntimeEvent::TurnFinished(completion) => {
                    if let Some(message) = persistence_failure(&completion) {
                        let session = watched.session.lock().expect("session poisoned").clone();
                        watched.announce(HostEvent::PersistenceFailed { session, message });
                    }
                    // The turn's own completion still goes through the one
                    // mapping, because the turn did complete.
                    if let Some(event) = translate(CodingRuntimeEvent::TurnFinished(completion)) {
                        if out.send(event).is_err() {
                            break;
                        }
                    }
                }
                CodingRuntimeEvent::ProviderChanged { .. }
                | CodingRuntimeEvent::ReasoningEffortChanged { .. } => {
                    if let Some(app) = watched.front_end.app() {
                        watched.front_end.feed().redescribe(&app);
                    }
                }
                // How much the session may do without asking is session state,
                // not a fact in the log — so a screen draws it from here or not
                // at all. Pushed rather than polled, like the two above: the
                // mode moves under the person's hands when their own Shift+Tab
                // (or `/mode`) asks for it, and a screen that had to ask back
                // would be one frame behind its own keystroke.
                CodingRuntimeEvent::ModeChanged { mode } => {
                    let session = watched.session.lock().expect("session poisoned").clone();
                    watched.announce(HostEvent::ModeChanged {
                        session,
                        mode: host_mode(mode),
                    });
                }
                other => {
                    if let Some(event) = translate(other) {
                        if out.send(event).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    // Commands, as runtime calls.
    let (commands, mut command_rx) = mpsc::unbounded_channel::<AgentCommand>();
    let out = front_end.events();
    let front = front_end.clone();
    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            let (receipt, command) = match command {
                AgentCommand::Tagged { id, command } => (Some(id), command.untagged()),
                AgentCommand::Invoke { ref id, .. } => (Some(id.clone()), command),
                other => (None, other),
            };
            // A team member's, not the runtime's: reached in the App, the way
            // the harness's own pump reaches it (`docs/adr/0023` §4, §8).
            let command = match command {
                AgentCommand::To { session, command } => {
                    let (receipt, inner) = match *command {
                        AgentCommand::Tagged { id, command } => (Some(id), command.untagged()),
                        other => (receipt, other.untagged()),
                    };
                    let answered = match front.app() {
                        None => Err((CommandError::Unavailable, None)),
                        Some(app) => atomcode_harness::plugins::handle::command_member(
                            &app,
                            &session,
                            inner,
                            receipt.as_ref(),
                            &front.forwarded(),
                            &out,
                        )
                        .map_err(|error| (error, None)),
                    };
                    match answered {
                        // Its receipt comes when the member takes it.
                        Ok(None) => {}
                        Ok(Some(turn)) => reply(&out, receipt, Ok(turn)),
                        Err(refused) => reply(&out, receipt, Err(refused)),
                    }
                    continue;
                }
                AgentCommand::Invoke {
                    id,
                    session,
                    name,
                    args,
                } => {
                    let found = front
                        .app()
                        .ok_or(CommandError::Unavailable)
                        .and_then(|app| {
                            atomcode_harness::plugins::handle::catalog_command(
                                &app, &session, &name,
                            )
                        });
                    match found {
                        Ok((target, command)) => {
                            reply(&out, receipt, Ok(None));
                            atomcode_harness::plugins::handle::run_catalog_command(
                                target,
                                command,
                                id,
                                args,
                                out.clone(),
                            );
                        }
                        Err(error) => reply(&out, receipt, Err((error, None))),
                    }
                    continue;
                }
                other => other,
            };
            let stop = matches!(command, AgentCommand::Shutdown);
            let answered = run(&handle, &front, &out, command).await;
            reply(&out, receipt, answered);
            if stop {
                break;
            }
        }
    });

    let session = control.session.lock().expect("session poisoned").clone();
    // 共享(`/webui` / `/sync` / `/app`)要的是这个 runtime 的句柄,而句柄不在
    // 宿主契约里——它是产品的东西。接上时交给那一层,它自己判断现在共享没有。
    crate::tui_share::remember(control.clone());

    Ok(HostConnection {
        session,
        commands,
        events,
        control,
    })
}

impl crate::tui_share::Live for RuntimeControl {
    fn handle(&self) -> atomcode_coding::CodingRuntimeHandle {
        self.handle.clone()
    }
    fn session(&self) -> String {
        self.session.lock().expect("session poisoned").clone()
    }
    fn working_dir(&self) -> std::path::PathBuf {
        self.config
            .lock()
            .expect("config poisoned")
            .working_dir
            .clone()
    }
}

/// What happened to a command: taken, or refused and why.
type Answered = Result<Option<u64>, (CommandError, Option<String>)>;

fn reply(out: &mpsc::UnboundedSender<AgentEvent>, receipt: Option<CommandId>, answered: Answered) {
    match (receipt, answered) {
        (Some(command), Ok(turn)) => {
            let _ = out.send(AgentEvent::Accepted {
                command,
                turn,
                steered: false,
            });
        }
        (Some(command), Err((error, _))) => {
            let _ = out.send(AgentEvent::Rejected { command, error });
        }
        // Nobody asked for a receipt, so a failure a person should see is said
        // the way every other failure is. **Every** failure: a command refused
        // without a word is a refusal the front end cannot read, and a front
        // end that wrote a provisional state on the strength of having asked
        // is left holding it. That is the shape of the stop button that looked
        // pressed and did nothing — `esc` sends a bare `Cancel` (no receipt),
        // the screen says 正在停止, and only an *event* takes that back. When
        // an arm has no specific reason, say what the error class was rather
        // than nothing at all.
        (None, Err((error, message))) => {
            let _ = out.send(AgentEvent::Error {
                message: message.unwrap_or_else(|| refused_words(&error)),
                http_status: None,
                code: None,
                retryable: None,
            });
        }
        (None, Ok(_)) => {}
    }
}

/// A refusal in words, for a front end that was sent no receipt and so has only
/// this channel to hear it on.
fn refused_words(error: &CommandError) -> String {
    match error {
        CommandError::StaleQuestion => "that question is no longer waiting for an answer".into(),
        CommandError::NotRunning => "nothing is running to act on".into(),
        CommandError::Unavailable => "the agent cannot take commands now".into(),
        CommandError::Busy { reason } => reason.clone(),
        CommandError::NotFound => "no session by that id".into(),
        CommandError::Unsupported => "that command is not supported here".into(),
        // `CommandError` is `non_exhaustive`: a refusal added later still has
        // to reach the person as *something*.
        _ => "the command was refused".into(),
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::{refused_words, reply};
    use atomcode_kernel::event::{AgentEvent, CommandError};

    /// A refusal is said even when the command carried no receipt.
    ///
    /// `esc` sends a bare `Cancel` — no receipt — and the screen has already
    /// written 正在停止 by the time the runtime answers. Only an *event* takes
    /// that claim back, so a refusal that produced none left the stop button
    /// looking pressed while the turn ran on: the reported "按 esc 没反应,
    /// 一直显示正在停止". The words matter as much as the event: they are how a
    /// person (and whoever reads the report) learns *why* the stop was refused.
    #[test]
    fn a_refusal_without_a_receipt_is_still_said() {
        let (out, mut events) = tokio::sync::mpsc::unbounded_channel();
        reply(&out, None, Err((CommandError::Unavailable, None)));
        let event = events
            .try_recv()
            .expect("a refusal nobody holds a receipt for must not be swallowed");
        assert!(
            matches!(event, AgentEvent::Error { ref message, .. }
                if message == &refused_words(&CommandError::Unavailable)),
            "{event:?}"
        );
    }

    /// The negative control: a command that WAS taken says nothing.
    ///
    /// Otherwise every keystroke that reached the runtime would paint an error,
    /// and the event above would prove nothing about refusals in particular.
    #[test]
    fn a_command_that_was_taken_says_nothing() {
        let (out, mut events) = tokio::sync::mpsc::unbounded_channel();
        reply(&out, None, Ok(None));
        assert!(events.try_recv().is_err(), "nothing happened to report");
    }

    /// A receipt still comes back as a receipt, not as an error line: a driver
    /// that asked for an id is matching on it.
    #[test]
    fn a_refusal_with_a_receipt_is_answered_with_the_receipt() {
        let (out, mut events) = tokio::sync::mpsc::unbounded_channel();
        reply(
            &out,
            Some("c-1".into()),
            Err((
                CommandError::Busy {
                    reason: "working".into(),
                },
                None,
            )),
        );
        let event = events.try_recv().expect("the receipt is owed an answer");
        assert!(
            matches!(event, AgentEvent::Rejected { ref command, .. } if command == "c-1"),
            "{event:?}"
        );
    }
}

async fn run(
    handle: &CodingRuntimeHandle,
    front: &FrontEnd,
    out: &mpsc::UnboundedSender<AgentEvent>,
    command: AgentCommand,
) -> Answered {
    let refused = |error: RuntimeError| {
        let message = error.to_string();
        (command_error(error), Some(message))
    };
    match command {
        AgentCommand::SendMessage { text, images } => {
            let input = UserInput { text, images };
            // 终端里打的这句,网页端是从 hub 的「输入被接受」看到的——不回显,
            // 那边就只见回答不见问题。提交成功才回显:没送出去的话不该出现在
            // 别人的屏幕上。
            let sent = handle.submit(input.clone()).await;
            if sent.is_ok() {
                crate::tui_share::echo_local_input(&input);
            }
            sent.map(|_| None).map_err(refused)
        }
        AgentCommand::SendMessageWithContext {
            text,
            images,
            context,
        } => {
            handle
                .queue_local_context(atomcode_coding::runtime::LocalContextInput {
                    content: context,
                })
                .await
                .map_err(refused)?;
            handle
                .submit(UserInput { text, images })
                .await
                .map(|_| None)
                .map_err(refused)
        }
        AgentCommand::Respond { id, value } => handle
            .respond(id, value)
            .await
            .map(|_| None)
            .map_err(|error| (command_error(error), None)),
        // A stop the runtime could not take is said out loud: the screen has
        // already written 正在停止 by the time this comes back, and an event is
        // the only thing that can take that back. `refused` carries the
        // runtime's own words (unavailable / delivery failed / …) instead of
        // collapsing them to a bare class name.
        AgentCommand::Cancel => handle.cancel().await.map(|_| None).map_err(refused),
        AgentCommand::Compact { focus } => handle.compact(focus).map(|_| None).map_err(|_| {
            (
                CommandError::Unavailable,
                Some("compaction is unavailable right now".into()),
            )
        }),
        AgentCommand::Snapshot => {
            let snapshot = handle.snapshot().await.map_err(refused)?;
            let _ = out.send(AgentEvent::Snapshot {
                snapshot: (*snapshot).clone(),
            });
            Ok(None)
        }
        AgentCommand::Subscribe { session, from } => {
            let Some(app) = front.app() else {
                return Err((CommandError::Unavailable, None));
            };
            match front.feed().subscribe_to(&app, &session, from) {
                Ok(()) => Ok(None),
                // A member that is gone, from its kept log.
                Err(CommandError::NotFound) => front
                    .feed()
                    .replay_kept(&app, &session, from)
                    .await
                    .map(|_| None)
                    .map_err(|error| (error, None)),
                Err(error) => Err((error, None)),
            }
        }
        AgentCommand::Unsubscribe { session } => {
            front.feed().unsubscribe(&session);
            Ok(None)
        }
        AgentCommand::Shutdown => {
            let _ = handle.shutdown().await;
            Ok(None)
        }
        _ => Err((CommandError::Unsupported, None)),
    }
}

/// A runtime event in the handle protocol's words, when the front end is owed
/// one. Session changes and descriptions are handled by the caller.
/// Why the turn's record could not be kept, when it could not be.
///
/// A turn that finished and a turn whose record was written are two different
/// claims, and this is the second one. `None` is the ordinary case.
fn persistence_failure(completion: &TurnCompletion) -> Option<String> {
    match completion {
        TurnCompletion::Completed { .. } => None,
        TurnCompletion::SnapshotUnavailable { error, .. } => Some(error.message.clone()),
    }
}

/// What the staleness guard can conclude when there is no App to read a log
/// from (`RuntimeControl::fresh`).
///
/// Two situations look identical from the call site and must not be answered
/// the same way. A front end that **has** fed an App and is not feeding one now
/// is between Apps: its caller's position cannot be checked, and letting the
/// command through would assume exactly what the guard exists to verify. A
/// front end that has **never** fed one keeps no log at all — a protocol server
/// with no screen — so there is nothing for a caller to be stale against, which
/// is the same situation as a session the feed has never seen and is allowed
/// for the same reason.
///
/// Before this was distinguished, such a front end could never undo anything.
fn without_a_log(apps_fed: u64) -> Result<(), HostError> {
    if apps_fed == 0 {
        Ok(())
    } else {
        Err(HostError::Unavailable)
    }
}

/// A goal, as the contract says a self-driving session.
fn running_of_goal(goal: atomcode_coding::GoalProgress) -> atomcode_host_api::Running {
    atomcode_host_api::Running {
        kind: "goal".into(),
        what: goal.condition,
        round: goal.round,
        of: goal.max_rounds,
        elapsed_secs: goal.elapsed_secs,
        paused: (!goal.active).then(|| format!("{:?}", goal.phase)),
    }
}

/// A loop, the same way. No `of`: a loop repeats until it is stopped.
fn running_of_loop(looping: atomcode_coding::LoopProgress) -> atomcode_host_api::Running {
    atomcode_host_api::Running {
        kind: "loop".into(),
        what: looping.label,
        round: looping.round,
        of: None,
        elapsed_secs: looping.elapsed_secs,
        paused: (!looping.active).then(|| tr(SMsg::HostHeld).into_owned()),
    }
}

/// The mode a *person* means, for the one this runtime calls it.
///
/// `Ask` is `Build`: the contract names what a person chooses and the runtime
/// names what it does. One function for both directions — who a mode reaches
/// the runtime and what the runtime reports back — because two tables would be
/// two chances to map one of the four onto the wrong side.
fn host_mode(mode: atomcode_coding::RuntimeMode) -> atomcode_host_api::Mode {
    use atomcode_host_api::Mode;
    match mode {
        atomcode_coding::RuntimeMode::Plan => Mode::Plan,
        atomcode_coding::RuntimeMode::Build => Mode::Ask,
        atomcode_coding::RuntimeMode::AcceptEdits => Mode::AcceptEdits,
        atomcode_coding::RuntimeMode::Auto => Mode::Auto,
    }
}

/// The same table the other way round, for [`HostCommand::SetMode`].
fn runtime_mode(mode: atomcode_host_api::Mode) -> atomcode_coding::RuntimeMode {
    use atomcode_coding::RuntimeMode;
    match mode {
        atomcode_host_api::Mode::Plan => RuntimeMode::Plan,
        atomcode_host_api::Mode::Ask => RuntimeMode::Build,
        atomcode_host_api::Mode::AcceptEdits => RuntimeMode::AcceptEdits,
        atomcode_host_api::Mode::Auto => RuntimeMode::Auto,
    }
}

fn translate(event: CodingRuntimeEvent) -> Option<AgentEvent> {
    match event {
        CodingRuntimeEvent::Agent(event) => Some(event),
        CodingRuntimeEvent::Request(request) => Some(AgentEvent::Request {
            id: request.id,
            kind: request.kind,
            payload: request.payload,
        }),
        CodingRuntimeEvent::TurnFinished(completion) => {
            let reason = match completion {
                TurnCompletion::Completed { reason, .. }
                | TurnCompletion::SnapshotUnavailable { reason, .. } => reason,
            };
            Some(AgentEvent::TurnComplete { turn: None, reason })
        }
        CodingRuntimeEvent::CompactionStarted { trigger } => {
            Some(AgentEvent::CompactionStarted { trigger })
        }
        CodingRuntimeEvent::CompactionFinished { completion } => match completion {
            CompactionCompletion::Completed(outcome) => Some(AgentEvent::Compacted {
                trigger: outcome.trigger,
                epoch: outcome.epoch,
                removed: outcome.removed_messages,
                bytes_before: outcome.bytes_before,
                bytes_after: outcome.bytes_after,
                committed: outcome.committed,
                snapshot: None,
            }),
            CompactionCompletion::Failed { trigger, error } => {
                Some(AgentEvent::CompactionFailed { trigger, error })
            }
            CompactionCompletion::Interrupted { .. } => None,
        },
        CodingRuntimeEvent::ControllerWarning(message)
        | CodingRuntimeEvent::PersistenceWarning(message) => Some(AgentEvent::Error {
            message,
            http_status: None,
            code: None,
            retryable: None,
        }),
        _ => None,
    }
}

/// Why the runtime refused a handle-protocol command, in that protocol's words.
fn command_error(error: RuntimeError) -> CommandError {
    match error {
        RuntimeError::Busy => CommandError::Busy {
            reason: "the runtime is busy".into(),
        },
        RuntimeError::StaleRequest { .. } => CommandError::StaleQuestion,
        _ => CommandError::Unavailable,
    }
}

/// Host control over a runtime.
struct RuntimeControl {
    handle: CodingRuntimeHandle,
    /// The live session, as the runtime last said. Empty for a runtime with none.
    session: Mutex<String>,
    /// What the runtime runs, for a change that is expressed as a new
    /// configuration — the thinking level.
    config: Mutex<CodingAgentConfig>,
    /// The host configuration this graph was built from, by
    /// [`HostConfig::fingerprint`]. `None` for a host that has no configuration
    /// source or cannot say.
    fingerprint: Mutex<Option<String>>,
    watchers: Mutex<Vec<mpsc::UnboundedSender<HostEvent>>>,
    front_end: Arc<FrontEnd>,
    /// How this host resolves configuration, when it can.
    ///
    /// Held here rather than stashed on the front end and read back: the front
    /// end was carrying it for this adapter's benefit, which is what kept a
    /// Product crate holding a front-end contract's type.
    host_config: Option<Arc<dyn HostConfig>>,
}

impl RuntimeControl {
    fn addressed(&self, session: &str) -> Result<(), HostError> {
        if *self.session.lock().expect("session poisoned") == session {
            Ok(())
        } else {
            Err(HostError::NotFound)
        }
    }

    /// Refuse a command based on a fact older than a message or a turn the
    /// session has had since (`docs/adr/0021` §9).
    ///
    /// The guard compares against the front end's own log, so it can only be
    /// applied to a front end that keeps one. Two different things look the
    /// same from here and must not be answered the same way:
    ///
    /// - **Not fed right now**, but it has been (`apps_fed() > 0`): the host is
    ///   between Apps. A caller's position cannot be checked, and letting the
    ///   command through would be assuming what this guard exists to verify.
    ///   Refused.
    /// - **Never fed at all**: this front end keeps no log — a protocol server
    ///   driving sessions with no screen, say. There is nothing for the caller
    ///   to be stale against, which is the same situation as a session this
    ///   feed has never seen, two lines below, and it is allowed for the same
    ///   reason. Refusing instead would mean such a front end could never undo
    ///   anything, which is what it did before this was distinguished.
    fn fresh(
        &self,
        session: &str,
        based_on: atomcode_kernel::session::SeqNo,
    ) -> Result<(), HostError> {
        let Some(app) = self.front_end.app() else {
            return without_a_log(self.front_end.apps_fed());
        };
        let Some(agent) = Feed::find(&app, session) else {
            return Ok(());
        };
        let events = agent.session().events();
        let moved = events.iter().any(|logged| {
            logged.seq > based_on
                && matches!(
                    logged.event,
                    atomcode_harness::session::SessionEvent::TurnStart { .. }
                        | atomcode_harness::session::SessionEvent::UserMessage { .. }
                )
        });
        if moved {
            return Err(HostError::Stale {
                current: events.last().map(|logged| logged.seq).unwrap_or(0),
            });
        }
        Ok(())
    }

    /// Put `next` in place of the configuration the runtime runs, and keep it.
    async fn reconfigure(&self, next: CodingAgentConfig) -> Result<HostReply, HostError> {
        self.handle
            .reassemble_provider(next.clone())
            .await
            .map_err(refused)?;
        *self.config.lock().expect("config poisoned") = next;
        Ok(HostReply::Done)
    }

    fn announce(&self, event: HostEvent) {
        self.watchers
            .lock()
            .expect("watchers poisoned")
            .retain(|watcher| watcher.send(event.clone()).is_ok());
    }

    /// Take a new live session the runtime reports, and say so.
    fn changed(&self, session: Option<String>) -> Result<HostReply, HostError> {
        let session = session.ok_or(HostError::Failed {
            message: "the runtime has no session to report".into(),
        })?;
        let previous = std::mem::replace(
            &mut *self.session.lock().expect("session poisoned"),
            session.clone(),
        );
        if previous != session {
            self.announce(HostEvent::SessionChanged {
                session: session.clone(),
                previous: (!previous.is_empty()).then_some(previous),
            });
        }
        Ok(HostReply::SessionChanged { session })
    }

    /// Throw a stored session away.
    ///
    /// **The lease is what refuses the live one**, and it is the only guard
    /// here that is worth having: a session being had — by this runtime or by
    /// another — is one whose lease cannot be acquired, so it comes back
    /// `SessionInUse` without this function knowing which runtime holds it. An
    /// explicit "is it mine?" check in front of it was written first and taken
    /// back out: removing it changed no answer (`a_stored_session_is_deleted_
    /// but_never_the_live_one` stayed green), which is the definition of code
    /// no criterion can see.
    ///
    /// Everything delegated from it goes with it: the store's own `delete`
    /// takes the children first, because nothing would ever offer those
    /// sessions again once the parent is gone.
    fn delete_stored(&self, session: &str) -> Result<(), HostError> {
        use atomcode_capabilities::session::SessionManager;
        // Which project's store it is in: sessions are kept per working
        // directory, and the one being deleted is usually not this session's.
        let scan = SessionManager::scan_all();
        let entry = scan
            .entries
            .into_iter()
            .find(|entry| entry.id == session)
            .ok_or(HostError::NotFound)?;
        let manager = SessionManager::for_project(&entry.working_dir);
        // The lease is the other runtime's answer to "is anyone using this?":
        // one held elsewhere fails here rather than deleting under it.
        let lease = manager
            .acquire_lease(session)
            .map_err(|_| HostError::SessionInUse {
                id: session.to_string(),
            })?;
        manager.delete(&lease).map_err(|error| HostError::Failed {
            message: error.to_string(),
        })
    }

    fn list(&self, working_dir: Option<String>) -> Vec<StoredSession> {
        use atomcode_capabilities::session::SessionManager;
        let scan = SessionManager::scan_all();
        let mut sessions: Vec<StoredSession> = scan
            .entries
            .into_iter()
            .filter(|entry| entry.message_count > 0)
            .map(|entry| StoredSession {
                id: entry.id,
                title: (!entry.name.is_empty()).then_some(entry.name),
                working_dir: Some(entry.working_dir.display().to_string()),
                created_at: u64::try_from(entry.created_at_ms).unwrap_or(0),
                updated_at: u64::try_from(entry.updated_at_ms).unwrap_or(0),
                turns: u32::try_from(entry.turn_count).unwrap_or(u32::MAX),
                needs_newer_version: entry.needs_newer_version,
            })
            .filter(|stored| working_dir.is_none() || stored.working_dir == working_dir)
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        sessions
    }
}

/// The last few exchanges of a stored session, for someone deciding whether to
/// come back to it.
///
/// Read from the log the session already keeps — the same door `/worklog` and
/// recall read it through — rather than by opening the session: looking at a
/// conversation must not start it.
///
/// **What a row cannot say is what it was about.** The list already gives the
/// name, the age and the turn count; the words are what tell a person whether
/// this is the one. Both halves of each exchange, oldest of the shown ones
/// first, so it reads the way the conversation ran.
///
/// Undone turns are left out: they were taken back, and a preview that shows
/// them describes a conversation that no longer exists.
fn preview_of(session: &str) -> Result<Vec<String>, HostError> {
    use atomcode_capabilities::session::{events, SessionManager};
    /// How many exchanges. Enough to recognise a conversation, few enough to
    /// read at a glance while walking the list.
    const TURNS: usize = 3;
    /// One line each: a preview that wraps is a preview that hides the row
    /// under it.
    const WIDE: usize = 160;

    let scan = SessionManager::scan_all();
    let entry = scan
        .entries
        .into_iter()
        .find(|entry| entry.id == session)
        .ok_or(HostError::NotFound)?;
    let manager = SessionManager::for_project(&entry.working_dir);
    let logged = manager
        .load_events(session)
        .map_err(|error| HostError::Failed {
            message: error.to_string(),
        })?;
    let records = events::turn_records(session, &logged);
    let kept: Vec<_> = records
        .iter()
        .filter(|record| !record.undone)
        .rev()
        .take(TURNS)
        .collect();
    let mut lines = Vec::new();
    for record in kept.into_iter().rev() {
        for (who, text) in [
            (atomcode_i18n::product::Msg::PreviewSaid, &record.user),
            (
                atomcode_i18n::product::Msg::PreviewAnswered,
                &record.assistant,
            ),
        ] {
            let first = text.lines().find(|line| !line.trim().is_empty());
            let Some(first) = first else {
                continue;
            };
            let said: String = first.trim().chars().take(WIDE).collect();
            lines.push(format!("{} {said}", atomcode_i18n::product::t(who)));
        }
    }
    Ok(lines)
}

/// The files a coding session is configured from, grouped the way a person
/// looks for them.
///
/// **A file that is not there is reported as not there.** That is the answer
/// this question is usually asked for: "my instructions are being ignored" is
/// almost always "that file is not where you thought it was", and a list that
/// showed only what it found would answer a different question — one whose
/// answer is always "everything is fine".
///
/// A named function rather than inline in the handler so it can be judged
/// without a runtime: what this carries is a list of paths, and a list of paths
/// is exactly the kind of thing that goes quietly wrong.
fn source_groups(working_dir: &std::path::Path) -> Vec<atomcode_host_api::SourceGroup> {
    use atomcode_capabilities::memory::MemoryStore;
    use atomcode_config::config::instructions::{InstructionLevel, LayeredInstructions};
    use atomcode_host_api::{SourceFile, SourceGroup};

    let file = |label: &str, path: std::path::PathBuf| SourceFile {
        label: label.to_string(),
        present: path.is_file(),
        path: path.display().to_string(),
    };
    let instructions = LayeredInstructions::load(working_dir);
    vec![
        SourceGroup {
            label: tr(SMsg::SourceConfigFile).into_owned(),
            files: vec![file(
                &tr(SMsg::SourceSettings),
                atomcode_config::config::Config::default_path(),
            )],
        },
        SourceGroup {
            label: tr(SMsg::SourceInstructionFiles).into_owned(),
            files: instructions
                .status_lines(working_dir)
                .into_iter()
                .map(|line| SourceFile {
                    label: match line.level {
                        InstructionLevel::Global => atomcode_config::i18n::t(
                            atomcode_config::i18n::Msg::StatusInstructionScopeGlobal,
                        )
                        .into_owned(),
                        InstructionLevel::Project => atomcode_config::i18n::t(
                            atomcode_config::i18n::Msg::StatusInstructionScopeProject,
                        )
                        .into_owned(),
                        InstructionLevel::User => atomcode_config::i18n::t(
                            atomcode_config::i18n::Msg::StatusInstructionScopeUser,
                        )
                        .into_owned(),
                    },
                    // The loader's own answer, not a second `is_file` here: it
                    // is the one that decided whether this file is in play, and
                    // a page that disagreed with it would be reporting on a
                    // different run than the one that is happening.
                    present: line.found,
                    path: line.path.display().to_string(),
                })
                .collect(),
        },
        SourceGroup {
            label: tr(SMsg::SourceMemoryFiles).into_owned(),
            files: vec![
                file(
                    &atomcode_config::i18n::t(atomcode_config::i18n::Msg::StatusMemoryScopeGlobal),
                    MemoryStore::global().path().to_path_buf(),
                ),
                file(
                    &atomcode_config::i18n::t(atomcode_config::i18n::Msg::StatusMemoryScopeProject),
                    MemoryStore::project(working_dir).path().to_path_buf(),
                ),
                file(
                    &atomcode_config::i18n::t(atomcode_config::i18n::Msg::StatusMemoryScopeLocal),
                    MemoryStore::local(working_dir).path().to_path_buf(),
                ),
            ],
        },
    ]
}

#[async_trait]
impl HostControl for RuntimeControl {
    async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
        match command {
            HostCommand::NewSession { session } => {
                self.addressed(&session)?;
                let changed = self.handle.fresh_session().await.map_err(refused)?;
                self.changed(changed.session_id)
            }
            HostCommand::Resume { session, target } => {
                self.addressed(&session)?;
                if target == session {
                    return Err(HostError::SessionInUse { id: target });
                }
                let Some(stored) = self
                    .list(None)
                    .into_iter()
                    .find(|stored| stored.id == target)
                else {
                    return Err(HostError::NotFound);
                };
                if stored.needs_newer_version {
                    return Err(HostError::Failed {
                        message: format!(
                            "session {target} was written by a newer AtomCode; update to resume it"
                        ),
                    });
                }
                let changed = self.handle.resume_session(target).await.map_err(refused)?;
                self.changed(changed.session_id)
            }
            HostCommand::SetReasoningEffort { session, level } => {
                self.addressed(&session)?;
                let mut next = self.config.lock().expect("config poisoned").clone();
                next.chat_options.reasoning_effort = level;
                self.handle
                    .reassemble_provider(next.clone())
                    .await
                    .map_err(refused)?;
                *self.config.lock().expect("config poisoned") = next;
                Ok(HostReply::Done)
            }
            HostCommand::Sources { session } => {
                self.addressed(&session)?;
                // The runtime's own working directory, not the process's: a
                // session that has been `cd`-ed reads a different project's
                // files, and the page is about the session.
                let now = self.handle.context_stats().await.map_err(refused)?;
                Ok(HostReply::Sources {
                    groups: source_groups(&now.working_dir),
                })
            }
            HostCommand::ListSessions { working_dir } => Ok(HostReply::Sessions {
                sessions: self.list(working_dir),
            }),
            HostCommand::DeleteSession { session } => {
                self.delete_stored(&session)?;
                Ok(HostReply::Done)
            }
            HostCommand::PreviewSession { session } => Ok(HostReply::SessionPreview {
                lines: preview_of(&session)?,
            }),
            HostCommand::Undo {
                session,
                turn,
                based_on,
            } => {
                self.addressed(&session)?;
                self.fresh(&session, based_on)?;
                let nth = match turn {
                    None => None,
                    Some(turn) => Some(
                        self.handle
                            .rewind_points()
                            .await
                            .map_err(refused)?
                            .points
                            .into_iter()
                            .find(|point| point.turn_id == turn)
                            .map(|point| point.prompt_number)
                            .ok_or(HostError::RewindPointNotFound { turn })?,
                    ),
                };
                let undone = self.handle.undo_to_prompt(nth).await.map_err(refused)?;
                Ok(HostReply::Undone {
                    prompt: Some(undone.restored_prompt),
                    restored_files: Vec::new(),
                })
            }
            HostCommand::RewindPoints { session } => {
                self.addressed(&session)?;
                let catalog = self.handle.rewind_points().await.map_err(refused)?;
                Ok(HostReply::RewindPoints {
                    points: catalog
                        .points
                        .into_iter()
                        .rev()
                        .map(|point| atomcode_host_api::RewindPoint {
                            turn: point.turn_id,
                            prompt: point.prompt_preview,
                            // The ledger already carries a line count per file;
                            // it used to be thrown away here and counted, which
                            // left a screen able to say "3 files" and nothing
                            // about which.
                            changes: point
                                .files
                                .into_iter()
                                .map(|file| atomcode_host_api::ChangedFile {
                                    path: file.path,
                                    added: file.additions,
                                    removed: file.deletions,
                                    binary: file.binary,
                                })
                                .collect(),
                            code: point.before_tree.is_some(),
                        })
                        .collect(),
                    code_unavailable: catalog.code_unavailable.map(|why| {
                        use atomcode_coding::runtime::CodeUnavailable as Why;
                        match why {
                            Why::NotEnabled => atomcode_host_api::CodeUnavailable::NotEnabled,
                            Why::NoSession => atomcode_host_api::CodeUnavailable::NoSession,
                            Why::SetupFailed(message) => {
                                atomcode_host_api::CodeUnavailable::Failed { message }
                            }
                        }
                    }),
                })
            }
            HostCommand::Rewind {
                session,
                turn,
                scope,
                based_on,
            } => {
                self.addressed(&session)?;
                self.fresh(&session, based_on)?;
                let scope = match scope {
                    atomcode_kernel::session::RewindScope::Conversation => {
                        atomcode_coding::runtime::RewindScope::Conversation
                    }
                    atomcode_kernel::session::RewindScope::Code => {
                        atomcode_coding::runtime::RewindScope::Code
                    }
                    atomcode_kernel::session::RewindScope::Both => {
                        atomcode_coding::runtime::RewindScope::ConversationAndCode
                    }
                };
                let rewound = self.handle.rewind(turn, scope).await.map_err(refused)?;
                Ok(HostReply::Undone {
                    prompt: rewound.restored_prompt,
                    restored_files: rewound.restored_files,
                })
            }
            HostCommand::SwitchModel { session, model } => {
                self.addressed(&session)?;
                let Some(source) = self.host_config.clone() else {
                    return Err(HostError::Failed {
                        message: "this host does not resolve models".into(),
                    });
                };
                let mut next = source
                    .for_model(&model)
                    .map_err(|message| HostError::Failed { message })?;
                // The thinking level is the session's, not the model's default.
                next.chat_options.reasoning_effort = self
                    .config
                    .lock()
                    .expect("config poisoned")
                    .chat_options
                    .reasoning_effort;
                let reply = self.reconfigure(next).await?;
                // Persist the selection so it survives the next start. Without
                // this the switch is runtime-only and the next launch resolves
                // the stale `default_model` from the file (the "reverts to the
                // old model on restart" bug). Best-effort: the live switch has
                // already taken effect, so a write failure must not fail it.
                if let Err(error) = source.set_default_model(&model) {
                    tracing::warn!(
                        target: "atomcode::model",
                        %model,
                        %error,
                        "model switched for this run but could not be persisted",
                    );
                }
                Ok(reply)
            }
            HostCommand::McpStatus { session } => {
                self.addressed(&session)?;
                use atomcode_capabilities::mcp::ServerStatus;
                use atomcode_host_api::{McpServer, McpServerState};
                let status = self.handle.mcp_status().await.map_err(refused)?;
                Ok(HostReply::McpServers {
                    servers: status
                        .servers
                        .into_iter()
                        .map(|(name, state)| McpServer {
                            name,
                            state: match state {
                                ServerStatus::Connecting => McpServerState::Connecting,
                                ServerStatus::Connected => McpServerState::Connected,
                                ServerStatus::BlockedUntrusted => McpServerState::Untrusted,
                                ServerStatus::Failed(message) => McpServerState::Failed { message },
                                ServerStatus::Disconnected => McpServerState::Disconnected,
                            },
                        })
                        .collect(),
                })
            }
            HostCommand::Settings { session } => {
                self.addressed(&session)?;
                Ok(HostReply::Settings {
                    settings: self
                        .host_config
                        .clone()
                        .map(|source| source.settings())
                        .unwrap_or_default(),
                })
            }
            // Written to the host's file. Whether it takes effect now or at the
            // next start is the setting's own business — said in the listing, so
            // a person knows before they change it.
            //
            // `language` is the one that has to take effect here: it is declared
            // `ImmediateUi`, and what "immediately" means for it is the process's
            // locale table — the same table this screen's welcome block reads its
            // heading and tip descriptions from. Without this hop the file says
            // one language and everything drawn from the table keeps saying the
            // other until the next start, which is exactly what the setting's own
            // `applies` field promises does not happen.
            HostCommand::SetSetting { session, id, value } => {
                self.addressed(&session)?;
                let source = self.host_config.clone().ok_or_else(|| HostError::Failed {
                    message: tr(SMsg::HostNoEditableConfig).into_owned(),
                })?;
                source
                    .set_setting(&id, &value)
                    .map_err(|message| HostError::Failed { message })?;
                if id == "language" {
                    crate::tui_settings::apply_language(&value);
                }
                Ok(HostReply::Done)
            }
            HostCommand::ResetSetting { session, id } => {
                self.addressed(&session)?;
                let source = self.host_config.clone().ok_or_else(|| HostError::Failed {
                    message: tr(SMsg::HostNoEditableConfig).into_owned(),
                })?;
                source
                    .reset_setting(&id)
                    .map_err(|message| HostError::Failed { message })?;
                if id == "language" {
                    crate::tui_settings::apply_language("auto");
                }
                Ok(HostReply::Done)
            }
            // The four a person means, onto the four this runtime has. `Ask` is
            // its `Build`: the names differ because the contract names what a
            // person chooses and the runtime names what it does.
            HostCommand::SetMode { session, mode } => {
                self.addressed(&session)?;
                self.handle
                    .set_mode(runtime_mode(mode))
                    .await
                    .map_err(refused)?;
                Ok(HostReply::Done)
            }
            // Read back from the runtime, not from a copy this adapter keeps:
            // the mode that governs a tool call is the runtime's three flags, and
            // a second answer here could disagree with the one that refuses a
            // write. `Some` always — this host governs an execution mode, and
            // `None` is for a host whose tree carries none (see the contract).
            HostCommand::Mode { session } => {
                self.addressed(&session)?;
                let mode = self.handle.mode().await.map_err(refused)?;
                Ok(HostReply::Mode {
                    mode: Some(host_mode(mode)),
                })
            }
            // A new session, because what a conversation read and wrote belongs
            // to where it ran — the runtime says so by handing back a session
            // id, and the front end follows that stream instead.
            HostCommand::ChangeDirectory { session, directory } => {
                self.addressed(&session)?;
                let changed = self
                    .handle
                    .change_directory(std::path::PathBuf::from(directory))
                    .await
                    .map_err(refused)?;
                self.changed(changed.session_id)
            }
            HostCommand::McpTools { session, server } => {
                self.addressed(&session)?;
                let tools = self.handle.mcp_tools(server).await.map_err(refused)?;
                Ok(HostReply::McpTools { tools: tools.tools })
            }
            // The catalog is the tree's, read through the runtime that owns it
            // — the screen may not reach into the agent's App
            // (`docs/adr/0022` §3).
            HostCommand::ToolCatalog { session } => {
                self.addressed(&session)?;
                let tools = self.handle.tool_catalog().await.map_err(refused)?;
                Ok(HostReply::ToolCatalog {
                    tools: tools.into_iter().map(catalog_tool).collect(),
                })
            }
            // Answers with the catalog as it now is, so a screen renders what
            // happened rather than what it asked for.
            HostCommand::SwitchTool {
                session,
                pattern,
                on,
            } => {
                self.addressed(&session)?;
                let tools = self
                    .handle
                    .switch_tool(pattern, on)
                    .await
                    .map_err(refused)?;
                Ok(HostReply::ToolCatalog {
                    tools: tools.into_iter().map(catalog_tool).collect(),
                })
            }
            HostCommand::WithdrawMcpTools { session } => {
                self.addressed(&session)?;
                self.handle.withdraw_mcp_tools().await.map_err(refused)?;
                Ok(HostReply::Done)
            }
            // The catalog is the tree's — the `models` seam the `/model` switch
            // already resolves through — read here rather than by the screen,
            // which may not reach into the agent's App (`docs/adr/0022` §3).
            HostCommand::Models { session } => {
                self.addressed(&session)?;
                use atomcode_host_api::ModelChoice;
                let app = self.front_end.app().ok_or(HostError::Unavailable)?;
                let models = app
                    .service::<atomcode_harness::seams::ModelsSvc>()
                    .ok_or_else(|| HostError::Failed {
                        message: tr(SMsg::HostNoModelCatalog).into_owned(),
                    })?;
                let current = models.current();
                Ok(HostReply::Models {
                    models: models
                        .list()
                        .into_iter()
                        .map(|model| ModelChoice {
                            // What tells two of them apart at a glance: whose
                            // account it is, and how much it can hold.
                            about: format!("{} · {}k", model.account, model.context_window / 1000),
                            id: model.id,
                        })
                        .collect(),
                    current,
                })
            }
            // A name is a fact in the log like everything else about the
            // session (`docs/adr/0024`): committed into the live one, so every
            // front end reading it hears the new name without being told twice.
            HostCommand::Rename { session, title } => {
                self.addressed(&session)?;
                let title = title.trim().to_string();
                if title.is_empty() {
                    return Err(HostError::Failed {
                        message: tr(SMsg::NameCannotBeEmpty).into_owned(),
                    });
                }
                let app = self.front_end.app().ok_or(HostError::Unavailable)?;
                let agent = app
                    .service::<atomcode_harness::seams::AgentsSvc>()
                    .and_then(|agents| agents.by_session(&session))
                    .ok_or(HostError::NotFound)?;
                let log = agent.session();
                let turn = log.current_turn();
                atomcode_harness::session::commit(
                    agent.ctx(),
                    &log,
                    // The person named it: `/rename` is an explicit choice, so the
                    // driver may pin it (a composer pill) as user-chosen.
                    atomcode_harness::session::SessionEvent::Titled {
                        turn,
                        title,
                        user_set: true,
                    },
                );
                Ok(HostReply::Done)
            }
            HostCommand::Reload { session } => {
                self.addressed(&session)?;
                // The configuration first: with it where it was, what follows is
                // a re-read of the disk beside it and the session stays in the
                // tree it is in. With it moved — or with a host that cannot say
                // — the graph is built again from it.
                if let Some(source) = self.host_config.clone() {
                    let now = source.fingerprint();
                    let before = self
                        .fingerprint
                        .lock()
                        .expect("fingerprint poisoned")
                        .clone();
                    if now.is_none() || now != before {
                        let next = source
                            .current()
                            .map_err(|message| HostError::Failed { message })?;
                        let changed = self
                            .handle
                            .reprepare_config(next.clone())
                            .await
                            .map_err(refused)?;
                        *self.config.lock().expect("config poisoned") = next;
                        *self.fingerprint.lock().expect("fingerprint poisoned") = now;
                        return match self.changed(changed.session_id)? {
                            HostReply::SessionChanged { session: now } if now == session => {
                                Ok(HostReply::Done)
                            }
                            other => Ok(other),
                        };
                    }
                }
                let changed = self.handle.reload_capabilities().await.map_err(refused)?;
                match self.changed(changed.session_id)? {
                    HostReply::SessionChanged { session: now } if now == session => {
                        Ok(HostReply::Done)
                    }
                    other => Ok(other),
                }
            }
            HostCommand::SignOut { session } => {
                self.addressed(&session)?;
                // Sharing first, and before the credentials: signing out is
                // "nobody is me any more", and a session still on the hub is
                // still readable from a phone or a browser by whoever has the
                // link. Deleting the credentials first would make "I logged
                // out" and "it is still on my phone" true at the same time —
                // and this step is the one that can fail.
                //
                // Best-effort by construction (`stop_all_sharing` never
                // errors): a relay child that will not die must not be able to
                // keep somebody logged in.
                crate::tui_share::stop_all_sharing();
                // Credentials, then the live provider — the same order
                // the classic screen's logout uses: a failure below must not
                // leave the identity file behind saying otherwise.
                atomcode_auth::logout().map_err(|error| HostError::Failed {
                    message: error.to_string(),
                })?;
                self.handle
                    .deactivate_provider(ProviderUnavailableReason::AuthenticationRequired)
                    .await
                    .map_err(refused)?;
                Ok(HostReply::Done)
            }
            HostCommand::SignIn { session } => {
                self.addressed(&session)?;
                let next = match self.host_config.clone() {
                    Some(source) => source
                        .current()
                        .map_err(|message| HostError::Failed { message })?,
                    None => self.config.lock().expect("config poisoned").clone(),
                };
                self.reconfigure(next).await
            }
            HostCommand::Autonomy { session } => {
                self.addressed(&session)?;
                let now = self.handle.autonomy().await.map_err(refused)?;
                // A goal wins when both are somehow registered: it is the one
                // with a condition to report, and a person who set a goal is
                // waiting on the goal.
                let running = now
                    .goal
                    .map(running_of_goal)
                    .or_else(|| now.looping.map(running_of_loop));
                Ok(HostReply::Autonomy { running })
            }
            HostCommand::Context { session } => {
                self.addressed(&session)?;
                let now = self.handle.context_stats().await.map_err(refused)?;
                Ok(HostReply::Context {
                    window: now.context_window,
                    used: now.used_tokens,
                    model: now.model,
                    working_dir: now.working_dir.display().to_string(),
                })
            }
            // Best-effort by contract: a host that meters nothing, and a source
            // that is slow or down, both answer with an empty list. "I could not
            // reach the meter" and "there is no meter" look the same to a person
            // — neither is a number — and making this fail would make `/usage`
            // the one command that breaks when the network hiccups.
            HostCommand::Usage { session } => {
                self.addressed(&session)?;
                let (windows, plan, spent) = self.handle.usage().await.map_err(refused)?;
                Ok(HostReply::Usage {
                    plan: plan.map(|plan| atomcode_host_api::Entitlement {
                        plan: plan.plan,
                        active: plan.active,
                        claimed_at: plan.claimed_at,
                        expires_at: plan.expires_at,
                        remaining_days: plan.remaining_days,
                        total_days: plan.total_days,
                    }),
                    stats: spent.map(|spent| atomcode_host_api::UsageStats {
                        from: spent.from,
                        to: spent.to,
                        models: spent
                            .models
                            .into_iter()
                            .map(|m| atomcode_host_api::ModelUse {
                                name: m.name,
                                tokens: m.tokens,
                                requests: m.requests,
                            })
                            .collect(),
                        daily: spent
                            .daily
                            .into_iter()
                            .map(|d| atomcode_host_api::DayUse {
                                date: d.date,
                                tokens: d.tokens,
                                requests: d.requests,
                            })
                            .collect(),
                        series: spent
                            .series
                            .into_iter()
                            .map(|s| atomcode_host_api::ModelSeries {
                                name: s.name,
                                daily: s.daily,
                            })
                            .collect(),
                        total_tokens: spent.total_tokens,
                        total_requests: spent.total_requests,
                    }),
                    windows: windows
                        .into_iter()
                        .map(|w| atomcode_host_api::UsageWindow {
                            label: w.reset_label,
                            exhausted: w.quota_exhausted,
                            resets_at: w.reset_at_display,
                            resets_in_seconds: w.seconds_until_reset,
                            window_seconds: w.window_size_seconds.max(0),
                            // Whole percent, clamped: it is read off a bar.
                            used_percent: (w.usage_percent > 0.0)
                                .then(|| w.usage_percent.clamp(0.0, 100.0).round() as u8),
                            calls_used: (w.calls_used > 0).then_some(w.calls_used),
                            call_limit: (w.call_limit > 0).then_some(w.call_limit),
                        })
                        .collect(),
                })
            }
            HostCommand::Providers { session } => {
                self.addressed(&session)?;
                let providers = self
                    .host_config
                    .clone()
                    .map(|source| source.providers())
                    .unwrap_or_default();
                Ok(HostReply::Providers {
                    providers,
                    // What the running graph was built from, which is the one a
                    // person is on — not what the file calls the default, which
                    // a `/model` since then may have moved off.
                    current: Some(self.config.lock().expect("config poisoned").model.clone()),
                })
            }
            HostCommand::Changes { session, file } => {
                self.addressed(&session)?;
                let changes = self.handle.workspace_changes(file).await.map_err(refused)?;
                Ok(HostReply::Changes {
                    files: changes
                        .files
                        .into_iter()
                        .map(|f| atomcode_host_api::ChangedFile {
                            path: f.path,
                            added: f.additions,
                            removed: f.deletions,
                            binary: f.binary,
                        })
                        .collect(),
                    diff: changes.diff,
                    unavailable: changes.unavailable,
                })
            }
            HostCommand::WhoAmI { session } => {
                self.addressed(&session)?;
                let identity = self
                    .host_config
                    .clone()
                    .and_then(|source| source.identity());
                Ok(match identity {
                    Some(Identity { who, detail }) => HostReply::Identity {
                        signed_in: true,
                        who: Some(who),
                        detail,
                    },
                    None => HostReply::Identity {
                        signed_in: false,
                        who: None,
                        detail: None,
                    },
                })
            }
            // Read as a setting, so a screen that can draw one setting can draw
            // this one, and `/config` and `/think` are looking at one value.
            HostCommand::Thinking { session } => {
                self.addressed(&session)?;
                let on = self
                    .config
                    .lock()
                    .expect("config poisoned")
                    .thinking_enabled
                    .unwrap_or(false);
                Ok(HostReply::Settings {
                    settings: vec![atomcode_host_api::Setting {
                        id: "thinking".into(),
                        label: tr(SMsg::SettingThinking).into_owned(),
                        value: if on { "on".into() } else { "off".into() },
                        accepts: "on | off".into(),
                        applies: atomcode_config::i18n::t(
                            atomcode_config::i18n::Msg::AppliesNextTurnCli,
                        )
                        .into_owned(),
                    }],
                })
            }
            // The provider is built with it, so this reassembles — the same
            // path the thinking *level* takes, for the same reason.
            HostCommand::SetThinking { session, on } => {
                self.addressed(&session)?;
                let mut next = self.config.lock().expect("config poisoned").clone();
                next.thinking_enabled = Some(on);
                self.handle
                    .reassemble_provider(next.clone())
                    .await
                    .map_err(refused)?;
                *self.config.lock().expect("config poisoned") = next;
                Ok(HostReply::Done)
            }
            HostCommand::Readiness { session } => {
                self.addressed(&session)?;
                // Three checks the old driver protocol exposed separately and
                // the new bridge never carried over, asked once and answered
                // with what to do. Order matters: a stopped runtime cannot be
                // fixed by signing in, so it is reported first.
                if self.handle.is_stopped() {
                    return Ok(HostReply::Readiness {
                        ready: false,
                        why: Some(tr(SMsg::RuntimeAlreadyStopped).into_owned()),
                        fix: None,
                    });
                }
                Ok(readiness_for(self.handle.provider_unavailable_reason()))
            }
            _ => Err(HostError::Failed {
                message: "this host does not do that yet".into(),
            }),
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.watchers.lock().expect("watchers poisoned").push(tx);
        rx
    }
}

/// `docs/adr/0021` §8: what each runtime error is in the host contract.
/// A runtime refusal, as the contract says it.
///
/// A function rather than `impl From<RuntimeError> for HostError`: both types
/// are foreign to this crate now that the adapter lives here, and the orphan
/// rule is right to stop it — the conversion is this host's opinion, not
/// something either crate should carry for everyone.
/// What a provider that cannot serve means for a person about to type.
///
/// A table rather than a condition at the call site, for the same reason
/// [`refused`] is one (`docs/adr/0021` §8): every cause gets an answer, and a
/// new cause cannot be added without deciding what the screen says about it.
///
/// `fix` names a command this build has. `None` where nothing this screen can
/// run would help — naming a command that does nothing would be worse than
/// saying nothing, because the person would run it.
pub fn readiness_for(reason: Option<ProviderUnavailableReason>) -> HostReply {
    let (why, fix) = match reason {
        None => {
            return HostReply::Readiness {
                ready: true,
                why: None,
                fix: None,
            }
        }
        Some(ProviderUnavailableReason::NotConfigured) => {
            // Signing in again cannot help a machine that has no provider at
            // all; the wizard that can is `tui_onboarding`, and naming it is
            // as far as this host's say goes.
            (
                tr(SMsg::NoProviderConfigured),
                Some(crate::tui_onboarding::COMMAND),
            )
        }
        Some(ProviderUnavailableReason::AuthenticationRequired) => {
            (tr(SMsg::LoginExpired), Some("login"))
        }
        Some(ProviderUnavailableReason::UnsupportedBuild) => {
            (tr(SMsg::ProviderUnsupportedByBuild), None)
        }
    };
    HostReply::Readiness {
        ready: false,
        why: Some(why.into()),
        fix: fix.map(str::to_string),
    }
}

pub fn refused(error: RuntimeError) -> HostError {
    {
        match error {
            RuntimeError::Busy => HostError::Busy {
                reason: "the runtime is busy".into(),
            },
            RuntimeError::Cancelled => HostError::Cancelled,
            RuntimeError::SessionInUse { id } => HostError::SessionInUse { id },
            RuntimeError::DeliveryFailed | RuntimeError::Unavailable => HostError::Unavailable,
            RuntimeError::ProviderUnavailable(reason) => HostError::ProviderUnavailable {
                reason: match reason {
                    ProviderUnavailableReason::NotConfigured => {
                        atomcode_host_api::ProviderUnavailableReason::NotConfigured
                    }
                    ProviderUnavailableReason::AuthenticationRequired => {
                        atomcode_host_api::ProviderUnavailableReason::AuthenticationRequired
                    }
                    ProviderUnavailableReason::UnsupportedBuild => {
                        atomcode_host_api::ProviderUnavailableReason::UnsupportedBuild
                    }
                },
            },
            RuntimeError::SnapshotUnavailable(_) => HostError::NotFound,
            RuntimeError::InvalidWorkingDirectory(message) => {
                HostError::InvalidWorkingDirectory { message }
            }
            RuntimeError::UndoOutOfRange {
                requested,
                available,
            } => HostError::UndoOutOfRange {
                requested,
                available,
            },
            RuntimeError::RewindPointUnavailable { turn_id } => {
                HostError::RewindPointNotFound { turn: turn_id }
            }
            RuntimeError::CodeRewindUnavailable(message) => {
                HostError::CodeRewindUnavailable { message }
            }
            // Not the host's: a stale question is the handle protocol's, and a
            // policy intervention belongs to the row that raises it.
            other @ (RuntimeError::ReconfigureFailed(_)
            | RuntimeError::StaleRequest { .. }
            | RuntimeError::NoPendingPolicyIntervention
            | RuntimeError::InvalidPolicyRecoveryAction) => HostError::Failed {
                message: other.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod source_tests {
    use super::source_groups;
    use atomcode_i18n::screen::{t as tr, Msg as SMsg};

    /// Every file the session is configured from is reported, and the ones that
    /// are not there are reported as not there.
    ///
    /// The missing half is the point. A person opens this page because the
    /// agent is not doing what their files say, and the usual answer is that
    /// the file is somewhere else — a list that showed only what it found
    /// would be a list that can never say that.
    #[test]
    fn a_configuration_file_that_is_not_there_is_still_reported() {
        let project = tempfile::tempdir().expect("a temp dir");
        // One instruction file present, the rest of the layers absent.
        std::fs::write(project.path().join("AGENTS.md"), "# 项目").expect("write");

        let groups = source_groups(project.path());
        // Read from the table rather than spelled out, so this asserts *which
        // three groups* there are and not one language's words for them.
        let labels: Vec<&str> = groups.iter().map(|group| group.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                tr(SMsg::SourceConfigFile),
                tr(SMsg::SourceInstructionFiles),
                tr(SMsg::SourceMemoryFiles),
            ]
        );

        let all: Vec<&atomcode_host_api::SourceFile> =
            groups.iter().flat_map(|group| group.files.iter()).collect();
        assert!(
            all.iter().any(|file| file.present),
            "the one that is there is marked found: {all:#?}"
        );
        // Both halves of the list, because they are found two different ways:
        // the instruction layers come from the loader's own answer and
        // everything else from asking the filesystem. A criterion satisfied by
        // one of them would leave the other free to say whatever it liked.
        let by_path = |ends: &str| -> &atomcode_host_api::SourceFile {
            all.iter()
                .find(|file| file.path.ends_with(ends))
                .unwrap_or_else(|| panic!("{ends}: {all:#?}"))
        };
        assert!(
            by_path("AGENTS.md").present,
            "the loader found the project's instructions"
        );
        assert!(
            !by_path("ATOMCODE.md").present,
            "and says the global layer is absent"
        );
        assert!(
            !by_path(".atomcode/memory.md").present,
            "the filesystem half answers too: this project has no memory file"
        );
        // Every entry carries a path, whether or not it was found — the path is
        // the answer to "then where should it be".
        assert!(
            all.iter().all(|file| !file.path.is_empty()),
            "every entry says where it would be: {all:#?}"
        );
        assert!(
            all.iter().any(|file| file.path.ends_with("AGENTS.md")),
            "the project's own instructions are among them: {all:#?}"
        );
    }
}

#[cfg(test)]
mod completion_tests {
    use super::{persistence_failure, without_a_log};

    /// "No log right now" and "no log, ever" are different answers.
    ///
    /// The guard compares a caller's position against the front end's own log.
    /// A screen between Apps has one and cannot be read — refuse. A protocol
    /// server that never had one has nothing to be stale against — allow, the
    /// same answer the guard already gives for a session its feed has never
    /// seen. Answering both with "unavailable" meant a front end with no screen
    /// could never undo anything, which is what this fixed.
    #[test]
    fn a_front_end_that_never_kept_a_log_is_not_stale() {
        assert!(without_a_log(0).is_ok());
        assert!(matches!(
            without_a_log(1),
            Err(atomcode_host_api::HostError::Unavailable)
        ));
        assert!(matches!(
            without_a_log(7),
            Err(atomcode_host_api::HostError::Unavailable)
        ));
    }
    use atomcode_coding::{RuntimeSnapshotError, RuntimeTurnStats, TurnCompletion};
    use atomcode_kernel::event::StopReason;

    /// A turn that finished and a turn whose record was kept are two claims,
    /// and before this only one of them reached a front end.
    ///
    /// The screen lost it entirely and ACP reported it as an internal error —
    /// two different wrong answers to "the log did not get your turn", which is
    /// the one thing a person has to know, because the log is the session's
    /// only authority (`docs/adr/0024`).
    #[test]
    fn a_turn_that_finished_and_a_turn_that_was_kept_are_two_answers() {
        let kept = TurnCompletion::Completed {
            turn_id: 1,
            reason: StopReason::Stopped,
            snapshot: std::sync::Arc::new(atomcode_kernel::message::SessionSnapshot::new(
                Vec::new(),
            )),
            stats: RuntimeTurnStats::default(),
        };
        assert_eq!(persistence_failure(&kept), None);

        let lost = TurnCompletion::SnapshotUnavailable {
            turn_id: 1,
            reason: StopReason::Stopped,
            error: RuntimeSnapshotError {
                message: "磁盘满了".into(),
            },
            stats: RuntimeTurnStats::default(),
        };
        // The reason the turn stopped is the same in both; what differs is
        // whether anybody will be able to read about it later.
        assert_eq!(persistence_failure(&lost).as_deref(), Some("磁盘满了"));
    }
}

/// The runtime's listing in the contract's words. The two say the same thing
/// and are deliberately separate types: one is this product's, the other is
/// what any front end reads (`docs/adr/0021` §2).
fn catalog_tool(listing: atomcode_harness::seams::ToolListing) -> atomcode_host_api::CatalogTool {
    use atomcode_harness::seams::ToolState as Live;
    use atomcode_host_api::ToolState as Wire;
    atomcode_host_api::CatalogTool {
        name: listing.name,
        owner: listing.owner,
        state: match listing.state {
            Live::On => Wire::On,
            Live::OffInSession => Wire::OffInSession,
            Live::ExcludedByConfig => Wire::ExcludedByConfig,
        },
    }
}
