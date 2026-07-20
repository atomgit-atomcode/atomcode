//! Daemon entry points for the unified native [`atomcode_coding::CodingRuntime`].

use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::{PrepareOptions, SessionMode};
use atomcode_coding::runtime::CodingRuntimeEvent;
use atomcode_coding::CodingRuntimeConfig;
use tokio::sync::mpsc;

/// Convert the shared runtime configuration into the native coding-agent config.
pub fn coding_config_from_runtime(cfg: &CodingRuntimeConfig) -> CodingAgentConfig {
    cfg.agent_config()
}

pub async fn start_native_runtime(
    cfg: CodingRuntimeConfig,
) -> Result<(atomcode_coding::CodingRuntime, CodingAgentConfig), atomcode_coding::RuntimeStartError>
{
    start_native_runtime_with_session(cfg, SessionMode::Fresh).await
}

pub async fn start_native_runtime_with_session(
    cfg: CodingRuntimeConfig,
    session: SessionMode,
) -> Result<(atomcode_coding::CodingRuntime, CodingAgentConfig), atomcode_coding::RuntimeStartError>
{
    let coding_cfg = coding_config_from_runtime(&cfg);
    let prepare = PrepareOptions {
        session,
        skill_dirs: None,
        plugin_skill_dirs: crate::gather_plugin_skill_dirs(),
        mcp: cfg.mcp,
        memory: true,
        web: true,
        review: true,
        rate_limit_source: Some(crate::coding_plan_rate_limit_source()),
    };
    let runtime = atomcode_coding::CodingRuntime::start(atomcode_coding::CodingRuntimeStart {
        agent: coding_cfg.clone(),
        prepare,
        provider_factory: crate::coding_provider_factory(),
        plugin_hooks: crate::installed_plugin_hook_source(),
    })
    .await?;
    Ok((runtime, coding_cfg))
}

/// Start a restored runtime asynchronously while immediately returning its native
/// command and event channels to the TUI session-switch path.
pub fn spawn_native_runtime_for_session_deferred(
    cfg: CodingRuntimeConfig,
    id: String,
    snapshot: atomcode_kernel::message::SessionSnapshot,
) -> (
    mpsc::UnboundedSender<atomcode_coding::DriverCommand>,
    mpsc::UnboundedReceiver<CodingRuntimeEvent>,
) {
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let runtime =
            start_native_runtime_with_session(cfg, SessionMode::ExternalSnapshot { id, snapshot })
                .await;
        let (runtime, _) = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                let message = error.to_string();
                let _ = event_tx.send(CodingRuntimeEvent::Agent(
                    atomcode_kernel::event::AgentEvent::Error {
                        message: message.clone(),
                        http_status: None,
                        code: None,
                    },
                ));
                while let Some(control) = control_rx.recv().await {
                    if matches!(control, atomcode_coding::DriverCommand::Shutdown) {
                        break;
                    }
                    let _ = event_tx.send(CodingRuntimeEvent::Agent(
                        atomcode_kernel::event::AgentEvent::Error {
                            message: format!("runtime unavailable: {message}"),
                            http_status: None,
                            code: None,
                        },
                    ));
                }
                return;
            }
        };
        let atomcode_coding::CodingRuntime {
            handle,
            mut events,
            task,
            ..
        } = runtime;
        loop {
            tokio::select! {
                control = control_rx.recv() => {
                    let Some(control) = control else {
                        let _ = handle.shutdown().await;
                        break;
                    };
                    let event = match control {
                        atomcode_coding::DriverCommand::UndoToPrompt(nth) => Some(
                            CodingRuntimeEvent::UndoFinished(handle.undo_to_prompt(nth).await),
                        ),
                        atomcode_coding::DriverCommand::RefreshContextStats => Some(
                            CodingRuntimeEvent::ContextStatsRefreshed(handle.context_stats().await),
                        ),
                        atomcode_coding::DriverCommand::RestoreSnapshotCorrelated {
                            snapshot,
                            correlation_id,
                        } => {
                            let result = async {
                                handle.restore_snapshot(snapshot).await?;
                                handle.snapshot().await
                            }
                            .await;
                            Some(CodingRuntimeEvent::SnapshotRestoreFinished {
                                correlation_id,
                                result,
                            })
                        }
                        atomcode_coding::DriverCommand::ReloadProvider(next) => Some(
                            CodingRuntimeEvent::ProviderReloadFinished(
                                handle.reassemble_provider(next).await,
                            ),
                        ),
                        control => {
                            if let Err(error) = handle.dispatch(control) {
                                let _ = event_tx.send(CodingRuntimeEvent::Agent(
                                    atomcode_kernel::event::AgentEvent::Error {
                                        message: error.to_string(),
                                        http_status: None,
                                        code: None,
                                    },
                                ));
                            }
                            None
                        }
                    };
                    if let Some(event) = event {
                        let _ = event_tx.send(event);
                    }
                }
                event = events.recv() => match event {
                    Some(event) => {
                        if event_tx.send(event.event).is_err() {
                            let _ = handle.shutdown().await;
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        let _ = task.await;
    });
    (control_tx, event_rx)
}
