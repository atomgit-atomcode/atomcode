//! One-shot JSON event adapters for Grok Build and OpenCode.
use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::proc::{drain_and_wait, ChildSpec, ManagedChild, StdinMode, WaitOutcome};
use super::{
    ExternalSubagentProfile, PermissionMode, SubagentBackend, SubagentCapabilities, SubagentError,
    SubagentEvent, SubagentKind, SubagentResult, SubagentRun, SubagentStopReason,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub(super) struct ReadOnlyCliBackend(pub(super) ExternalSubagentProfile);

impl ReadOnlyCliBackend {
    fn spec(&self, req: &SubagentRun) -> Result<ChildSpec, SubagentError> {
        let profile = &self.0;
        if profile.permission != PermissionMode::ReadOnly {
            return Err(SubagentError::AgentError(format!(
                "{} currently supports only read-only delegation",
                profile.kind
            )));
        }
        let mut spec = ChildSpec::new(profile.kind.binary(), &req.cwd);
        match profile.kind {
            SubagentKind::Grok => {
                if !cfg!(any(target_os = "macos", target_os = "linux")) {
                    return Err(SubagentError::AgentError(
                        "Grok read-only sandbox requires macOS or Linux".into(),
                    ));
                }
                spec = spec.args([
                    "--no-auto-update",
                    "--sandbox",
                    "read-only",
                    "--permission-mode",
                    "dontAsk",
                    "--no-subagents",
                    "--disable-web-search",
                    "--tools",
                    "read_file,list_dir,grep",
                    "--disallowed-tools",
                    "search_tool,use_tool",
                    "--output-format",
                    "streaming-json",
                ]);
                spec.args.push(format!("--single={}", req.prompt));
            }
            SubagentKind::OpenCode => {
                spec = spec.args([
                    "--pure",
                    "run",
                    "--format",
                    "json",
                    "--agent",
                    "atomcode-readonly",
                ]);
                spec.stdin = StdinMode::Piped;
                spec.env.extend([
                    ("PWD".into(), req.cwd.to_string_lossy().into_owned()),
                    ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()),
                    ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
                    (
                        "OPENCODE_CONFIG_CONTENT".into(),
                        json!({
                            "agent": { "atomcode-readonly": {
                                "mode": "primary",
                                "permission": {
                                    "*": "deny", "read": "allow", "glob": "allow", "grep": "allow",
                                    "list": "allow", "external_directory": "deny"
                                }
                            }}
                        })
                        .to_string(),
                    ),
                ]);
            }
            _ => {
                return Err(SubagentError::ProtocolError(
                    "unsupported CLI backend".into(),
                ))
            }
        }
        if let Some(model) = &profile.model {
            spec.args.push(format!("--model={model}"));
        }
        Ok(spec)
    }
}

/// OpenCode CLI versions differ in how agent permissions are configured. Check
/// the effective policy before sending a prompt, so an ignored setting cannot
/// delegate work to the default writable agent.
fn opencode_policy_is_read_only(raw: &str, tool_output_pattern: Option<&str>) -> bool {
    let Ok(agent) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    if agent["name"] != "atomcode-readonly" || agent["mode"] != "primary" {
        return false;
    }
    let Some(rules) = agent["permission"].as_array() else {
        return false;
    };
    let Some(deny) = rules.iter().rposition(|rule| {
        rule["permission"] == "*" && rule["pattern"] == "*" && rule["action"] == "deny"
    }) else {
        return false;
    };
    let tail = &rules[deny + 1..];
    let denies_external_paths = tail.iter().any(|rule| {
        rule["permission"] == "external_directory"
            && rule["pattern"] == "*"
            && rule["action"] == "deny"
    });
    denies_external_paths
        && tail
            .iter()
            .any(|rule| rule["permission"] == "read" && rule["action"] == "allow")
        && tail.iter().all(|rule| match rule["action"].as_str() {
            Some("deny" | "ask") => true,
            Some("allow") => match rule["permission"].as_str() {
                Some("read" | "glob" | "grep" | "list") => true,
                // OpenCode appends an allowance for its own truncated tool output.
                Some("external_directory") => {
                    tool_output_pattern.is_some() && rule["pattern"].as_str() == tool_output_pattern
                }
                _ => false,
            },
            _ => false,
        })
}

