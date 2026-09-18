//! `ExternalSubagentTool` — exposes one named external-agent instance as a
//! kernel tool the main model can call (`subagent_<name>`), plus the factory
//! that builds a backend from a profile and the registration helper that probes
//! the binary and mounts a tool per enabled profile.
//!
//! Phase 1 / T1.5 of the external-agent subagent-driver spec.

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolRegistry, ToolResult};

use super::claude_code::ClaudeCodeBackend;
use super::codex::CodexBackend;
use super::{
    ExternalSubagentProfile, PermissionMode, SubagentBackend, SubagentError, SubagentEvent,
    SubagentKind, SubagentRun, SubagentStopReason,
};

/// Derive the tool name for a profile: `subagent_<sanitized name>`. Any char
/// that is not ASCII-alphanumeric becomes `_` so the name is a valid tool id.
pub fn tool_name_for(profile_name: &str) -> String {
    let sanitized: String = profile_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("subagent_{sanitized}")
}

/// Build a backend from a resolved profile.
pub fn build_backend(profile: &ExternalSubagentProfile) -> Box<dyn SubagentBackend> {
    let timeout = profile.timeout;
    match profile.kind {
        SubagentKind::Codex => {
            let mut b = CodexBackend::new(&profile.name, profile.permission)
                .allow_dangerous(profile.allow_dangerous);
            if let Some(m) = &profile.model {
                b = b.with_model(m.clone());
            }
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Box::new(b)
        }
        SubagentKind::ClaudeCode => {
            let mut b = ClaudeCodeBackend::new(&profile.name, profile.permission)
                .allow_dangerous(profile.allow_dangerous);
            if let Some(m) = &profile.model {
                b = b.with_model(m.clone());
            }
            if let Some(t) = timeout {
                b = b.with_timeout(t);
            }
            Box::new(b)
        }
    }
}

/// Whether an EXECUTABLE named `bin` is found on `PATH`. Bare probe (no spawn):
/// scans `PATH` entries for a matching file (and, on Windows, common extensions).
/// On Unix the file must also have an execute bit — a plain data file named
/// `codex`/`claude` (e.g. a not-yet-`chmod`ed download) is not a runnable agent,
/// so it is rejected here rather than surfacing as a spawn `Permission denied`
/// only when the model first calls the tool.
pub fn binary_on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(&path).any(|dir| {
        exts.iter()
            .any(|ext| is_executable_file(&dir.join(format!("{bin}{ext}"))))
    })
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    // On Windows executability is implied by the extension (probed above).
    path.is_file()
}

/// A named external-agent instance mounted as a kernel tool.
pub struct ExternalSubagentTool {
    tool_name: String,
    description: String,
    backend: Box<dyn SubagentBackend>,
    /// Whether the instance is read-only (drives `read_only_hint` / `risk`).
    read_only: bool,
}

impl ExternalSubagentTool {
    /// Build a tool from a profile-built backend.
    pub fn new(backend: Box<dyn SubagentBackend>, permission: PermissionMode) -> Self {
        let name = backend.name().to_string();
        let tool_name = tool_name_for(&name);
        let agent = match backend.kind() {
            SubagentKind::Codex => "Codex",
            SubagentKind::ClaudeCode => "Claude Code",
        };
        let description = format!(
            "Delegate a self-contained coding task to the {agent} agent \
             (instance `{name}`, {perm} mode). Provide ONE detailed, standalone \
             prompt in `prompt`: the agent runs in a separate process with no \
             access to this conversation, so include all needed context. Returns \
             the agent's final answer.",
            perm = permission
        );
        Self {
            tool_name,
            description,
            backend,
            read_only: permission == PermissionMode::ReadOnly,
        }
    }
}

/// Parse the `prompt` argument. Accepts the standard `{"prompt": "..."}` object;
/// falls back to treating a bare non-JSON string as the prompt (weak-model
/// tolerance).
fn parse_prompt(args: &str) -> Option<String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => v
            .get("prompt")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty()),
        // Not JSON → treat the raw args as the prompt.
        Err(_) => Some(trimmed.to_string()),
    }
}

