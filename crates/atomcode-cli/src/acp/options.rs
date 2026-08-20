//! Session operating modes and config options: `session/set_mode` and
//! `session/set_config_option` handlers plus the mode/config catalog helpers.
//!
//! The ACP session surface exposes an execution mode (mapped to the kernel
//! [`RuntimeMode`]) and a config-option catalog (model / mode /
//! reasoning_effort selects). This module owns the wire → kernel mapping for
//! both, the shared catalog-apply logic, and the handlers that broadcast the
//! resulting `current_mode_update` / `config_option_update` notifications.

use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, CurrentModeUpdate, SessionConfigOption, SessionConfigOptionValue,
    SessionId, SessionMode, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};
use atomcode_coding::RuntimeMode;

use crate::acp::sessions::Sessions;
use crate::acp::SessionModelResolver;

// ── session modes / config option catalog helpers ────────────────────────────

/// The four kernel operating modes, advertised as ACP session modes.
pub fn session_modes() -> Vec<SessionMode> {
    [
        RuntimeMode::Build,
        RuntimeMode::AcceptEdits,
        RuntimeMode::Auto,
        RuntimeMode::Plan,
    ]
    .into_iter()
    .map(|mode| SessionMode::new(mode.wire(), mode.label()))
    .collect()
}

/// Map an ACP `SessionModeId` (the kernel wire name) back to [`RuntimeMode`].
pub fn runtime_mode_from_id(id: &str) -> Option<RuntimeMode> {
    match id {
        "build" => Some(RuntimeMode::Build),
        "accept_edits" => Some(RuntimeMode::AcceptEdits),
        "bypass" => Some(RuntimeMode::Auto),
        "plan" => Some(RuntimeMode::Plan),
        _ => None,
    }
}

/// Initial `SessionModeState`: kernel default mode (`build`) plus all modes.
pub fn session_mode_state() -> SessionModeState {
    SessionModeState::new(RuntimeMode::Build.wire(), session_modes())
}

/// Apply a client-supplied value to one option in the catalog, in place.
///
/// A `select` option accepts a `ValueId`; a `boolean` option accepts a
/// `Boolean`. Returns `true` when the `config_id` was found and the value had
/// the right shape.
pub fn apply_config_option(
    catalog: &mut [SessionConfigOption],
    config_id: &str,
    value: &SessionConfigOptionValue,
) -> bool {
    for option in catalog.iter_mut() {
        if option.id.0.as_ref() != config_id {
            continue;
        }
        use agent_client_protocol::schema::v1::{SessionConfigKind, SessionConfigSelect};
        option.kind = match (&option.kind, value) {
            (SessionConfigKind::Select(select), SessionConfigOptionValue::ValueId { value }) => {
                SessionConfigKind::Select(SessionConfigSelect::new(
                    value.clone(),
                    select.options.clone(),
                ))
            }
            (SessionConfigKind::Boolean(_), SessionConfigOptionValue::Boolean { value }) => {
                SessionConfigKind::Boolean(
                    agent_client_protocol::schema::v1::SessionConfigBoolean::new(*value),
                )
            }
            _ => return false,
        };
        return true;
    }
    false
}

// ── session/set_mode handler ─────────────────────────────────────────────────

/// Switch the runtime to the wire `mode_id` and commit the new mode to the
/// session state. Returns the applied [`RuntimeMode`]; the wire notification is
/// the caller's job ([`apply_session_mode`]).
async fn switch_runtime_mode(
    sessions: &Sessions,
    session_id: &SessionId,
    mode_id: &str,
) -> Result<RuntimeMode, AcpError> {
    let mode = runtime_mode_from_id(mode_id).ok_or_else(|| {
        AcpError::invalid_params().data(format!("unknown session mode `{mode_id}`"))
    })?;
    let runtime = {
        let map = sessions.lock().await;
        match map.get(session_id.0.as_ref()) {
            Some(state) => state.runtime.clone(),
            None => return Err(AcpError::invalid_params().data("unknown session")),
        }
    };

    runtime
        .set_mode(mode)
        .await
        .map_err(|e| AcpError::internal_error().data(format!("set mode failed: {e}")))?;

    {
        let mut map = sessions.lock().await;
        if let Some(state) = map.get_mut(session_id.0.as_ref()) {
            state.current_mode = mode;
        }
    }
    Ok(mode)
}

