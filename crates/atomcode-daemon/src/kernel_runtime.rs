//! Daemon entry points for the unified native [`atomcode_coding::CodingRuntime`].

use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::{PrepareOptions, SessionMode};
use atomcode_coding::runtime::CodingRuntimeEvent;
use atomcode_coding::CodingRuntimeConfig;
use tokio::sync::{mpsc, watch};

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
    start_native_runtime_with_session_bootstrap(
        cfg,
        session,
        atomcode_coding::ProviderBootstrap::Required,
    )
    .await
}

async fn start_native_runtime_with_session_bootstrap(
    cfg: CodingRuntimeConfig,
    session: SessionMode,
    bootstrap: atomcode_coding::ProviderBootstrap,
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
    let runtime = atomcode_coding::CodingRuntime::start_with_bootstrap(
        atomcode_coding::CodingRuntimeStart {
            agent: coding_cfg.clone(),
            prepare,
            provider_factory: crate::coding_provider_factory(),
            plugin_hooks: crate::installed_plugin_hook_source(),
        },
        bootstrap,
    )
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
    watch::Receiver<atomcode_coding::DeferredRuntimeState>,
) {
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = watch::channel(atomcode_coding::DeferredRuntimeState::Starting);
    tokio::spawn(async move {
        let bootstrap = if cfg.model.is_empty() {
            atomcode_coding::ProviderBootstrap::Unavailable(
                atomcode_coding::ProviderUnavailableReason::NotConfigured,
            )
        } else {
            atomcode_coding::ProviderBootstrap::RecoverAuthentication
        };
        let runtime = start_native_runtime_with_session_bootstrap(
            cfg,
            SessionMode::ExternalSnapshot { id, snapshot },
            bootstrap,
        )
        .await;
        let (runtime, _) = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                let message = error.to_string();
                state_tx.send_replace(atomcode_coding::DeferredRuntimeState::Failed(
                    message.clone(),
                ));
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
        state_tx.send_replace(atomcode_coding::DeferredRuntimeState::Ready(handle.clone()));
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
                        atomcode_coding::DriverCommand::DeactivateProvider(reason) => Some(
                            CodingRuntimeEvent::ProviderDeactivationFinished(
                                handle.deactivate_provider(reason).await,
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
    (control_tx, event_rx, state_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deferred_runtime_publishes_authoritative_awaiting_provider_handle() {
        let working_dir = tempfile::tempdir().unwrap();
        let config = atomcode_config::config::Config::default();
        let cfg = CodingRuntimeConfig::from_config(
            &config,
            working_dir.path(),
            None,
            None,
            false,
            true,
        );
        let (control_tx, _event_rx, mut state_rx) =
            spawn_native_runtime_for_session_deferred(
                cfg,
                "deferred-test".into(),
                atomcode_kernel::message::SessionSnapshot::new(Vec::new()),
            );

        tokio::time::timeout(std::time::Duration::from_secs(5), state_rx.changed())
            .await
            .unwrap()
            .unwrap();
        let state = state_rx.borrow().clone();
        let atomcode_coding::DeferredRuntimeState::Ready(handle) = state else {
            panic!("deferred runtime did not publish a ready handle");
        };
        assert_eq!(
            handle.status().phase,
            atomcode_coding::RuntimePhase::AwaitingProvider
        );

        control_tx
            .send(atomcode_coding::DriverCommand::Shutdown)
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), control_tx.closed())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn deferred_runtime_reports_same_session_conflict() {
        let home = tempfile::tempdir().unwrap();
        let working_dir = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let config = atomcode_config::config::Config::default();
        let cfg = || {
            CodingRuntimeConfig::from_config(
                &config,
                working_dir.path(),
                None,
                None,
                false,
                true,
            )
        };
        let snapshot = || atomcode_kernel::message::SessionSnapshot::new(Vec::new());
        let (first_tx, _first_events, mut first_state) =
            spawn_native_runtime_for_session_deferred(
                cfg(),
                "same-deferred-session".into(),
                snapshot(),
            );
        tokio::time::timeout(std::time::Duration::from_secs(5), first_state.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            *first_state.borrow(),
            atomcode_coding::DeferredRuntimeState::Ready(_)
        ));

        let (second_tx, _second_events, mut second_state) =
            spawn_native_runtime_for_session_deferred(
                cfg(),
                "same-deferred-session".into(),
                snapshot(),
            );
        tokio::time::timeout(std::time::Duration::from_secs(5), second_state.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            &*second_state.borrow(),
            atomcode_coding::DeferredRuntimeState::Failed(message)
                if message.contains("already in use")
        ));

        first_tx
            .send(atomcode_coding::DriverCommand::Shutdown)
            .unwrap();
        second_tx
            .send(atomcode_coding::DriverCommand::Shutdown)
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), first_tx.closed())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), second_tx.closed())
            .await
            .unwrap();
    }
}
