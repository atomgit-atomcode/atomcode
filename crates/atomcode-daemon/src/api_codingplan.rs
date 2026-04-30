use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use atomcode_core::auth;
use atomcode_core::coding_plan;

use crate::{
    api_auth::{poll_login_session, LoginPollStep},
    api_config::{cleanup_expired_sessions, config_response, load_config, save_config},
    json_error, AppState,
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
    Json(req): Json<CodingPlanSetupRequest>,
) -> impl IntoResponse {
    // Clean up expired sessions
    cleanup_expired_sessions(&state.login_sessions).await;

    // Check if already logged in
    let is_logged_in = auth::get_stored_auth().is_some();

    if !is_logged_in {
        // Not logged in — check if a login_id was provided
        match req.login_id {
            None => {
                return json_error(
                    StatusCode::UNAUTHORIZED,
                    "Not logged in. Call /auth/login/start first.",
                )
                .into_response()
            }
            Some(login_id) => {
                match poll_login_session(&state, &login_id).await {
                    Ok(LoginPollStep::Pending) => {
                        return (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({
                                "success": false,
                                "status": "login_pending",
                                "error": "Login still pending. Poll /auth/login/:login_id/poll until authorized."
                            })),
                        )
                            .into_response()
                    }
                    Ok(LoginPollStep::Authorized(_)) => {}
                    Err((status, message)) => return json_error(status, message).into_response(),
                }
            }
        }
    }

    // At this point, the user is logged in. Run CodingPlan setup.
    let mut config = match load_config() {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    // coding_plan::setup::run uses blocking HTTP internally; keep it off
    // the async runtime worker threads.
    let setup_result = tokio::task::spawn_blocking(move || {
        // step_login will see is_logged_in() == true and skip.
        let report = coding_plan::run(&mut config, None)?;
        Ok::<_, anyhow::Error>((config, report))
    })
    .await;

    let (config, report) = match setup_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("CodingPlan setup failed: {:#}", e),
            )
            .into_response()
        }
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("CodingPlan setup task failed: {:#}", e),
            )
            .into_response()
        }
    };

    // Persist config if setup succeeded
    if report.should_persist_config() {
        if let Err(e) = save_config(&config) {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        if let Err(e) = coding_plan::write_last_sync_now() {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write CodingPlan sync marker: {:#}", e),
            )
            .into_response();
        }
    }

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