fn err(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: true,
        images: Vec::new(),
    }
}

#[async_trait]
impl Tool for ExternalSubagentTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The full, self-contained task for the external agent."
                }
            },
            "required": ["prompt"]
        })
    }

    fn read_only_hint(&self) -> bool {
        self.read_only
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        // A read-only instance cannot modify anything; any writing instance runs
        // an external agent that may edit files → gate it via approval.
        if self.read_only {
            RiskLevel::Safe
        } else {
            RiskLevel::Risky
        }
    }

    /// One "Always" grant covers the whole instance (every call to this tool),
    /// not per-prompt — approving the codex-primary subagent once is intentional.
    fn always_grant_scope(&self, _args: &str) -> String {
        self.tool_name.clone()
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let Some(prompt) = parse_prompt(args) else {
            return err(format!(
                "{}: missing required `prompt` (a self-contained task string).",
                self.tool_name
            ));
        };

        // Forward the agent's activity stream to the driver progress channel.
        let progress = ctx.progress.clone();
        let mut run = SubagentRun::new(prompt, ctx.working_dir.clone());
        run.cancel = ctx.cancel.clone();
        run.on_event = Some(Box::new(move |ev| {
            if let SubagentEvent::Activity(line) = ev {
                progress.emit(line);
            }
        }));

        match self.backend.run(run).await {
            Ok(res) => match res.stop_reason {
                SubagentStopReason::Completed => ToolResult {
                    call_id: String::new(),
                    content: res.output,
                    is_error: false,
                    images: Vec::new(),
                },
                SubagentStopReason::Cancelled => {
                    err(format!("{}: cancelled before completion.", self.tool_name))
                }
                SubagentStopReason::Timeout => err(format!(
                    "{}: timed out.{}",
                    self.tool_name,
                    if res.output.is_empty() {
                        String::new()
                    } else {
                        format!(" Partial output:\n{}", res.output)
                    }
                )),
                SubagentStopReason::PermissionDenied => err(format!(
                    "{}: the agent could not proceed (permission denied).",
                    self.tool_name
                )),
            },
            Err(SubagentError::DangerousModeRefused) => err(format!(
                "{}: bypass permission mode is not allowed in this context.",
                self.tool_name
            )),
            Err(e) => err(format!("{}: {e}", self.tool_name)),
        }
    }
}

