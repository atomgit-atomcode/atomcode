//! Fires due scheduled tasks inside the daemon process.
//!
//! Android has no system scheduler this app can register with, and no `atomcode`
//! command that could be spawned — the daemon is a library loaded into the app
//! process. A task that comes due therefore has to run *here*, in the process that
//! already owns a runtime, a session store and a provider. Every other platform
//! keeps its OS scheduler (`systemctl` / `launchd` / Task Scheduler) and never
//! starts this loop.
//!
//! Why not reuse the CLI's `schedule run`: that entry point spawns the CLI, which
//! is exactly what cannot happen here. What it *does* is reproduced below against
//! the same runtime API — a fresh session, no interactive approvals, and the run
//! lease taken before anything else so two sweeps cannot start the task twice.
//!
//! Consequences worth knowing (they are the honest limits of running inside a
//! phone app, not bugs):
//!
//! * A task only fires while the app process is alive. There is no wakeup that
//!   starts a stopped app; that would need `AlarmManager` and a broadcast receiver.
//! * Nobody can answer an approval, so one is **denied** and counted instead of
//!   parking the run forever. A turn that needed approval finishes as `error`
//!   with that reason recorded — the same posture as the CLI's unattended path.

use std::time::Duration;

use atomcode_coding::{CodingRuntimeEvent, RuntimeMode, SessionMode, TurnCompletion, UserInput};
use atomcode_config::schedule::ScheduleTask;
use atomcode_schedule::service::{Entry, Service};

use crate::AppState;

/// How often the task list is read.
///
/// A rule's granularity is minutes (daily / weekly / hourly / interval / cron), so
/// half a minute is precise enough, and a sweep only reads the store.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Start the sweep loop. Android only: elsewhere the OS scheduler owns this.
pub(crate) fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; skipping it avoids a sweep while the app is
        // still starting, before any runtime could have a provider.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = sweep(&state).await {
                tracing::warn!(%error, "scheduled task sweep failed");
            }
        }
    });
}

/// Run every task that is due and not already running.
async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let service = crate::api_schedule::system_service();
    let now = unix_seconds();
    for entry in service.catalog()? {
        // The rule lives in `atomcode_schedule` so it can be tested on a machine
        // that does not build this Android-only module.
        if !atomcode_schedule::service::due_at(&entry, now) {
            continue;
        }
        // `next_run` is the schedule's own answer to "when is this due", so the
        // loop never re-derives timing from the rule and cannot drift from what
        // the panel shows.
        let Some(next_run) = entry.next_run.as_ref() else {
            continue;
        };
        run_due(state, &service, &entry, next_run.at).await;
    }
    Ok(())
}

/// Take the run lease, drive one turn, record what happened.
async fn run_due(state: &AppState, service: &Service, entry: &Entry, due_at: i64) {
    let task = entry.definition.task.clone();
    // Stable for the due instant: a second sweep in the same minute, or a retry
    // after the app was killed mid-run, recognises the run instead of starting a
    // second one. `begin_revision` is what enforces that.
    let request_id = format!("android-{}-{}", task.id, due_at);
    let lease = match service.store.begin_revision(
        &task.id,
        Some(&request_id),
        Some(&entry.definition.revision),
    ) {
        Ok(lease) => lease,
        Err(error) => {
            tracing::info!(%error, task = %task.id, "scheduled run skipped");
            return;
        }
    };

    match run_turn(state, &task).await {
        Ok(outcome) => {
            if let Some(session_id) = outcome.session_id.as_deref() {
                // Best effort: the run still happened, and a metadata write failure
                // must not turn a completed turn into an error.
                let _ = lease.session(session_id);
            }
            if let Err(error) = lease.finish(outcome.status, outcome.diagnostic) {
                tracing::warn!(%error, task = %task.id, "scheduled run result not recorded");
            }
        }
        Err(error) => {
            tracing::warn!(%error, task = %task.id, "scheduled run failed");
            let _ = lease.finish("error", Some(format!("{error:#}")));
        }
    }
}

struct Outcome {
    /// `"ok"` / `"error"` — the values the CLI's `run` also records.
    status: &'static str,
    session_id: Option<String>,
    diagnostic: Option<String>,
}

