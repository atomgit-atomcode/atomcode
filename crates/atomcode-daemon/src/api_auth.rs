use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use atomcode_config::config::Config;
use atomcode_auth as auth;
use atomcode_telemetry::Event;

use crate::{api_config::cleanup_expired_sessions, json_error, AppState, LoginSessionEntry};

pub(crate) enum LoginPollStep {
    Pending,
    Authorized(auth::UserInfo),
}

enum BlockingLoginPollStep {
    Pending(LoginSessionEntry),
    Authorized(auth::UserInfo),
}

pub(crate) fn pending_invite_for_login() -> (Option<String>, Option<uuid::Uuid>) {
    match atomcode_telemetry::pending_invite::load(&Config::config_dir()) {
        Some(invite) => (Some(invite.invite_code), Some(invite.install_uuid)),
        None => (None, None),
    }
}

// ============================================================================
// Response DTOs
// ============================================================================

#[derive(Debug, Serialize)]
struct AuthStatusResponse {
    logged_in: bool,
    /// Credentials exist on disk but the token can't be made valid (expired
    /// and the refresh_token was refused / absent). The sidebar used to show
    /// "logged in" here while chat returned "登录已过期" — this flag lets the
    /// frontend surface a distinct "session expired, re-login" state instead.
    expired: bool,
    auth_path: String,
    user: Option<auth::UserInfo>,
    token: Option<TokenInfo>,
}

/// Classify the reported auth state from what disk holds (`present`) and
/// whether that stored token is actually usable (`token_usable` — valid now
/// or successfully refreshed). `expired` means present-but-dead: the exact
/// mismatch that made the sidebar disagree with chat.
fn classify_auth_status(present: bool, token_usable: bool) -> (bool, bool) {
    let logged_in = present;
    let expired = present && !token_usable;
    (logged_in, expired)
}

#[derive(Debug, Serialize)]
struct TokenInfo {
    token_type: String,
    expires_in: Option<i64>,
    created_at: i64,
    has_refresh_token: bool,
}

#[derive(Debug, Serialize)]
struct LoginStartResponse {
    login_id: String,
    url: String,
    expires_in_seconds: u64,
}

#[derive(Debug, Serialize)]
struct LoginPollResponse {
    status: String,
    user: Option<auth::UserInfo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginStartRequest {
    #[serde(default = "default_true")]
    open_browser: bool,
}

fn default_true() -> bool {
    true
}

// ============================================================================
// Handlers
// ============================================================================

/// GET /auth/status - Returns whether the user is signed in.
pub(crate) async fn auth_status() -> impl IntoResponse {
    let auth_path = auth::auth_file_path();
    let auth_path_str = auth_path.to_string_lossy().to_string();

    match auth::get_stored_auth() {
        Some(info) => {
            let has_refresh = info.refresh_token.is_some();
            // Presence of auth.toml is not the same as a usable session: an
            // expired token whose refresh_token is dead/absent still parses
            // fine but every chat turn 401s. Probe real usability the same
            // way chat does — `get_valid_token` returns the stored token when
            // valid, refreshes (and saves) near expiry, and errors when it
            // can't be made valid. It's disk-only in the common case and
            // network-bounded (5s connect / 10s total) only when a refresh is
            // actually attempted; run it off the async runtime.
            let token_usable = tokio::task::spawn_blocking(|| auth::get_valid_token().is_ok())
                .await
                .unwrap_or(false);
            let (logged_in, expired) = classify_auth_status(true, token_usable);
            Json(AuthStatusResponse {
                logged_in,
                expired,
                auth_path: auth_path_str,
                user: Some(info.user),
                token: Some(TokenInfo {
                    token_type: info.token_type,
                    expires_in: info.expires_in,
                    created_at: info.created_at,
                    has_refresh_token: has_refresh,
                }),
            })
            .into_response()
        }
        None => Json(AuthStatusResponse {
            logged_in: false,
            expired: false,
            auth_path: auth_path_str,
            user: None,
            token: None,
        })
        .into_response(),
    }
}

/// POST /auth/login/start - Starts OAuth login and returns URL + login_id.
pub(crate) async fn auth_login_start(
    State(state): State<AppState>,
    Json(req): Json<LoginStartRequest>,
) -> impl IntoResponse {
    // Clean up expired sessions first
    cleanup_expired_sessions(&state.login_sessions).await;

    let start_result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(auth::LoginSession, String)> {
            let session = auth::start_login()?;
            let url = session.url().to_string();
            if req.open_browser {
                session.open_browser_best_effort();
            }
            Ok((session, url))
        })
        .await;

    let (session, url) = match start_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to start login: {:#}", e),
            )
            .into_response()
        }
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Login task failed: {:#}", e),
            )
            .into_response()
        }
    };

    let login_id = uuid::Uuid::new_v4().to_string();
    let entry = LoginSessionEntry {
        session,
        created_at: std::time::Instant::now(),
    };

    state
        .login_sessions
        .write()
        .await
        .insert(login_id.clone(), entry);

    Json(LoginStartResponse {
        login_id,
        url,
        expires_in_seconds: 600,
    })
    .into_response()
}

