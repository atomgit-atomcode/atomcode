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
        Err("这个宿主的配置不能从屏幕上改".into())
    }

    /// The providers this host is configured with, for a person to pick between.
    ///
    /// Never a credential: see [`atomcode_host_api::ProviderChoice`]. Empty
    /// for a host that has no configuration file to read them from.
    fn providers(&self) -> Vec<atomcode_host_api::ProviderChoice> {
        Vec::new()
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
                CodingRuntimeEvent::ProviderChanged { .. }
                | CodingRuntimeEvent::ReasoningEffortChanged { .. } => {
                    if let Some(app) = watched.front_end.app() {
                        watched.front_end.feed().redescribe(&app);
                    }
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
    Ok(HostConnection {
        session,
        commands,
        events,
        control,
    })
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
        // the way every other failure is.
        (None, Err((_, Some(message)))) => {
            let _ = out.send(AgentEvent::Error {
                message,
                http_status: None,
                code: None,
                retryable: None,
            });
        }
        (None, _) => {}
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
        AgentCommand::SendMessage { text, images } => handle
            .submit(UserInput { text, images })
            .await
            .map(|_| None)
            .map_err(refused),
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
        AgentCommand::Cancel => handle
            .cancel()
            .await
            .map(|_| None)
            .map_err(|error| (command_error(error), None)),
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
        paused: (!looping.active).then(|| "停着".to_string()),
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
    fn fresh(
        &self,
        session: &str,
        based_on: atomcode_kernel::session::SeqNo,
    ) -> Result<(), HostError> {
        let Some(app) = self.front_end.app() else {
            return Err(HostError::Unavailable);
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
            HostCommand::ListSessions { working_dir } => Ok(HostReply::Sessions {
                sessions: self.list(working_dir),
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
                            files: point.files.len(),
                            code: point.before_tree.is_some(),
                        })
                        .collect(),
                    code_unavailable: catalog.code_unavailable,
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
                self.reconfigure(next).await
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
            HostCommand::SetSetting { session, id, value } => {
                self.addressed(&session)?;
                let source = self.host_config.clone().ok_or_else(|| HostError::Failed {
                    message: "这个宿主没有可改的配置".into(),
                })?;
                source
                    .set_setting(&id, &value)
                    .map_err(|message| HostError::Failed { message })?;
                Ok(HostReply::Done)
            }
            // The four a person means, onto the four this runtime has. `Ask` is
            // its `Build`: the names differ because the contract names what a
            // person chooses and the runtime names what it does.
            HostCommand::SetMode { session, mode } => {
                self.addressed(&session)?;
                use atomcode_host_api::Mode;
                self.handle
                    .set_mode(match mode {
                        Mode::Plan => atomcode_coding::RuntimeMode::Plan,
                        Mode::Ask => atomcode_coding::RuntimeMode::Build,
                        Mode::AcceptEdits => atomcode_coding::RuntimeMode::AcceptEdits,
                        Mode::Auto => atomcode_coding::RuntimeMode::Auto,
                    })
                    .await
                    .map_err(refused)?;
                Ok(HostReply::Done)
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
                        message: "这个宿主没有模型目录".into(),
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
                        message: "名字不能是空的".into(),
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
                    atomcode_harness::session::SessionEvent::Titled { turn, title },
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
            // Best-effort by contract: a host that meters nothing, and a source
            // that is slow or down, both answer with an empty list. "I could not
            // reach the meter" and "there is no meter" look the same to a person
            // — neither is a number — and making this fail would make `/usage`
            // the one command that breaks when the network hiccups.
            HostCommand::Usage { session } => {
                self.addressed(&session)?;
                let windows = self.handle.usage().await.map_err(refused)?;
                Ok(HostReply::Usage {
                    windows: windows
                        .into_iter()
                        .map(|w| atomcode_host_api::UsageWindow {
                            label: w.reset_label,
                            exhausted: w.quota_exhausted,
                            resets_at: w.reset_at_display,
                            resets_in_seconds: w.seconds_until_reset,
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
                        label: "思考".into(),
                        value: if on { "on".into() } else { "off".into() },
                        accepts: "on | off".into(),
                        applies: "下一回合".into(),
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
