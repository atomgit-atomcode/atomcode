use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use std::sync::atomic::{AtomicBool, Ordering};

use atomcode_auth as auth;
use atomcode_codingplan as coding_plan;
use atomcode_telemetry::{CodingplanErrorKind, CodingplanResult, Event, SessionMode};

use crate::{
    api_auth::{pending_invite_for_login, poll_login_session, LoginPollStep},
    api_config::{config_response, load_config, update_config},
    daemon_scope, json_error, AppState,
};

// ============================================================================
// Request/Response DTOs
// ============================================================================

#[derive(Debug, Deserialize)]
pub(crate) struct CodingPlanSetupRequest {
    pub login_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodingPlanSetupResponse {
    success: bool,
    report_text: String,
    default_provider: String,
    providers: Vec<crate::ProviderInfo>,
    steps: SetupSteps,
}

#[derive(Debug, Serialize)]
struct SetupSteps {
    login: StepInfo,
    claim: StepInfo,
    models: StepInfo,
    status: StepInfo,
}

#[derive(Debug, Serialize)]
struct StepInfo {
    status: String,
    message: String,
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /codingplan/setup - Runs CodingPlan provider setup.
pub(crate) async fn codingplan_setup(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<atomcode_telemetry::SessionMode>,
    Json(req): Json<CodingPlanSetupRequest>,
) -> impl IntoResponse {
    let state_clone = state.clone();
    daemon_scope(&state, None, client_mode, || async move {
        let state = state_clone;
        // Check if already logged in
        let is_logged_in = tokio::task::spawn_blocking(|| auth::get_valid_token().is_ok())
            .await
            .unwrap_or(false);

        if !is_logged_in {
            // Not logged in — check if a login_id was provided
            match req.login_id {
                None => {
                    state.telemetry.track(Event::TakeCodingplan {
                        type_: CodingplanResult::Fail,
                        error_kind: Some(CodingplanErrorKind::AuthError),
                        error_data: Some(serde_json::json!({
                            "step": "login",
                            "message": "Not logged in. Call /auth/login/start first.",
                        }).to_string()),
                    });
                    return json_error(
                        StatusCode::UNAUTHORIZED,
                        "Not logged in. Call /auth/login/start first.",
                    )
                    .into_response();
                }
                Some(login_id) => {
                    match poll_login_session(&state, &login_id).await {
                        Ok(result) => match result.step {
                            LoginPollStep::Authorized {
                                user,
                                newly_authorized,
                            } => {
                                state.telemetry.set_account_id(Some(user.id.clone()));
                                if newly_authorized {
                                    let (invite_code, install_uuid) = pending_invite_for_login();
                                    let event = Event::LoginSuccess {
                                        invite_code,
                                        install_uuid,
                                    };
                                    if let Err(e) =
                                        state.telemetry.track_durable(event.clone()).await
                                    {
                                        tracing::warn!(
                                            ?e,
                                            "login_success durable enqueue failed; falling back to async telemetry"
                                        );
                                        state.telemetry.track(event);
                                    }
                                }
                            }
                            step => {
                                let (status, message) = match step {
                                    LoginPollStep::Pending => (
                                        StatusCode::CONFLICT,
                                        "Login still pending. Poll the login endpoint until authorized."
                                            .to_string(),
                                    ),
                                    LoginPollStep::Expired => (
                                        StatusCode::GONE,
                                        "Login session expired".to_string(),
                                    ),
                                    LoginPollStep::Cancelled => (
                                        StatusCode::GONE,
                                        "Login session was cancelled".to_string(),
                                    ),
                                    LoginPollStep::Failed { message, .. } => {
                                        (StatusCode::INTERNAL_SERVER_ERROR, message)
                                    }
                                    LoginPollStep::Retryable { message, .. } => {
                                        (StatusCode::SERVICE_UNAVAILABLE, message)
                                    }
                                    LoginPollStep::Authorized { .. } => unreachable!(),
                                };
                                state.telemetry.track(Event::TakeCodingplan {
                                    type_: CodingplanResult::Fail,
                                    error_kind: Some(CodingplanErrorKind::AuthError),
                                    error_data: Some(
                                        serde_json::json!({
                                            "step": "login",
                                            "message": message,
                                        })
                                        .to_string(),
                                    ),
                                });
                                return json_error(status, message).into_response();
                            }
                        },
                        Err(error) => {
                            let message = error.message;
                            state.telemetry.track(Event::TakeCodingplan {
                                type_: CodingplanResult::Fail,
                                error_kind: Some(CodingplanErrorKind::AuthError),
                                error_data: Some(serde_json::json!({
                                    "step": "login",
                                    "message": message,
                                }).to_string()),
                            });
                            return json_error(error.status, message).into_response();
                        }
                    }
                }
            }
        }

        // At this point, the user is logged in. Run CodingPlan setup.
        let mut config = match load_config() {
            Ok(c) => c,
            Err(e) => {
                state.telemetry.track(Event::TakeCodingplan {
                    type_: CodingplanResult::Fail,
                    error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                    error_data: Some(serde_json::json!({
                        "step": "config_save",
                        "message": e,
                    }).to_string()),
                });
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
            }
        };

        // coding_plan::setup::run uses blocking HTTP internally; keep it off
        // the async runtime worker threads.
        let setup_result = tokio::task::spawn_blocking(move || {
            // step_login will see is_logged_in() == true and skip.
            // Pass None for tel — we emit TakeCodingplan externally in this handler.
            let report = coding_plan::run(&mut config, None)?;
            Ok::<_, anyhow::Error>((config, report))
        })
        .await;

        let (mut config, report) = match setup_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                state.telemetry.track(Event::TakeCodingplan {
                    type_: CodingplanResult::Fail,
                    error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                    error_data: Some(serde_json::json!({
                        "step": "claim",
                        "message": format!("CodingPlan setup failed: {:#}", e),
                    }).to_string()),
                });
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("CodingPlan setup failed: {:#}", e),
                )
                .into_response();
            }
            Err(e) => {
                state.telemetry.track(Event::TakeCodingplan {
                    type_: CodingplanResult::Fail,
                    error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                    error_data: Some(serde_json::json!({
                        "step": "claim",
                        "message": format!("CodingPlan setup task failed: {:#}", e),
                    }).to_string()),
                });
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("CodingPlan setup task failed: {:#}", e),
                )
                .into_response();
            }
        };