fn opencode_tool_output_pattern() -> Option<String> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("share")))?;
    let dir = data.join("opencode").join("tool-output");
    let path = dir.to_str()?;
    if !dir.is_absolute() || path.contains(['*', '?']) {
        return None;
    }
    Some(format!("{path}{}*", std::path::MAIN_SEPARATOR))
}

async fn check_opencode_policy(
    mut spec: ChildSpec,
    req: &SubagentRun,
    timeout: Duration,
) -> Result<Option<SubagentStopReason>, SubagentError> {
    spec.args = ["--pure", "debug", "agent", "atomcode-readonly"]
        .map(str::to_owned)
        .to_vec();
    spec.stdin = StdinMode::Null;
    let mut child = ManagedChild::spawn(spec)?;
    let mut resolved = String::new();
    let mut oversized = false;
    let (outcome, _) = drain_and_wait(&mut child, timeout, &req.cancel, |line| {
        if resolved.len().saturating_add(line.len()) >= MAX_OUTPUT_BYTES {
            oversized = true;
        } else if !oversized {
            resolved.push_str(&line);
            resolved.push('\n');
        }
    })
    .await
    .map_err(|e| SubagentError::ProtocolError(e.to_string()))?;
    let tool_output_pattern = opencode_tool_output_pattern();
    match outcome {
        WaitOutcome::Cancelled => Ok(Some(SubagentStopReason::Cancelled)),
        WaitOutcome::TimedOut => Ok(Some(SubagentStopReason::Timeout)),
        WaitOutcome::Exited(status)
            if status.success()
                && !oversized
                && opencode_policy_is_read_only(&resolved, tool_output_pattern.as_deref()) =>
        {
            Ok(None)
        }
        WaitOutcome::Exited(_) => Err(SubagentError::AgentError(
            "could not verify the OpenCode read-only policy; update OpenCode or check its agent configuration".into(),
        )),
    }
}

#[derive(Default)]
struct RunOutput {
    text: String,
    error: Option<String>,
    finished: bool,
}

impl RunOutput {
    fn event(&mut self, kind: SubagentKind, req: &SubagentRun, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let ty = event["type"].as_str();
        match (kind, ty) {
            (_, Some("error")) => {
                self.error = Some("external agent reported an error".into());
                // Report only a bounded HTTP status. The raw event can contain
                // provider response bodies, headers and request metadata.
                if let Some(status) = event
                    .pointer("/error/data/statusCode")
                    .and_then(Value::as_u64)
                {
                    self.error = Some(format!("external agent reported HTTP {status}"));
                }
            }
            (SubagentKind::Grok, Some("end")) => {
                self.finished = event["stopReason"] == "end_turn";
                if !self.finished && self.error.is_none() {
                    self.error = Some("Grok ended without completing the task".into());
                }
            }
            (SubagentKind::OpenCode, Some("step_start")) => self.finished = false,
            (SubagentKind::OpenCode, Some("step_finish")) => {
                self.finished =
                    event.pointer("/part/reason").and_then(Value::as_str) == Some("stop");
            }
            (_, Some("tool_use" | "tool_call")) => {
                req.emit(SubagentEvent::Activity(format!(
                    "{kind}: reading workspace"
                )));
            }
            (_, Some("text")) => {
                let delta = match kind {
                    SubagentKind::Grok => event["data"].as_str(),
                    SubagentKind::OpenCode => event.pointer("/part/text").and_then(Value::as_str),
                    _ => None,
                };
                if let Some(delta) = delta.filter(|s| !s.is_empty()) {
                    if self.text.is_empty() {
                        req.emit(SubagentEvent::Activity(format!(
                            "{kind}: returning analysis"
                        )));
                    }
                    let mut len = delta.len().min(MAX_OUTPUT_BYTES - self.text.len());
                    while !delta.is_char_boundary(len) {
                        len -= 1;
                    }
                    self.text.push_str(&delta[..len]);
                    if len < delta.len() && self.error.is_none() {
                        self.error = Some("external agent output exceeded the size limit".into());
                    }
                }
            }
            _ => {}
        }
    }
}