/// POST /auth/login/:login_id/poll - Polls one OAuth login session.
pub(crate) async fn auth_login_poll(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<atomcode_telemetry::SessionMode>,
    Path(login_id): Path<String>,
) -> impl IntoResponse {
    let state_inner = state.clone();
    crate::telemetry_scope::daemon_scope(&state, None, client_mode, || async move {
        match poll_login_session(&state_inner, &login_id).await {
            Ok(LoginPollStep::Pending) => Json(LoginPollResponse {
                status: "pending".to_string(),
                user: None,
            })
            .into_response(),
            Ok(LoginPollStep::Authorized(user)) => {
                state_inner
                    .telemetry
                    .set_account_id(Some(user.id.to_string()));
                let (invite_code, install_uuid) = pending_invite_for_login();
                let event = Event::LoginSuccess {
                    invite_code,
                    install_uuid,
                };
                if let Err(e) = state_inner.telemetry.track_durable(event.clone()).await {
                    tracing::warn!(
                        ?e,
                        "login_success durable enqueue failed; falling back to async telemetry"
                    );
                    state_inner.telemetry.track(event);
                }
                Json(LoginPollResponse {
                    status: "authorized".to_string(),
                    user: Some(user),
                })
                .into_response()
            }
            Err((status, message)) => json_error(status, message).into_response(),
        }
    })
    .await
}

/// DELETE /auth/login/:login_id - Cancels and removes an in-flight login session.
pub(crate) async fn auth_login_cancel(
    State(state): State<AppState>,
    Path(login_id): Path<String>,
) -> impl IntoResponse {
    let removed = state.login_sessions.write().await.remove(&login_id);
    if removed.is_some() {
        Json(serde_json::json!({"success": true})).into_response()
    } else {
        json_error(StatusCode::NOT_FOUND, "Login session not found").into_response()
    }
}

/// POST /auth/logout - Logs out (removes stored auth).
pub(crate) async fn auth_logout(
    State(state): State<AppState>,
    axum::Extension(client_mode): axum::Extension<atomcode_telemetry::SessionMode>,
) -> impl IntoResponse {
    let state_inner = state.clone();
    crate::telemetry_scope::daemon_scope(&state, None, client_mode, || async move {
        match auth::logout() {
            Ok(()) => {
                state_inner.telemetry.set_account_id(None);
                // Return auth status after logout
                let auth_path = auth::auth_file_path();
                Json(AuthStatusResponse {
                    logged_in: false,
                    expired: false,
                    auth_path: auth_path.to_string_lossy().to_string(),
                    user: None,
                    token: None,
                })
                .into_response()
            }
            Err(e) => json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Logout failed: {:#}", e),
            )
            .into_response(),
        }
    })
    .await
}

pub(crate) async fn poll_login_session(
    state: &AppState,
    login_id: &str,
) -> Result<LoginPollStep, (StatusCode, String)> {
    cleanup_expired_sessions(&state.login_sessions).await;

    let entry = {
        let mut sessions = state.login_sessions.write().await;
        let entry = sessions.remove(login_id).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "Login session not found or expired".to_string(),
            )
        })?;
        if entry.created_at.elapsed().as_secs() >= 600 {
            return Err((StatusCode::NOT_FOUND, "Login session expired".to_string()));
        }
        entry
    };

    let poll_result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<BlockingLoginPollStep> {
            match entry.session.poll_once()? {
                auth::PollOutcome::Pending => Ok(BlockingLoginPollStep::Pending(entry)),
                auth::PollOutcome::Authorized => {
                    let auth_info = entry.session.finish(None)?;
                    auth::save_auth(&auth_info)?;
                    Ok(BlockingLoginPollStep::Authorized(auth_info.user))
                }
            }
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Login task failed: {:#}", e),
            )
        })?;

    match poll_result {
        Ok(BlockingLoginPollStep::Pending(entry)) => {
            state
                .login_sessions
                .write()
                .await
                .insert(login_id.to_string(), entry);
            Ok(LoginPollStep::Pending)
        }
        Ok(BlockingLoginPollStep::Authorized(user)) => Ok(LoginPollStep::Authorized(user)),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Login poll error: {:#}", e),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The sidebar login indicator reads `/auth/status`. Before the fix it
    // reported `logged_in` purely from the presence of `auth.toml`, so an
    // expired/rejected token still showed "logged in" while chat correctly
    // returned "登录已过期". These lock the three states the classifier must
    // distinguish so the frontend can tell "present-and-usable" from
    // "present-but-dead".

    #[test]
    fn no_credentials_is_logged_out_not_expired() {
        assert_eq!(classify_auth_status(false, false), (false, false));
    }

    #[test]
    fn present_and_usable_token_is_logged_in_not_expired() {
        assert_eq!(classify_auth_status(true, true), (true, false));
    }

    #[test]
    fn present_but_unusable_token_is_expired() {
        // File on disk exists (so the user "looks" logged in) but the token
        // can't be made valid — this is the exact sidebar/chat mismatch.
        assert_eq!(classify_auth_status(true, false), (true, true));
    }
}