        // Determine result type based on report
        let result_type = if report.should_persist_config() {
            CodingplanResult::Success
        } else {
            CodingplanResult::Fail
        };

        // Persist config if setup succeeded
        if report.should_persist_config() {
            config = match update_config(|latest| {
                coding_plan::merge_successful_config(latest, &config, &report)
            }) {
                Ok(config) => config,
                Err(e) => {
                    state.telemetry.track(Event::TakeCodingplan {
                        type_: result_type,
                        error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                        error_data: Some(serde_json::json!({
                            "step": "config_save",
                            "message": e,
                        }).to_string()),
                    });
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
                }
            };
            if let Err(e) = coding_plan::write_last_sync_now() {
                state.telemetry.track(Event::TakeCodingplan {
                    type_: result_type,
                    error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                    error_data: Some(serde_json::json!({
                        "step": "sync_marker",
                        "message": format!("Failed to write CodingPlan sync marker: {:#}", e),
                    }).to_string()),
                });
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to write CodingPlan sync marker: {:#}", e),
                )
                .into_response();
            }
        }

        // Emit TakeCodingplan exactly once on the success path
        state.telemetry.track(Event::TakeCodingplan {
            type_: result_type,
            error_kind: None,
            error_data: if result_type == CodingplanResult::Success {
                Some(serde_json::json!({
                    "step": null,
                }).to_string())
            } else {
                None
            },
        });

        // Build response
        let report_text = report.render();
        let steps = SetupSteps {
            login: step_info_from_result(&report.login),
            claim: step_info_from_result(&report.claim),
            models: step_info_from_result(&report.models),
            status: step_info_from_result(&report.status),
        };

        let config_resp = config_response(&config);
        Json(CodingPlanSetupResponse {
            success: report.should_persist_config(),
            report_text,
            default_provider: config_resp.default_provider,
            providers: config_resp.providers,
            steps,
        })
        .into_response()
    })
    .await
}

/// Convert a StepResult to a StepInfo for JSON serialization.
fn step_info_from_result<T: std::fmt::Debug>(result: &coding_plan::StepResult<T>) -> StepInfo {
    match result {
        coding_plan::StepResult::Ok(_) => StepInfo {
            status: "ok".to_string(),
            message: String::new(),
        },
        coding_plan::StepResult::Skipped(msg) => StepInfo {
            status: "skipped".to_string(),
            message: msg.clone(),
        },
        coding_plan::StepResult::Err(msg) => StepInfo {
            status: "error".to_string(),
            message: msg.clone(),
        },
    }
}