#[async_trait]
impl SubagentBackend for ReadOnlyCliBackend {
    fn name(&self) -> &str {
        &self.0.name
    }
    fn kind(&self) -> SubagentKind {
        self.0.kind
    }
    fn capabilities(&self) -> SubagentCapabilities {
        SubagentCapabilities::default()
    }

    async fn run(&self, req: SubagentRun) -> Result<SubagentResult, SubagentError> {
        let spec = self.spec(&req)?;
        let kind = self.kind();
        let mut output = RunOutput::default();
        if req.cancel.is_cancelled() {
            return Ok(SubagentResult {
                output: output.text,
                stop_reason: SubagentStopReason::Cancelled,
            });
        }
        let timeout = self.0.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let started = tokio::time::Instant::now();
        if kind == SubagentKind::OpenCode {
            if let Some(stop_reason) = check_opencode_policy(spec.clone(), &req, timeout).await? {
                return Ok(SubagentResult {
                    output: output.text,
                    stop_reason,
                });
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if req.cancel.is_cancelled() || remaining.is_zero() {
            return Ok(SubagentResult {
                output: output.text,
                stop_reason: if req.cancel.is_cancelled() {
                    SubagentStopReason::Cancelled
                } else {
                    SubagentStopReason::Timeout
                },
            });
        }
        let mut child = ManagedChild::spawn(spec)?;
        if kind == SubagentKind::OpenCode {
            child.write_stdin_and_close(req.prompt.clone());
        }
        req.emit(SubagentEvent::Activity(format!("{kind}: started")));
        let (outcome, _) = drain_and_wait(&mut child, remaining, &req.cancel, |line| {
            output.event(kind, &req, &line)
        })
        .await
        .map_err(|e| SubagentError::ProtocolError(e.to_string()))?;
        let stop_reason = match outcome {
            WaitOutcome::Cancelled => SubagentStopReason::Cancelled,
            WaitOutcome::TimedOut => SubagentStopReason::Timeout,
            WaitOutcome::Exited(status) => {
                if let Some(error) = output.error {
                    return Err(SubagentError::AgentError(error));
                }
                if !status.success() {
                    return Err(SubagentError::NonZeroExit {
                        code: status.code(),
                        stderr_tail: String::new(),
                    });
                }
                if !output.finished || output.text.trim().is_empty() {
                    return Err(SubagentError::ProtocolError(
                        "missing completed assistant response".into(),
                    ));
                }
                SubagentStopReason::Completed
            }
        };
        Ok(SubagentResult {
            output: output.text,
            stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(kind: SubagentKind) -> ReadOnlyCliBackend {
        ReadOnlyCliBackend(ExternalSubagentProfile::new(kind.as_str(), kind))
    }

    #[test]
    fn opencode_invocation_installs_deny_by_default_policy() {
        let req = SubagentRun::new("--auto", "/workspace");
        let spec = backend(SubagentKind::OpenCode).spec(&req).unwrap();
        assert!(matches!(spec.stdin, StdinMode::Piped));
        assert!(!spec.args.iter().any(|arg| arg == "--auto"));
        assert!(spec.args.iter().any(|arg| arg == "--pure"));
        let (_, config) = spec
            .env
            .iter()
            .find(|(name, _)| name == "OPENCODE_CONFIG_CONTENT")
            .unwrap();
        let config: Value = serde_json::from_str(config).unwrap();
        assert_eq!(
            config["agent"]["atomcode-readonly"]["permission"]["*"],
            "deny"
        );
        assert_eq!(
            config["agent"]["atomcode-readonly"]["permission"]["read"],
            "allow"
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn grok_uses_read_only_sandbox_and_limits_discovery() {
        let req = SubagentRun::new("--always-approve", ".");
        let spec = backend(SubagentKind::Grok).spec(&req).unwrap();
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--sandbox", "read-only"]));
        assert!(spec
            .args
            .windows(2)
            .any(|pair| pair == ["--disallowed-tools", "search_tool,use_tool"]));
        assert!(spec
            .args
            .iter()
            .any(|arg| arg == "--single=--always-approve"));
        assert!(!spec.args.iter().any(|arg| arg == "--always-approve"));
    }

    #[test]
    fn opencode_policy_refuses_ignored_or_overridden_permission() {
        let mut policy = json!({
            "name": "atomcode-readonly", "mode": "primary",
            "permission": [
                {"permission":"*", "pattern":"*", "action":"allow"},
                {"permission":"*", "pattern":"*", "action":"deny"},
                {"permission":"read", "pattern":"*", "action":"allow"},
                {"permission":"external_directory", "pattern":"*", "action":"deny"},
                {"permission":"external_directory", "pattern":"/data/opencode/tool-output/*", "action":"allow"}
            ]
        });
        let expected = Some("/data/opencode/tool-output/*");
        assert!(opencode_policy_is_read_only(&policy.to_string(), expected));
        policy["permission"][4]["pattern"] = json!("*");
        assert!(!opencode_policy_is_read_only(&policy.to_string(), expected));
        policy["permission"][4]["pattern"] = json!("/data/opencode/tool-output/*");
        policy["permission"][3]["pattern"] = json!("/narrow/*");
        assert!(!opencode_policy_is_read_only(&policy.to_string(), expected));
        policy["permission"][3]["pattern"] = json!("*");
        policy["permission"]
            .as_array_mut()
            .unwrap()
            .push(json!({"permission":"bash", "pattern":"*", "action":"allow"}));
        assert!(!opencode_policy_is_read_only(&policy.to_string(), expected));
        assert!(!opencode_policy_is_read_only("{}", expected));
    }

    #[tokio::test]
    async fn refuses_write_modes_without_starting_a_process() {
        for kind in [SubagentKind::Grok, SubagentKind::OpenCode] {
            let mut backend = backend(kind);
            backend.0.permission = PermissionMode::AcceptEdits;
            assert!(matches!(
                backend.run(SubagentRun::new("review", ".")).await,
                Err(SubagentError::AgentError(_))
            ));
        }
    }

    #[test]
    fn extracts_final_text_and_requires_a_successful_terminal_event() {
        let req = SubagentRun::new("review", ".");
        for (kind, events) in [
            (
                SubagentKind::Grok,
                [
                    r#"{"type":"thought","data":"private"}"#,
                    r#"{"type":"text","data":"OK"}"#,
                    r#"{"type":"end","stopReason":"end_turn"}"#,
                ],
            ),
            (
                SubagentKind::OpenCode,
                [
                    r#"{"type":"step_finish","part":{"reason":"tool-calls"}}"#,
                    r#"{"type":"text","part":{"text":"OK"}}"#,
                    r#"{"type":"step_finish","part":{"reason":"stop"}}"#,
                ],
            ),
        ] {
            let mut output = RunOutput::default();
            for event in &events[..2] {
                output.event(kind, &req, event);
            }
            assert_eq!(output.text, "OK");
            assert!(!output.finished);
            output.event(kind, &req, events[2]);
            assert!(output.finished);
            assert!(output.error.is_none());
            output.event(
                kind,
                &req,
                r#"{"type":"error","error":{"data":{"statusCode":402,"responseBody":"private"}}}"#,
            );
            assert_eq!(
                output.error.as_deref(),
                Some("external agent reported HTTP 402")
            );
        }
    }
}
