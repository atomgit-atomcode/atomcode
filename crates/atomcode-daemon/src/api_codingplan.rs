use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
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

#[derive(Debug, Serialize)]
struct CodingPlanUsageSummaryResponse {
    schema_version: u8,
    available: bool,
    plan: Option<CodingPlanPlanResponse>,
    primary_window: Option<CodingPlanQuotaWindowResponse>,
    windows: Vec<CodingPlanQuotaWindowResponse>,
    quota_hint: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodingPlanPlanResponse {
    name: String,
    status: i32,
    claimed_at: Option<String>,
    expires_at: Option<String>,
    total_days: i32,
    remaining_days: i32,
}

#[derive(Clone, Debug, Serialize)]
struct CodingPlanQuotaWindowResponse {
    metric: &'static str,
    window_hours: i32,
    window_size_seconds: i64,
    limit: Option<i64>,
    used: Option<i64>,
    remaining: Option<i64>,
    usage_percent: f64,
    remaining_percent: f64,
    quota_exhausted: bool,
    next_reset_at: Option<String>,
    next_reset_display: Option<String>,
    seconds_until_reset: i64,
    reset_label: Option<String>,
    usage_description: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodingPlanDailyUsageResponse {
    schema_version: u8,
    available: bool,
    days: u32,
    start_date: String,
    end_date: String,
    models: Vec<String>,
    rows: Vec<CodingPlanDailyUsageRowResponse>,
    model_tokens: HashMap<String, u64>,
    model_requests: HashMap<String, u64>,
    total_tokens: u64,
    total_requests: u64,
}

#[derive(Debug, Serialize)]
struct CodingPlanDailyUsageRowResponse {
    date: String,
    model_tokens: HashMap<String, u64>,
    model_requests: HashMap<String, u64>,
    total_tokens: u64,
    total_requests: u64,
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /codingplan/usage/summary - Returns the current CodingPlan entitlement
/// and quota window. This is deliberately independent from the coding runtime:
/// account-wide usage must not mutate or depend on the active session.
pub(crate) async fn codingplan_usage_summary() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let client = coding_plan::Client::from_stored_auth()?;
        client.status_v2()
    })
    .await;

    match result {
        Ok(Ok(status)) => Json(build_usage_summary(status)).into_response(),
        Ok(Err(error)) => codingplan_usage_error("summary", error),
        Err(error) => {
            tracing::error!(?error, "codingplan usage summary task failed");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to load CodingPlan usage",
            )
            .into_response()
        }
    }
}

/// GET /codingplan/usage/daily - Returns the existing 60-day, account-wide
/// usage series used by the TUI's `/usage` view.
pub(crate) async fn codingplan_usage_daily() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let client = coding_plan::Client::from_stored_auth()?;
        client.usage()
    })
    .await;

    match result {
        Ok(Ok(usage)) => Json(CodingPlanDailyUsageResponse {
            schema_version: 1,
            available: true,
            days: usage.days,
            start_date: usage.start_date,
            end_date: usage.end_date,
            models: usage.models,
            rows: usage
                .rows
                .into_iter()
                .map(|row| CodingPlanDailyUsageRowResponse {
                    date: row.date,
                    model_tokens: row.model_tokens,
                    model_requests: row.model_counts,
                    total_tokens: row.total_tokens,
                    total_requests: row.total_counts,
                })
                .collect(),
            model_tokens: usage.model_tokens,
            model_requests: usage.model_counts,
            total_tokens: usage.total_tokens,
            total_requests: usage.total_counts,
        })
        .into_response(),
        Ok(Err(error)) => codingplan_usage_error("daily", error),
        Err(error) => {
            tracing::error!(?error, "codingplan daily usage task failed");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to load CodingPlan usage",
            )
            .into_response()
        }
    }
}