/// Switch the mode and broadcast `current_mode_update`.
///
/// Shared by `session/set_mode` and the `mode` session config option
/// (`handle_set_session_config_option`), so both entry points behave
/// identically. Unknown mode ids and unknown sessions are errors.
async fn apply_session_mode(
    sessions: &Sessions,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    mode_id: &str,
) -> Result<(), AcpError> {
    let mode = switch_runtime_mode(sessions, session_id, mode_id).await?;
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(mode.wire())),
    ))?;
    Ok(())
}

/// Handle a `session/set_mode` request.
///
/// Maps the ACP mode id to the kernel [`RuntimeMode`], switches the live
/// runtime, and broadcasts `current_mode_update`. See [`apply_session_mode`].
pub async fn handle_set_session_mode(
    sessions: &Sessions,
    cx: &ConnectionTo<Client>,
    req: &SetSessionModeRequest,
) -> Result<SetSessionModeResponse, AcpError> {
    apply_session_mode(sessions, cx, &req.session_id, req.mode_id.0.as_ref()).await?;
    Ok(SetSessionModeResponse::new())
}

// ── session/set_config_option handler ────────────────────────────────────────

/// The config option id that drives kernel model reloads (a `select` over the
/// configured model catalog). Session config catalogs are built by the CLI
/// entry point; ACP matches on this stable id.
pub const MODEL_CONFIG_ID: &str = "model";

/// The config option id that switches the session's execution mode (a `select`
/// over the same mode ids as [`session_modes`]). Handled by delegating to
/// [`apply_session_mode`].
pub const MODE_CONFIG_ID: &str = "mode";

/// The config option id that sets the session's reasoning effort (a `select`
/// over `off` / `high` / `max`, mirroring the TUI `/effort` command). Handled
/// by re-resolving the kernel config and reloading the provider.
pub const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";

/// Reasoning effort tiers accepted for [`REASONING_EFFORT_CONFIG_ID`]. Values
/// follow the TUI `/effort` ladder: `off` (no opinion, API default), `high`,
/// `max`. Like qwen-code, only the value's legality is checked here — whether
/// the active provider/model honours it is the adapter's call (it may clamp or
/// ignore; the kernel `ReasoningEffort` doc says an adapter MAY ignore it).
pub const REASONING_EFFORT_TIERS: [&str; 3] = ["off", "high", "max"];

/// Apply a `session/set_config_option` request to the session's catalog and
/// kernel state, without touching the wire (the caller sends the
/// notifications).
///
/// Returns the full updated catalog and, when the changed option was the
/// `mode` selector, the applied [`RuntimeMode`] so the caller can broadcast
/// `current_mode_update`. Failures leave the session state untouched.
pub(super) async fn apply_session_config_option(
    sessions: &Sessions,
    req: &SetSessionConfigOptionRequest,
    model_resolver: Option<&SessionModelResolver>,
    effort_resolver: Option<&SessionModelResolver>,
) -> Result<(Vec<SessionConfigOption>, Option<RuntimeMode>), AcpError> {
    let (runtime, cwd, mut catalog) = {
        let map = sessions.lock().await;
        match map.get(req.session_id.0.as_ref()) {
            Some(state) => (
                state.runtime.clone(),
                state.cwd.clone(),
                state.config_options.clone(),
            ),
            None => return Err(AcpError::invalid_params().data("unknown session")),
        }
    };
    if !apply_config_option(&mut catalog, req.config_id.0.as_ref(), &req.value) {
        return Err(AcpError::invalid_params().data(format!(
            "unknown session config option `{}` or value of the wrong shape",
            req.config_id.0
        )));
    }

    let switched_mode = match req.config_id.0.as_ref() {
        // Execution mode: reuse the same validation path as `session/set_mode`
        // so both entry points stay consistent.
        MODE_CONFIG_ID => {
            let Some(value_id) = req.value.as_value_id() else {
                return Err(AcpError::invalid_params().data("`mode` option requires a value id"));
            };
            Some(switch_runtime_mode(sessions, &req.session_id, value_id.0.as_ref()).await?)
        }
        // Reasoning effort: only the value's legality is checked (qwen-code
        // semantics); whether the active provider honours it is the adapter's
        // choice. Re-resolve the kernel config so the effort lands on the wire.
        REASONING_EFFORT_CONFIG_ID => {
            let Some(value_id) = req.value.as_value_id() else {
                return Err(AcpError::invalid_params()
                    .data("`reasoning_effort` option requires a value id"));
            };
            let effort = value_id.0.as_ref();
            if !REASONING_EFFORT_TIERS.contains(&effort) {
                return Err(AcpError::invalid_params().data(format!(
                    "unknown reasoning effort `{effort}`; choose one of: {}",
                    REASONING_EFFORT_TIERS.join(", ")
                )));
            }
            let Some(resolved) = effort_resolver.and_then(|resolve| resolve(effort)) else {
                return Err(AcpError::internal_error().data(format!(
                    "reasoning effort `{effort}` cannot be applied to this session"
                )));
            };
            let mut next = resolved;
            // Keep this session's working directory authoritative.
            next.working_dir = cwd;
            runtime.reprepare_config(next).await.map_err(|e| {
                AcpError::internal_error().data(format!("reasoning effort reload failed: {e}"))
            })?;
            None
        }
        // Model selector: reload the kernel provider from a freshly resolved
        // config so subsequent turns run on the selected model.
        MODEL_CONFIG_ID => {
            if let Some(value_id) = req.value.as_value_id() {
                let value_id = value_id.0.as_ref().to_string();
                let Some(resolved) = model_resolver.and_then(|resolve| resolve(&value_id)) else {
                    return Err(AcpError::internal_error().data(format!(
                        "model `{value_id}` is not available for session reload"
                    )));
                };
                let mut next = resolved;
                // Keep this session's working directory authoritative.
                next.working_dir = cwd;
                runtime.reprepare_config(next).await.map_err(|e| {
                    AcpError::internal_error().data(format!("model reload failed: {e}"))
                })?;
            } else {
                return Err(AcpError::invalid_params().data("model option requires a value id"));
            }
            None
        }
        _ => None,
    };

    {
        let mut map = sessions.lock().await;
        if let Some(state) = map.get_mut(req.session_id.0.as_ref()) {
            state.config_options = catalog.clone();
        }
    }
    Ok((catalog, switched_mode))
}