/// Register one tool per enabled profile whose backing binary is present on
/// `PATH`. Profiles whose binary is missing are skipped (with a one-line stderr
/// warning) rather than failing assembly. Returns the registered tool names.
pub fn register_external_subagent_tools(
    registry: &mut ToolRegistry,
    profiles: &[ExternalSubagentProfile],
) -> Vec<String> {
    let mut registered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for profile in profiles {
        if !binary_on_path(profile.kind.binary()) {
            eprintln!(
                "subagent: `{}` ({}) not registered — binary `{}` not found on PATH",
                profile.name,
                profile.kind,
                profile.kind.binary()
            );
            continue;
        }
        let backend = build_backend(profile);
        let tool = ExternalSubagentTool::new(backend, profile.permission);
        let tool_name = tool.name().to_string();
        // Two profiles whose names sanitize to the same tool id would silently
        // overwrite in the registry (BTreeMap::insert) — the later one winning
        // with a possibly different permission posture. Refuse the collision.
        if !seen.insert(tool_name.clone()) {
            eprintln!(
                "subagent: `{}` → tool `{tool_name}` collides with an earlier profile; skipped \
                 (rename to a distinct instance name)",
                profile.name
            );
            continue;
        }
        registered.push(tool_name);
        registry.register(std::sync::Arc::new(tool));
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::super::SubagentResult;
    use super::*;
    use tokio_util::sync::CancellationToken;

    struct StubBackend {
        name: String,
        kind: SubagentKind,
        result: SubagentResult,
    }

    #[async_trait]
    impl SubagentBackend for StubBackend {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> SubagentKind {
            self.kind
        }
        fn capabilities(&self) -> super::super::SubagentCapabilities {
            Default::default()
        }
        async fn run(&self, req: SubagentRun) -> Result<SubagentResult, SubagentError> {
            // Echo prompt into an activity event to prove wiring, then return canned.
            req.emit(SubagentEvent::Activity(format!("got: {}", req.prompt)));
            Ok(self.result.clone())
        }
    }

    fn stub_tool(output: &str) -> ExternalSubagentTool {
        let backend = StubBackend {
            name: "codex-primary".into(),
            kind: SubagentKind::Codex,
            result: SubagentResult {
                output: output.into(),
                stop_reason: SubagentStopReason::Completed,
            },
        };
        ExternalSubagentTool::new(Box::new(backend), PermissionMode::ReadOnly)
    }

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[test]
    fn tool_name_sanitizes() {
        assert_eq!(tool_name_for("codex-primary"), "subagent_codex_primary");
        assert_eq!(tool_name_for("claude review 2"), "subagent_claude_review_2");
        assert_eq!(tool_name_for("x/../y"), "subagent_x____y");
    }

    #[test]
    fn tool_name_and_schema() {
        let tool = stub_tool("ok");
        assert_eq!(tool.name(), "subagent_codex_primary");
        assert!(tool.description().contains("Codex"));
        assert!(tool.description().contains("codex-primary"));
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"][0], "prompt");
    }

    #[test]
    fn read_only_instance_is_safe_writing_is_risky() {
        let ro = stub_tool("x");
        assert!(ro.read_only_hint());
        assert!(matches!(ro.risk(""), RiskLevel::Safe));

        let backend = StubBackend {
            name: "codex-w".into(),
            kind: SubagentKind::Codex,
            result: SubagentResult {
                output: "x".into(),
                stop_reason: SubagentStopReason::Completed,
            },
        };
        let rw = ExternalSubagentTool::new(Box::new(backend), PermissionMode::AcceptEdits);
        assert!(!rw.read_only_hint());
        assert!(matches!(rw.risk(""), RiskLevel::Risky));
    }

    #[test]
    fn parse_prompt_accepts_object_and_bare_string() {
        assert_eq!(
            parse_prompt(r#"{"prompt":"do it"}"#).as_deref(),
            Some("do it")
        );
        assert_eq!(parse_prompt("bare prompt").as_deref(), Some("bare prompt"));
        assert_eq!(parse_prompt(r#"{"prompt":"  "}"#), None);
        assert_eq!(parse_prompt(""), None);
        assert_eq!(parse_prompt(r#"{"other":1}"#), None);
    }

    #[tokio::test]
    async fn execute_returns_backend_output() {
        let tool = stub_tool("STUB RESULT");
        let res = tool.execute(r#"{"prompt":"summarize"}"#, &ctx()).await;
        assert!(!res.is_error);
        assert_eq!(res.content, "STUB RESULT");
    }

    #[tokio::test]
    async fn execute_missing_prompt_is_error() {
        let tool = stub_tool("x");
        let res = tool.execute("{}", &ctx()).await;
        assert!(res.is_error);
        assert!(res.content.contains("missing required `prompt`"));
    }

    #[test]
    fn build_backend_maps_kind() {
        let p = ExternalSubagentProfile::new("c", SubagentKind::Codex);
        assert_eq!(build_backend(&p).kind(), SubagentKind::Codex);
        let p = ExternalSubagentProfile::new("cc", SubagentKind::ClaudeCode);
        assert_eq!(build_backend(&p).kind(), SubagentKind::ClaudeCode);
    }

    #[test]
    fn register_skips_missing_binary() {
        // A profile whose kind maps to a binary that certainly is not on PATH:
        // we cannot rename the binary, but we CAN assert that a nonexistent PATH
        // yields no registration by checking binary_on_path directly.
        assert!(!binary_on_path("atomcode-definitely-not-a-real-binary-xyz"));
    }
}
