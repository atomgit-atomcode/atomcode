//! External-agent subagent drivers.
//!
//! Lets atomcode drive external coding agents (Claude Code, Codex) as
//! subagents, exposed to the main model as named subagent tools. This module
//! owns the backend-neutral contract; concrete adapters (`codex`, `claude_code`)
//! and the shared child-process helper (`proc`) live alongside it.
//!
//! Design: `docs/plans/2026-08-21-external-agent-subagent-drivers-spec.md`.
//!
//! Phase 1 / T1.1 — this file is the pure contract only: types + the
//! [`SubagentBackend`] trait. Adapters and process management arrive in later
//! tasks. Nothing here spawns a process or references an external binary.

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub mod claude_code;
pub mod codex;
pub mod proc;

/// Which external agent a backend drives. Parsed from the `kind` field of an
/// `[[subagent.external]]` config entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentKind {
    /// Anthropic Claude Code (`claude` CLI, headless print mode).
    ClaudeCode,
    /// OpenAI Codex (`codex` CLI).
    Codex,
}

impl SubagentKind {
    /// Parse from a config string (`"claude-code"` / `"codex"`); accepts a few
    /// common spellings. Returns `None` for anything unrecognized so config
    /// loading can surface an explicit error rather than guessing.
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude_code" | "claude" | "cc" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The canonical config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// The external binary this kind drives (looked up on `PATH`).
    pub fn binary(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }
}

impl fmt::Display for SubagentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Non-interactive permission posture for a driven agent, mapped by each adapter
/// onto that agent's own flags (see the spec's mapping table). `ReadOnly` is the
/// deliberate fail-closed default; `Bypass` is dangerous and must be explicitly
/// requested (and is refused outright in non-interactive/scheduled contexts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// No writes; plan/read-only sandbox. Default.
    #[default]
    ReadOnly,
    /// Auto-accept file edits, still gated elsewhere.
    AcceptEdits,
    /// Full autonomy within a workspace-write sandbox.
    Auto,
    /// Bypass approvals and sandbox entirely. DANGEROUS — explicit opt-in only.
    Bypass,
}

impl PermissionMode {
    /// Parse from a config string. Returns `None` for unrecognized values so the
    /// caller can reject rather than silently default (except that a MISSING
    /// field should default to `ReadOnly` at the config layer).
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" => Some(Self::ReadOnly),
            "accept-edits" | "acceptedits" => Some(Self::AcceptEdits),
            "auto" => Some(Self::Auto),
            "bypass" => Some(Self::Bypass),
            _ => None,
        }
    }

    /// The canonical config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }

    /// Whether this mode grants the driven agent any autonomy beyond reading.
    pub fn is_dangerous(self) -> bool {
        matches!(self, Self::Bypass)
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Static, start-time features a backend supports. Method/field presence is the
/// capability: the assembly layer checks these before wiring a `SubagentRun` so
/// an unsupported option is rejected up front rather than silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubagentCapabilities {
    /// The backend can restrict the driven agent to a subset of tools.
    pub tool_filter: bool,
    /// The backend can request structured (schema-validated) output.
    pub structured_output: bool,
}

/// A progress event streamed from a running backend back to the Task panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentEvent {
    /// An ephemeral activity line (tool use, status) for the live panel.
    Activity(String),
    /// A chunk of the agent's assistant-visible text output.
    TextDelta(String),
}

/// Sink for [`SubagentEvent`]s. Backends call this as work streams in; the Task
/// layer forwards it to the existing progress hook. `None` disables streaming
/// (the final [`SubagentResult`] still carries the full output).
pub type EventSink = Box<dyn Fn(SubagentEvent) + Send + Sync>;

/// One delegated run: the prompt to hand the external agent and where to run it.
/// The permission posture is a property of the named backend instance (from its
/// config profile), NOT of the individual call, so it lives on the backend, not
/// here. Backends are one-shot in Phase 1 — no multi-turn continuation (that is
/// Phase 4, Codex app-server).
pub struct SubagentRun {
    /// The task prompt handed to the external agent.
    pub prompt: String,
    /// Working directory the agent runs in.
    pub cwd: PathBuf,
    /// Optional tool allowlist (only honored when the backend advertises
    /// `capabilities().tool_filter`).
    pub tool_filter: Option<Vec<String>>,
    /// Cooperative cancellation — cancelling kills the child process tree.
    pub cancel: CancellationToken,
    /// Optional live-progress sink.
    pub on_event: Option<EventSink>,
}

impl SubagentRun {
    /// Minimal constructor for a no-streaming run.
    pub fn new(prompt: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            prompt: prompt.into(),
            cwd: cwd.into(),
            tool_filter: None,
            cancel: CancellationToken::new(),
            on_event: None,
        }
    }

    /// Emit an event to the sink if one is attached.
    pub fn emit(&self, event: SubagentEvent) {
        if let Some(sink) = &self.on_event {
            sink(event);
        }
    }
}

impl fmt::Debug for SubagentRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubagentRun")
            .field("prompt_len", &self.prompt.len())
            .field("cwd", &self.cwd)
            .field("tool_filter", &self.tool_filter)
            .field("has_event_sink", &self.on_event.is_some())
            .finish()
    }
}