/// Single-flight guard so concurrent logins (VS Code + JetBrains at once)
/// don't fire duplicate background syncs.
static AUTO_SYNC_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Background CodingPlan model sync triggered right after a successful OAuth
/// login (newly authorized).
///
/// Login alone only persists the token; the model list served by `/models`
/// comes from the local config, which is populated by the CodingPlan claim +
/// models-v2 steps. Without this, both IDE plugins show "signed in" but an
/// empty model picker until the user manually runs `/codingplan` / clicks
/// "Sync CodingPlan models". Triggering the sync here in the daemon means both
/// VS Code and JetBrains pick up the models via their usual `/models` refresh
/// (and config-file watch), with zero plugin changes.
///
/// Deliberately fire-and-forget: the login poll response must not wait for the
/// claim/models network round-trips. Failures are logged / telemetry-tracked
/// but never fail the login itself.
pub(crate) fn sync_codingplan_after_login(state: AppState, client_mode: SessionMode) {
    if AUTO_SYNC_IN_FLIGHT
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::debug!("codingplan auto-sync already in flight; skipping");
        return;
    }
    let state_for_scope = state.clone();
    tokio::spawn(async move {
        let _reset = AutoSyncReset;
        daemon_scope(&state, None, client_mode, || async move {
            let mut config = match load_config() {
                Ok(c) => c,
                Err(e) => {
                    state_for_scope.telemetry.track(Event::TakeCodingplan {
                        type_: CodingplanResult::Fail,
                        error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                        error_data: Some(
                            serde_json::json!({
                                "step": "config_load",
                                "message": e,
                            })
                            .to_string(),
                        ),
                    });
                    tracing::warn!(error = %e, "codingplan auto-sync: config load failed");
                    return;
                }
            };

            let setup_result = tokio::task::spawn_blocking(move || {
                let report = coding_plan::run(&mut config, None)?;
                Ok::<_, anyhow::Error>((config, report))
            })
            .await;

            let (config, report) = match setup_result {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    state_for_scope.telemetry.track(Event::TakeCodingplan {
                        type_: CodingplanResult::Fail,
                        error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                        error_data: Some(
                            serde_json::json!({
                                "step": "claim",
                                "message": format!("CodingPlan auto-sync failed: {:#}", e),
                            })
                            .to_string(),
                        ),
                    });
                    tracing::warn!(error = ?e, "codingplan auto-sync after login failed");
                    return;
                }
                Err(e) => {
                    tracing::error!(error = ?e, "codingplan auto-sync task panicked");
                    return;
                }
            };

            if !report.should_persist_config() {
                // e.g. claim refused / empty model list — leave existing
                // config untouched; the user can still set up providers
                // manually.
                tracing::info!(
                    report = %report.render(),
                    "codingplan auto-sync after login did not persist config"
                );
                return;
            }

            if let Err(e) = update_config(|latest| {
                coding_plan::merge_successful_config(latest, &config, &report)
            }) {
                state_for_scope.telemetry.track(Event::TakeCodingplan {
                    type_: CodingplanResult::Fail,
                    error_kind: Some(CodingplanErrorKind::ExecutionFailed),
                    error_data: Some(
                        serde_json::json!({
                            "step": "config_save",
                            "message": e,
                        })
                        .to_string(),
                    ),
                });
                tracing::warn!(error = %e, "codingplan auto-sync: config merge failed");
                return;
            }
            if let Err(e) = coding_plan::write_last_sync_now() {
                tracing::warn!(error = ?e, "codingplan auto-sync: sync marker write failed");
            }

            state_for_scope.telemetry.track(Event::TakeCodingplan {
                type_: CodingplanResult::Success,
                error_kind: None,
                error_data: Some(serde_json::json!({ "step": null }).to_string()),
            });
            tracing::info!("codingplan auto-sync after login completed");
        })
        .await;
    });
}

/// Resets the single-flight flag when the spawned sync task finishes.
struct AutoSyncReset;
impl Drop for AutoSyncReset {
    fn drop(&mut self) {
        AUTO_SYNC_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}