fn build_usage_summary(status: coding_plan::StatusResponse) -> CodingPlanUsageSummaryResponse {
    let plan = status.codingplan_free.map(|plan| CodingPlanPlanResponse {
        name: plan.plan_name,
        status: plan.status,
        claimed_at: non_empty(plan.claimed_at),
        expires_at: non_empty(plan.expires_at),
        total_days: plan.total_days.max(0),
        remaining_days: plan.remaining_days.max(0),
    });

    let mut windows: Vec<CodingPlanQuotaWindowResponse> = status
        .rate_limit_windows
        .into_iter()
        .filter(|window| window.show_enable == 1)
        .map(|window| {
            let limit = positive_value(window.call_limit);
            let used = non_negative_value(window.calls_used);
            let usage_percent =
                normalized_percentage(window.usage_percent, window.calls_used, window.call_limit);
            CodingPlanQuotaWindowResponse {
                metric: "calls",
                window_hours: window.window_hours.max(0),
                window_size_seconds: window.window_size_seconds.max(0),
                limit,
                used,
                remaining: remaining_value(limit, used),
                usage_percent,
                remaining_percent: (100.0 - usage_percent).clamp(0.0, 100.0),
                quota_exhausted: window.quota_exhausted,
                next_reset_at: non_empty(window.reset_at),
                next_reset_display: non_empty(window.reset_at_display),
                seconds_until_reset: window.seconds_until_reset.max(0),
                reset_label: non_empty(window.reset_label),
                usage_description: non_empty(window.usage_status_desc),
            }
        })
        .collect();
    windows.sort_by_key(|window| {
        if window.window_hours > 0 {
            window.window_hours
        } else {
            i32::MAX
        }
    });

    let legacy_window = status.current_usage.map(|usage| {
        let limit = positive_value(usage.window_token_limit);
        let used = non_negative_value(usage.window_tokens_used);
        let usage_percent = normalized_percentage(
            usage.usage_percent,
            usage.window_tokens_used,
            usage.window_token_limit,
        );
        CodingPlanQuotaWindowResponse {
            metric: "tokens",
            window_hours: usage.window_hours.max(0),
            window_size_seconds: i64::from(usage.window_hours.max(0)) * 3600,
            limit,
            used,
            remaining: remaining_value(limit, used),
            usage_percent,
            remaining_percent: (100.0 - usage_percent).clamp(0.0, 100.0),
            quota_exhausted: status.window_quota_exhausted,
            next_reset_at: non_empty(usage.reset_at),
            next_reset_display: non_empty(usage.reset_at_display),
            seconds_until_reset: usage.seconds_until_reset.max(0),
            reset_label: non_empty(usage.reset_label),
            usage_description: non_empty(usage.usage_status_desc),
        }
    });

    // Match the TUI: the shortest positive visible rate-limit window is the
    // primary window. The legacy token window is only used when the newer
    // server schema did not provide a visible positive window.
    let primary_window = windows
        .iter()
        .find(|window| window.window_hours > 0)
        .cloned()
        .or(legacy_window);
    let available = plan.is_some() || primary_window.is_some() || !windows.is_empty();

    CodingPlanUsageSummaryResponse {
        schema_version: 1,
        available,
        plan,
        primary_window,
        windows,
        quota_hint: status.window_quota_hint.and_then(non_empty),
    }
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn positive_value(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

fn non_negative_value(value: i64) -> Option<i64> {
    (value >= 0).then_some(value)
}

fn remaining_value(limit: Option<i64>, used: Option<i64>) -> Option<i64> {
    Some((limit? - used?).max(0))
}

fn normalized_percentage(server_value: f64, used: i64, limit: i64) -> f64 {
    let value = if server_value.is_finite() && server_value > 0.0 {
        server_value
    } else if limit > 0 {
        used.max(0) as f64 * 100.0 / limit as f64
    } else {
        0.0
    };
    value.clamp(0.0, 100.0)
}

fn codingplan_usage_error(context: &'static str, error: anyhow::Error) -> axum::response::Response {
    let unauthorized = coding_plan::is_auth_expired(&error)
        || error
            .to_string()
            .to_ascii_lowercase()
            .contains("not logged in");
    tracing::warn!(context, ?error, "codingplan usage query failed");
    if unauthorized {
        json_error(
            StatusCode::UNAUTHORIZED,
            "CodingPlan account is not logged in",
        )
        .into_response()
    } else {
        json_error(StatusCode::BAD_GATEWAY, "Unable to load CodingPlan usage").into_response()
    }
}

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
            // Background / cross-client sync: preserve the model this client is on
            // (never clobber another client's selection — see a63f6591).
            let report = coding_plan::run(
                &mut config,
                None,
                coding_plan::DefaultModelPolicy::PreservePrevious,
            )?;
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
                coding_plan::merge_successful_config(
                    latest,
                    &config,
                    &report,
                    coding_plan::DefaultModelPolicy::PreservePrevious,
                )
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
                // Background / cross-client sync: preserve the model this client is on
                // (never clobber another client's selection — see a63f6591).
                let report = coding_plan::run(
                    &mut config,
                    None,
                    coding_plan::DefaultModelPolicy::PreservePrevious,
                )?;
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
                coding_plan::merge_successful_config(
                    latest,
                    &config,
                    &report,
                    coding_plan::DefaultModelPolicy::PreservePrevious,
                )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_summary_prefers_shortest_visible_rate_window() {
        let status: coding_plan::StatusResponse = serde_json::from_value(serde_json::json!({
            "codingplan_free": {
                "plan_name": "CodingPlan Pro",
                "status": 1,
                "remaining_days": 18,
                "total_days": 30
            },
            "rate_limit_windows": [
                {
                    "show_enable": 1,
                    "window_hours": 720,
                    "window_size_seconds": 2592000,
                    "call_limit": 1000,
                    "calls_used": 400,
                    "usage_percent": 40
                },
                {
                    "show_enable": 0,
                    "window_hours": 1,
                    "call_limit": 5,
                    "calls_used": 5
                },
                {
                    "show_enable": 1,
                    "window_hours": 5,
                    "window_size_seconds": 18000,
                    "call_limit": 100,
                    "calls_used": 25,
                    "reset_label": "2 小时后"
                }
            ]
        }))
        .expect("status response");

        let summary = build_usage_summary(status);
        assert!(summary.available);
        assert_eq!(summary.windows.len(), 2);
        let primary = summary.primary_window.expect("primary window");
        assert_eq!(primary.metric, "calls");
        assert_eq!(primary.window_hours, 5);
        assert_eq!(primary.limit, Some(100));
        assert_eq!(primary.used, Some(25));
        assert_eq!(primary.remaining, Some(75));
        assert_eq!(primary.usage_percent, 25.0);
    }

    #[test]
    fn usage_summary_keeps_legacy_token_window_compatible() {
        let status: coding_plan::StatusResponse = serde_json::from_value(serde_json::json!({
            "current_usage": {
                "window_token_limit": 50000,
                "window_tokens_used": 12500,
                "window_hours": 1,
                "reset_at_display": "14:30"
            },
            "window_quota_exhausted": false
        }))
        .expect("legacy status response");

        let summary = build_usage_summary(status);
        let primary = summary.primary_window.expect("legacy primary window");
        assert_eq!(primary.metric, "tokens");
        assert_eq!(primary.remaining, Some(37500));
        assert_eq!(primary.usage_percent, 25.0);
        assert_eq!(primary.next_reset_display.as_deref(), Some("14:30"));
    }

    #[test]
    fn usage_summary_clamps_invalid_gateway_values() {
        assert_eq!(normalized_percentage(150.0, 0, 100), 100.0);
        assert_eq!(normalized_percentage(f64::NAN, 10, 100), 10.0);
        assert_eq!(remaining_value(Some(10), Some(20)), Some(0));
    }
}