/// Why a backend run ended. Distinct from the kernel's turn-level `StopReason`:
/// a driven agent is an external process, not an atomcode turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStopReason {
    /// The agent finished and produced a result.
    Completed,
    /// The run was cancelled (parent/user cancel).
    Cancelled,
    /// The agent refused a required permission and could not proceed.
    PermissionDenied,
    /// A liveness/overall timeout elapsed.
    Timeout,
}

/// The outcome of a delegated run, folded back into a `ToolResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentResult {
    /// The agent's final text output (summary handed back to the main model).
    pub output: String,
    /// Why the run ended.
    pub stop_reason: SubagentStopReason,
}

/// A failure driving an external agent. `run` returns `Ok(SubagentResult)` for
/// agent-level outcomes (including a clean permission refusal) and `Err` only
/// for driver-level failures (spawn/protocol/exit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentError {
    /// The external binary could not be found on `PATH`.
    BinaryNotFound { binary: String },
    /// Spawning the child process failed.
    SpawnFailed(String),
    /// The child exited non-zero.
    NonZeroExit { code: Option<i32>, stderr_tail: String },
    /// The agent's output stream/protocol could not be parsed.
    ProtocolError(String),
    /// `Bypass` was requested in a context that forbids it.
    DangerousModeRefused,
}

impl fmt::Display for SubagentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BinaryNotFound { binary } => {
                write!(f, "external agent binary `{binary}` not found on PATH")
            }
            Self::SpawnFailed(e) => write!(f, "failed to spawn external agent: {e}"),
            Self::NonZeroExit { code, stderr_tail } => write!(
                f,
                "external agent exited with {} (stderr: {stderr_tail})",
                code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
            ),
            Self::ProtocolError(e) => write!(f, "external agent protocol error: {e}"),
            Self::DangerousModeRefused => {
                f.write_str("bypass permission mode is not allowed in this context")
            }
        }
    }
}

impl std::error::Error for SubagentError {}

/// A driver for one named external-agent instance. Each `[[subagent.external]]`
/// profile constructs one backend, registered as a named subagent tool
/// (`subagent_<name>`). Concrete adapters implement this over a CLI subprocess.
#[async_trait]
pub trait SubagentBackend: Send + Sync {
    /// The instance name (e.g. `"codex-primary"`), source of the tool name.
    fn name(&self) -> &str;

    /// Which external agent this backend drives.
    fn kind(&self) -> SubagentKind;

    /// Static start-time capabilities.
    fn capabilities(&self) -> SubagentCapabilities;

    /// Delegate one prompt to the external agent and return its result.
    /// Streams progress via `req.on_event`; honors `req.cancel`.
    async fn run(&self, req: SubagentRun) -> Result<SubagentResult, SubagentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_config_round_trip_and_default() {
        assert_eq!(PermissionMode::default(), PermissionMode::ReadOnly);
        for mode in [
            PermissionMode::ReadOnly,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::Bypass,
        ] {
            assert_eq!(PermissionMode::from_config_str(mode.as_str()), Some(mode));
        }
        // Spelling tolerance + underscore normalization.
        assert_eq!(
            PermissionMode::from_config_str("Accept_Edits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(PermissionMode::from_config_str("readonly"), Some(PermissionMode::ReadOnly));
        // Unknown → None (caller rejects, does not silently default).
        assert_eq!(PermissionMode::from_config_str("yolo"), None);
        // Only Bypass is dangerous.
        assert!(PermissionMode::Bypass.is_dangerous());
        assert!(!PermissionMode::Auto.is_dangerous());
    }

    #[test]
    fn subagent_kind_parse_and_binary() {
        assert_eq!(SubagentKind::from_config_str("codex"), Some(SubagentKind::Codex));
        assert_eq!(
            SubagentKind::from_config_str("Claude_Code"),
            Some(SubagentKind::ClaudeCode)
        );
        assert_eq!(SubagentKind::from_config_str("cc"), Some(SubagentKind::ClaudeCode));
        assert_eq!(SubagentKind::from_config_str("gemini"), None);
        assert_eq!(SubagentKind::Codex.binary(), "codex");
        assert_eq!(SubagentKind::ClaudeCode.binary(), "claude");
        assert_eq!(SubagentKind::Codex.as_str(), "codex");
    }

    #[test]
    fn run_helpers_defaults_and_emit_is_optional() {
        let run = SubagentRun::new("do the thing", "/tmp/proj");
        assert!(run.tool_filter.is_none());
        assert!(run.on_event.is_none());
        // emit with no sink is a no-op (must not panic).
        run.emit(SubagentEvent::Activity("noop".into()));
    }

    #[test]
    fn event_sink_receives_events() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<SubagentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let mut run = SubagentRun::new("p", "/tmp");
        run.on_event = Some(Box::new(move |ev| sink_seen.lock().unwrap().push(ev)));
        run.emit(SubagentEvent::TextDelta("hi".into()));
        run.emit(SubagentEvent::Activity("run".into()));
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                SubagentEvent::TextDelta("hi".into()),
                SubagentEvent::Activity("run".into())
            ]
        );
    }

    #[test]
    fn error_display_is_human_readable() {
        let e = SubagentError::BinaryNotFound { binary: "codex".into() };
        assert!(e.to_string().contains("codex"));
        assert!(SubagentError::DangerousModeRefused.to_string().contains("bypass"));
    }
}