/// One turn, in a runtime this task owns.
///
/// A fresh session every run, deliberately: a scheduled task is a repeatable job,
/// and continuing yesterday's conversation would carry its context (and its
/// mistakes) into today's.
async fn run_turn(state: &AppState, task: &ScheduleTask) -> anyhow::Result<Outcome> {
    let config =
        atomcode_config::config::Config::load(&atomcode_config::config::Config::default_path())?;
    let provider = config.effective_model_selection().unwrap_or_default();
    if provider.is_empty() {
        anyhow::bail!("no provider is configured, so the task cannot start a turn");
    }
    let working_dir = std::path::PathBuf::from(&task.cwd);
    if !working_dir.is_dir() {
        anyhow::bail!(
            "working directory does not exist: {}",
            working_dir.display()
        );
    }

    let mut cfg = crate::live_api::live_runtime_config(
        &config,
        &provider,
        &working_dir,
        state.telemetry.clone(),
    );
    // Unattended, and never the interactive user's mode: `live_runtime_config`
    // parks approval requests forever because a webui is attached, which here would
    // hang the run.
    cfg.interactive = false;
    cfg.dangerously_skip_permissions = false;
    cfg.initial_mode = runtime_mode(&task.permission_mode);

    let (runtime, _agent) =
        crate::kernel_runtime::start_native_runtime_with_session(cfg, SessionMode::Fresh)
            .await
            .map_err(|error| anyhow::anyhow!("cannot start the runtime: {error}"))?;
    let atomcode_coding::CodingRuntime {
        handle,
        mut events,
        task: runtime_task,
        session,
        ..
    } = runtime;
    let session_id = session.as_ref().map(|session| session.id.clone());

    if let Err(error) = handle
        .wait_mcp_ready(atomcode_capabilities::mcp::CONNECT_TIMEOUT)
        .await
    {
        tear_down(handle, runtime_task).await;
        anyhow::bail!("MCP did not become ready: {error}");
    }
    if let Err(error) = handle.submit(UserInput::from(task.prompt.clone())).await {
        tear_down(handle, runtime_task).await;
        anyhow::bail!("cannot submit the task prompt: {error}");
    }

    let mut denied = 0usize;
    // A run only becomes `ok` when a terminal event says so. Starting from the
    // failure value matters: if the event stream closes early (the runtime task died,
    // the channel was dropped) the loop below simply ends, and a schedule run that
    // never reported an outcome must not be recorded as a success.
    let mut status = "error";
    let mut diagnostic: Option<String> =
        Some("the turn ended without a terminal event".to_string());

    while let Some(envelope) = events.recv().await {
        match envelope.event {
            CodingRuntimeEvent::Request(request) => {
                // No human is present. Deny and count rather than park the run — the
                // CLI's strict unattended approver refuses the same requests.
                let value = if request.kind == atomcode_capabilities::tools::APPROVAL_KIND {
                    denied += 1;
                    serde_json::to_value(atomcode_capabilities::tools::ApprovalResponse::deny())
                        .unwrap_or(serde_json::Value::Null)
                } else {
                    // A question (`request_user_input`) has no unattended answer
                    // either; `null` is the runtime's "declined" value.
                    serde_json::Value::Null
                };
                let _ = handle.respond(request.id, value).await;
            }
            CodingRuntimeEvent::TurnFinished(completion) => {
                // `Completed` only means the runtime produced a final snapshot — the
                // reason is what says whether the work actually finished. A provider
                // error, a tripped guard or a timeout all arrive here too, so
                // classifying on the variant alone recorded failures as successes.
                // The classification is the one the notification already uses, so a
                // scheduled run and the turn it reports cannot disagree.
                let (reason, snapshot_error) = match completion {
                    TurnCompletion::Completed { reason, .. } => (reason, None),
                    TurnCompletion::SnapshotUnavailable { reason, error, .. } => {
                        (reason, Some(error.message))
                    }
                };
                status = match crate::turn_outcome::TurnOutcome::of(reason) {
                    crate::turn_outcome::TurnOutcome::Ok => "ok",
                    crate::turn_outcome::TurnOutcome::Cancelled => "cancelled",
                    crate::turn_outcome::TurnOutcome::Failed => "error",
                };
                diagnostic = snapshot_error
                    .map(|message| format!("snapshot unavailable: {message}"))
                    .or_else(|| {
                        // A failed turn earns a diagnostic even when the snapshot was
                        // fine: "error" with no reason is not a useful ledger entry.
                        (status != "ok").then(|| format!("turn ended: {}", stop_reason_name(reason)))
                    });
                break;
            }
            _ => {}
        }
    }

    tear_down(handle, runtime_task).await;

    if status == "ok" && denied > 0 {
        status = "error";
        diagnostic = Some(format!(
            "{denied} approval request(s) denied: no one is present to approve a scheduled run"
        ));
    }
    Ok(Outcome {
        status,
        session_id,
        diagnostic,
    })
}

fn runtime_mode(permission_mode: &str) -> RuntimeMode {
    match permission_mode {
        "auto" => RuntimeMode::Auto,
        "accept_edits" => RuntimeMode::AcceptEdits,
        // `plan` and anything unrecognised: the most restrictive mode, because a
        // stored value this build cannot read is not permission to do more.
        _ => RuntimeMode::Plan,
    }
}

/// Stop the runtime and join its task.
///
/// Best effort: the run's outcome is already decided by this point, and a
/// shutdown error must not rewrite what happened.
async fn tear_down<T>(
    handle: atomcode_coding::CodingRuntimeHandle,
    runtime_task: tokio::task::JoinHandle<T>,
) {
    let _ = handle.shutdown().await;
    let _ = runtime_task.await;
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// The wire name of a stop reason (`provider_error`, `stopped`, …).
///
/// `StopReason` serializes by variant name, which is also what the session log and
/// `/health.last_turns` use — so a run ledger entry, the log and the notification
/// all name the same cause the same way.
fn stop_reason_name(reason: atomcode_kernel::event::StopReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}