/// Handle a `session/set_config_option` request.
///
/// Applies the option via [`apply_session_config_option`], broadcasts
/// `current_mode_update` when the mode changed and `config_option_update` with
/// the full updated catalog, then returns the catalog.
pub async fn handle_set_session_config_option(
    sessions: &Sessions,
    cx: &ConnectionTo<Client>,
    req: &SetSessionConfigOptionRequest,
    model_resolver: Option<&SessionModelResolver>,
    effort_resolver: Option<&SessionModelResolver>,
) -> Result<SetSessionConfigOptionResponse, AcpError> {
    let (catalog, switched_mode) =
        apply_session_config_option(sessions, req, model_resolver, effort_resolver).await?;
    if let Some(mode) = switched_mode {
        cx.send_notification(SessionNotification::new(
            req.session_id.clone(),
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(mode.wire())),
        ))?;
    }
    cx.send_notification(SessionNotification::new(
        req.session_id.clone(),
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(catalog.clone())),
    ))?;
    Ok(SetSessionConfigOptionResponse::new(catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        SessionConfigOption as Opt, SessionConfigOptionValue, SessionId,
        SetSessionConfigOptionRequest, SetSessionModeRequest,
    };
    use agent_client_protocol::{Agent, Channel, ConnectionTo, Responder};
    use atomcode_coding::CodingAgentConfig;
    use std::sync::Arc;

    use crate::acp::sessions::test_support::sessions_with;
    use crate::acp::sessions::SessionState;

    #[test]
    fn mode_id_mapping_round_trips_kernel_wire_names() {
        use atomcode_coding::RuntimeMode as K;
        assert_eq!(runtime_mode_from_id("build"), Some(K::Build));
        assert_eq!(runtime_mode_from_id("accept_edits"), Some(K::AcceptEdits));
        assert_eq!(runtime_mode_from_id("bypass"), Some(K::Auto));
        assert_eq!(runtime_mode_from_id("plan"), Some(K::Plan));
        assert_eq!(runtime_mode_from_id("nope"), None);
        // The advertised mode ids are the kernel wire names.
        let modes = session_modes();
        let ids: Vec<&str> = modes.iter().map(|m| m.id.0.as_ref()).collect();
        assert_eq!(ids, vec!["build", "accept_edits", "bypass", "plan"]);
        let state = session_mode_state();
        assert_eq!(state.current_mode_id.0.as_ref(), "build");
        assert_eq!(state.available_modes.len(), 4);
    }

    #[test]
    fn apply_config_option_updates_select_and_boolean() {
        use agent_client_protocol::schema::v1::{SessionConfigKind, SessionConfigValueId};
        let mut catalog = vec![
            Opt::select(
                "model",
                "Model",
                "m1",
                vec![
                    agent_client_protocol::schema::v1::SessionConfigSelectOption::new(
                        SessionConfigValueId::new("m1"),
                        "Model 1",
                    ),
                    agent_client_protocol::schema::v1::SessionConfigSelectOption::new(
                        SessionConfigValueId::new("m2"),
                        "Model 2",
                    ),
                ],
            ),
            Opt::boolean("verbose", "Verbose", false),
        ];

        assert!(apply_config_option(
            &mut catalog,
            "model",
            &SessionConfigOptionValue::value_id("m2")
        ));
        let SessionConfigKind::Select(select) = &catalog[0].kind else {
            panic!("model option must stay a select");
        };
        assert_eq!(select.current_value.0.as_ref(), "m2");

        assert!(apply_config_option(
            &mut catalog,
            "verbose",
            &SessionConfigOptionValue::boolean(true)
        ));
        let SessionConfigKind::Boolean(boolean) = &catalog[1].kind else {
            panic!("verbose option must stay a boolean");
        };
        assert!(boolean.current_value);

        // Unknown id and wrong-shape values are rejected.
        assert!(!apply_config_option(
            &mut catalog,
            "missing",
            &SessionConfigOptionValue::value_id("m1")
        ));
        assert!(!apply_config_option(
            &mut catalog,
            "model",
            &SessionConfigOptionValue::boolean(true)
        ));
    }

    #[tokio::test]
    async fn set_mode_switches_runtime_and_broadcasts() {
        use agent_client_protocol::schema::v1::SessionNotification;
        use atomcode_coding::runtime::{
            coding_runtime_control_channel, CodingRuntimeControl, RuntimeExit, RuntimeExitReason,
        };
        let (runtime, mut controls) = coding_runtime_control_channel();
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let state = SessionState {
            runtime,
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _task: tokio::spawn(async {
                RuntimeExit {
                    reason: RuntimeExitReason::ShutdownRequested,
                    forced: false,
                }
            }),
            native_id: "test-native".to_string(),
            cwd: std::path::PathBuf::from("/work"),
            current_mode: RuntimeMode::Build,
            config_options: Vec::new(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
        };
        let sessions: crate::acp::sessions::Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        sessions.lock().await.insert("acp-1".into(), state);

        // Control loop: answer SetMode (the handler awaits `done`).
        let control_task = tokio::spawn(async move {
            match controls.recv().await {
                Some(CodingRuntimeControl::SetMode { mode, done, .. }) => {
                    assert_eq!(mode, RuntimeMode::Plan);
                    let _ = done.send(Ok(()));
                    true
                }
                _ => false,
            }
        });

        // Minimal agent side: the exact handler under test, wired over an
        // in-memory channel. The client side sends the request and captures the
        // `current_mode_update` notification the handler broadcasts.
        let (agent_endpoint, client_endpoint) = Channel::duplex();
        let agent_sessions = Arc::clone(&sessions);
        let server = Agent
            .builder()
            .on_receive_request(
                async move |req: SetSessionModeRequest,
                            responder: Responder<SetSessionModeResponse>,
                            cx| {
                    match handle_set_session_mode(&agent_sessions, &cx, &req).await {
                        Ok(resp) => responder.respond(resp),
                        Err(err) => responder.respond_with_error(err),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(agent_endpoint);
        let server_task = tokio::spawn(server);

        let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        Client
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates
                            .lock()
                            .unwrap()
                            .push(serde_json::to_value(&notif.update).unwrap());
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_endpoint, |cx: ConnectionTo<Agent>| async move {
                let req = SetSessionModeRequest::new(SessionId::new("acp-1"), "plan");
                cx.send_request(req).block_task().await?;
                Ok(())
            })
            .await
            .unwrap();

        assert!(control_task.await.unwrap());
        server_task.abort();

        assert_eq!(
            sessions.lock().await.get("acp-1").unwrap().current_mode,
            RuntimeMode::Plan
        );
        let got = updates.lock().unwrap().clone();
        let mode_update = got
            .iter()
            .find(|u| u["sessionUpdate"] == "current_mode_update")
            .unwrap_or_else(|| panic!("no current_mode_update in updates: {got:?}"));
        assert_eq!(mode_update["currentModeId"], "plan");
    }

    /// The protocol allows `session/set_mode` and `session/set_config_option`
    /// at any time, including while a prompt turn is generating. A running
    /// turn holds the session's events receiver for its whole duration — the
    /// switches must complete without touching it (no deadlock) and must
    /// commit state without disturbing the turn.
    #[tokio::test]
    async fn mode_and_config_switches_complete_while_a_turn_is_running() {
        use atomcode_coding::runtime::{
            coding_runtime_control_channel, CodingRuntimeControl, RuntimeExit, RuntimeExitReason,
            RuntimeGeneration, SessionChanged,
        };
        let (runtime, mut controls) = coding_runtime_control_channel();
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let state = SessionState {
            runtime,
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _task: tokio::spawn(async {
                RuntimeExit {
                    reason: RuntimeExitReason::ShutdownRequested,
                    forced: false,
                }
            }),
            native_id: "test-native".to_string(),
            cwd: std::path::PathBuf::from("/work"),
            current_mode: RuntimeMode::Build,
            config_options: test_catalog(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
        };
        let sessions: crate::acp::sessions::Sessions =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                ("acp-1".to_string(), state),
            ])));

        // Control loop answers both switch commands while the "turn" runs.
        let control_task = tokio::spawn(async move {
            let mut set_mode = false;
            let mut reprepare = false;
            while let Some(ctrl) = controls.recv().await {
                match ctrl {
                    CodingRuntimeControl::SetMode { mode, done, .. } => {
                        set_mode = mode == RuntimeMode::Plan;
                        let _ = done.send(Ok(()));
                    }
                    CodingRuntimeControl::Reprepare { done, .. } => {
                        reprepare = true;
                        let _ = done.send(Ok(SessionChanged {
                            generation: RuntimeGeneration(0),
                            session_id: None,
                            working_dir: std::path::PathBuf::from("/work"),
                        }));
                    }
                    _ => {}
                }
                // Both switch kinds observed → the control loop may exit; the
                // handle lives on in the session state, so the channel never
                // closes on its own.
                if set_mode && reprepare {
                    break;
                }
            }
            (set_mode, reprepare)
        });

        // A running prompt turn holds the session's events receiver for its
        // whole duration (see `run_prompt_turn`). Hold it here across both
        // switches to prove the switches neither deadlock on it nor corrupt
        // turn state.
        let events_guard = Arc::clone(&sessions.lock().await.get("acp-1").unwrap().events);
        let _held = events_guard.lock().await;

        // 1. `session/set_mode` during the turn.
        switch_runtime_mode(&sessions, &SessionId::new("acp-1"), "plan")
            .await
            .expect("mode switch completes while a turn is running");

        // 2. `session/set_config_option` (reasoning_effort → provider reload) during the turn.
        let req = SetSessionConfigOptionRequest::new(
            SessionId::new("acp-1"),
            REASONING_EFFORT_CONFIG_ID,
            SessionConfigOptionValue::value_id("high"),
        );
        let resolver = |effort: &str| effort_resolver_fn(effort);
        let effort: &SessionModelResolver = &resolver;
        let (_catalog, switched_mode) =
            apply_session_config_option(&sessions, &req, None, Some(effort))
                .await
                .expect("config switch completes while a turn is running");
        assert!(switched_mode.is_none());

        drop(_held);
        let (set_mode, reprepare) = control_task.await.unwrap();
        assert!(set_mode, "SetMode control reached the runtime");
        assert!(reprepare, "Reprepare control reached the runtime");

        // Both switches committed to the session state.
        let map = sessions.lock().await;
        let state = map.get("acp-1").unwrap();
        assert_eq!(state.current_mode, RuntimeMode::Plan);
        assert!(state.config_options.iter().any(|o| {
            o.id.0.as_ref() == REASONING_EFFORT_CONFIG_ID
                && matches!(
                    &o.kind,
                    agent_client_protocol::schema::v1::SessionConfigKind::Select(s)
                        if s.current_value.0.as_ref() == "high"
                )
        }));
    }

    /// Live-session catalog with a `mode` select and a `reasoning_effort`
    /// select, mirroring what the CLI entry point builds.
    fn test_catalog() -> Vec<SessionConfigOption> {
        use agent_client_protocol::schema::v1::{
            SessionConfigOptionCategory, SessionConfigSelectOption,
        };
        vec![
            Opt::select(
                MODE_CONFIG_ID,
                "Mode",
                "build",
                [
                    ("build", "Build"),
                    ("accept_edits", "AcceptEdits"),
                    ("bypass", "Auto"),
                    ("plan", "Plan"),
                ]
                .into_iter()
                .map(|(value, name)| {
                    SessionConfigSelectOption::new(value.to_string(), name.to_string())
                })
                .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::Mode),
            Opt::select(
                REASONING_EFFORT_CONFIG_ID,
                "Reasoning effort",
                "off",
                [
                    ("off", "Off (API default)"),
                    ("high", "High"),
                    ("max", "Max"),
                ]
                .into_iter()
                .map(|(value, name)| {
                    SessionConfigSelectOption::new(value.to_string(), name.to_string())
                })
                .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ]
    }

    /// Sessions seeded with [`test_catalog`] (no live control receiver — only
    /// for paths that reject before touching the kernel).
    fn sessions_with_catalog(entries: Vec<(&str, &str)>) -> crate::acp::sessions::Sessions {
        let map: std::collections::HashMap<String, SessionState> = entries
            .into_iter()
            .map(|(id, cwd)| {
                let (runtime, _controls) =
                    atomcode_coding::runtime::coding_runtime_control_channel();
                let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
                (
                    id.to_string(),
                    SessionState {
                        runtime,
                        events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
                        _task: tokio::spawn(async {
                            atomcode_coding::runtime::RuntimeExit {
                                reason:
                                    atomcode_coding::runtime::RuntimeExitReason::ShutdownRequested,
                                forced: false,
                            }
                        }),
                        native_id: id.strip_prefix("acp-").unwrap_or(id).to_string(),
                        cwd: std::path::PathBuf::from(cwd),
                        current_mode: RuntimeMode::Build,
                        config_options: test_catalog(),
                        usage: (0, 0),
                        todo_calls: Vec::new(),
                        title: None,
                        additional_directories: Vec::new(),
                    },
                )
            })
            .collect();
        std::sync::Arc::new(tokio::sync::Mutex::new(map))
    }

    /// Resolve an effort value into a kernel config the way the CLI resolver
    /// does: `off` clears the override, anything else lands as a
    /// [`atomcode_kernel::provider::ReasoningEffort`].
    fn effort_resolver_fn(effort: &str) -> Option<CodingAgentConfig> {
        let mut cfg = CodingAgentConfig::new("key", "https://example.test/v1", "m", "/work");
        cfg.chat_options.reasoning_effort = match effort {
            "off" => None,
            "high" => Some(atomcode_kernel::provider::ReasoningEffort::High),
            _ => Some(atomcode_kernel::provider::ReasoningEffort::Max),
        };
        Some(cfg)
    }

    /// Drive one rejected `set_config_option` request through the wired handler
    /// over an in-memory channel; returns the JSON-RPC error.
    async fn drive_set_config_option(
        sessions: crate::acp::sessions::Sessions,
        build_req: impl Fn() -> SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::schema::v1::Error {
        let (agent_endpoint, client_endpoint) = Channel::duplex();
        let agent_sessions = Arc::clone(&sessions);
        let server = Agent
            .builder()
            .on_receive_request(
                async move |req: SetSessionConfigOptionRequest,
                            responder: Responder<SetSessionConfigOptionResponse>,
                            cx| {
                    match handle_set_session_config_option(&agent_sessions, &cx, &req, None, None)
                        .await
                    {
                        Ok(resp) => responder.respond(resp),
                        Err(err) => responder.respond_with_error(err),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(agent_endpoint);
        let server_task = tokio::spawn(server);
        let result = Client
            .builder()
            .connect_with(client_endpoint, move |cx: ConnectionTo<Agent>| async move {
                let err = cx
                    .send_request(build_req())
                    .block_task()
                    .await
                    .expect_err("set_config_option must be rejected");
                Ok(err)
            })
            .await
            .unwrap();
        server_task.abort();
        result
    }

    #[tokio::test]
    async fn set_config_option_unknown_option_or_session_errors_via_agent() {
        let sessions = sessions_with(vec![("acp-1", "/work")]);

        let unknown_option = drive_set_config_option(Arc::clone(&sessions), || {
            SetSessionConfigOptionRequest::new(
                SessionId::new("acp-1"),
                "nope",
                SessionConfigOptionValue::value_id("x"),
            )
        })
        .await;
        assert!(
            error_data_says_unknown(&unknown_option),
            "{unknown_option:?}"
        );

        let unknown_session = drive_set_config_option(Arc::clone(&sessions), || {
            SetSessionConfigOptionRequest::new(
                SessionId::new("acp-missing"),
                "model",
                SessionConfigOptionValue::value_id("x"),
            )
        })
        .await;
        assert!(
            error_data_says_unknown(&unknown_session),
            "{unknown_session:?}"
        );
    }

    #[tokio::test]
    async fn set_config_option_mode_switches_runtime_and_broadcasts() {
        use agent_client_protocol::schema::v1::SessionNotification;
        use atomcode_coding::runtime::{
            coding_runtime_control_channel, CodingRuntimeControl, RuntimeExit, RuntimeExitReason,
        };
        let (runtime, mut controls) = coding_runtime_control_channel();
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let state = SessionState {
            runtime,
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _task: tokio::spawn(async {
                RuntimeExit {
                    reason: RuntimeExitReason::ShutdownRequested,
                    forced: false,
                }
            }),
            native_id: "test-native".to_string(),
            cwd: std::path::PathBuf::from("/work"),
            current_mode: RuntimeMode::Build,
            config_options: test_catalog(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
        };
        let sessions: crate::acp::sessions::Sessions =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                ("acp-1".to_string(), state),
            ])));

        let control_task = tokio::spawn(async move {
            match controls.recv().await {
                Some(CodingRuntimeControl::SetMode { mode, done, .. }) => {
                    assert_eq!(mode, RuntimeMode::Plan);
                    let _ = done.send(Ok(()));
                    true
                }
                _ => false,
            }
        });

        let (agent_endpoint, client_endpoint) = Channel::duplex();
        let agent_sessions = Arc::clone(&sessions);
        let server = Agent
            .builder()
            .on_receive_request(
                async move |req: SetSessionConfigOptionRequest,
                            responder: Responder<SetSessionConfigOptionResponse>,
                            cx| {
                    match handle_set_session_config_option(&agent_sessions, &cx, &req, None, None)
                        .await
                    {
                        Ok(resp) => responder.respond(resp),
                        Err(err) => responder.respond_with_error(err),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(agent_endpoint);
        let server_task = tokio::spawn(server);

        let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates_for_handler = Arc::clone(&updates);
        let resp: SetSessionConfigOptionResponse = Client
            .builder()
            .on_receive_notification(
                move |notif: SessionNotification, _cx| {
                    let updates = Arc::clone(&updates_for_handler);
                    async move {
                        updates
                            .lock()
                            .unwrap()
                            .push(serde_json::to_value(&notif.update).unwrap());
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_endpoint, |cx: ConnectionTo<Agent>| async move {
                let resp: SetSessionConfigOptionResponse = cx
                    .send_request(SetSessionConfigOptionRequest::new(
                        SessionId::new("acp-1"),
                        MODE_CONFIG_ID,
                        SessionConfigOptionValue::value_id("plan"),
                    ))
                    .block_task()
                    .await?;
                Ok(resp)
            })
            .await
            .unwrap();
        assert!(control_task.await.unwrap(), "SetMode control received");
        server_task.abort();

        assert_eq!(
            sessions.lock().await.get("acp-1").unwrap().current_mode,
            RuntimeMode::Plan
        );
        let got = updates.lock().unwrap().clone();
        assert!(
            got.iter().any(
                |u| u["sessionUpdate"] == "current_mode_update" && u["currentModeId"] == "plan"
            ),
            "current_mode_update must be broadcast for the mode switch: {got:?}"
        );
        let mode_option = resp
            .config_options
            .iter()
            .find(|o| o.id.0.as_ref() == MODE_CONFIG_ID)
            .unwrap();
        let json = serde_json::to_value(mode_option).unwrap();
        assert_eq!(json["currentValue"], "plan", "{json}");
    }

    #[tokio::test]
    async fn set_config_option_reasoning_effort_reloads_provider() {
        use atomcode_coding::runtime::{
            coding_runtime_control_channel, RuntimeExit, RuntimeExitReason,
        };
        let (runtime, controls) = coding_runtime_control_channel();
        // No runtime owner: the provider reload fails fast (the control channel
        // send is rejected) instead of parking the turn waiting for a reply.
        drop(controls);
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let state = SessionState {
            runtime,
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _task: tokio::spawn(async {
                RuntimeExit {
                    reason: RuntimeExitReason::ShutdownRequested,
                    forced: false,
                }
            }),
            native_id: "test-native".to_string(),
            cwd: std::path::PathBuf::from("/work"),
            current_mode: RuntimeMode::Build,
            config_options: test_catalog(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
        };
        let sessions: crate::acp::sessions::Sessions =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                ("acp-1".to_string(), state),
            ])));

        let (agent_endpoint, client_endpoint) = Channel::duplex();
        let agent_sessions = Arc::clone(&sessions);
        // Records the value the resolver was asked to apply. The stub runtime
        // cannot complete a real reassemble, so the handler surfaces an error
        // *from the reload step* — which is exactly the contract under test:
        // a legal effort is accepted, forwarded to the resolver, and reaches
        // the provider reload (it is not rejected by value validation).
        let seen_effort: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let seen_for_handler = Arc::clone(&seen_effort);
        let server = Agent
            .builder()
            .on_receive_request(
                async move |req: SetSessionConfigOptionRequest,
                            responder: Responder<SetSessionConfigOptionResponse>,
                            cx| {
                    let seen = Arc::clone(&seen_for_handler);
                    let resolver = move |effort: &str| -> Option<CodingAgentConfig> {
                        *seen.lock().unwrap() = Some(effort.to_string());
                        effort_resolver_fn(effort)
                    };
                    let effort: &SessionModelResolver = &resolver;
                    match handle_set_session_config_option(
                        &agent_sessions,
                        &cx,
                        &req,
                        None,
                        Some(effort),
                    )
                    .await
                    {
                        Ok(resp) => responder.respond(resp),
                        Err(err) => responder.respond_with_error(err),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(agent_endpoint);
        let server_task = tokio::spawn(server);

        let err: agent_client_protocol::schema::v1::Error = Client
            .builder()
            .connect_with(client_endpoint, |cx: ConnectionTo<Agent>| async move {
                let err = cx
                    .send_request(SetSessionConfigOptionRequest::new(
                        SessionId::new("acp-1"),
                        REASONING_EFFORT_CONFIG_ID,
                        SessionConfigOptionValue::value_id("max"),
                    ))
                    .block_task()
                    .await
                    .expect_err("stub runtime cannot complete the reload");
                Ok(err)
            })
            .await
            .unwrap();
        server_task.abort();

        // The value passed validation and reached the resolver; the failure is
        // the stub runtime's, not an "unknown effort" / "cannot be applied"
        // rejection along the way.
        assert_eq!(
            *seen_effort.lock().unwrap(),
            Some("max".to_string()),
            "resolver must be asked for the requested effort"
        );
        assert!(
            err_data(&err).contains("reasoning effort reload failed"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn set_config_option_rejects_invalid_mode_or_effort() {
        let sessions = sessions_with_catalog(vec![("acp-1", "/work")]);

        let bad_mode = drive_set_config_option(Arc::clone(&sessions), || {
            SetSessionConfigOptionRequest::new(
                SessionId::new("acp-1"),
                MODE_CONFIG_ID,
                SessionConfigOptionValue::value_id("turbo"),
            )
        })
        .await;
        assert!(
            err_data(&bad_mode).contains("unknown session mode"),
            "{bad_mode:?}"
        );

        let bad_effort = drive_set_config_option(Arc::clone(&sessions), || {
            SetSessionConfigOptionRequest::new(
                SessionId::new("acp-1"),
                REASONING_EFFORT_CONFIG_ID,
                SessionConfigOptionValue::value_id("ultra"),
            )
        })
        .await;
        assert!(
            err_data(&bad_effort).contains("unknown reasoning effort"),
            "{bad_effort:?}"
        );
    }

    fn err_data(err: &agent_client_protocol::schema::v1::Error) -> String {
        err.data
            .as_ref()
            .and_then(|data| data.as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn error_data_says_unknown(err: &agent_client_protocol::schema::v1::Error) -> bool {
        err.data
            .as_ref()
            .and_then(|data| data.as_str())
            .map(|text| text.contains("unknown"))
            .unwrap_or(false)
    }
}
