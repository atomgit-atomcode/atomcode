//! Configuration for assembling a review agent. Mirrors `atomcode-coding`'s config but
//! for a READ-ONLY reviewer: provider creds, the repo working dir the read tools are
//! scoped to, and liveness bounds.

use std::path::PathBuf;
use std::time::Duration;

/// Everything [`build_review_agent`](crate::build_review_agent) needs.
#[derive(Clone, Debug)]
pub struct ReviewAgentConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// Repo root the review tools (read/grep/glob/codeintel) are scoped to — PINNED via
    /// the kernel `working_dir` seam, not the process cwd.
    pub working_dir: PathBuf,
    /// Model context window in tokens (forwarded to the provider). Default 128k.
    pub context_window: u32,
    /// Liveness: max wait for the next stream event. Default 120s.
    pub stream_timeout: Duration,
    /// Liveness: max wait for a driver response before degrade-to-deny. Default 300s.
    pub request_timeout: Duration,
    /// FULL system-prompt override. `None` (default) ⇒ the built-in
    /// [`review_persona`](crate::review_persona). `Some(text)` REPLACES it entirely — the
    /// built-in reviewer instructions are NOT appended. The caller is then responsible for
    /// telling the model about the read-only toolset + `report_finding`.
    pub persona: Option<String>,
    /// Extra system-prompt section APPENDED after the persona (built-in or overridden):
    /// the normal customization channel for domain rules, ignore lists, repo style guides,
    /// PR metadata — without copying or replacing the built-in reviewer instructions.
    /// Composes with `persona`: final prompt = (override or built-in) + "\n\n" + append.
    pub persona_append: Option<String>,
}

impl ReviewAgentConfig {
    /// Construct with the required fields and sane defaults for the rest.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        working_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            working_dir: working_dir.into(),
            context_window: 128_000,
            stream_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(300),
            persona: None,
            persona_append: None,
        }
    }

    /// Set a FULL system-prompt override (replaces the built-in reviewer persona).
    pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
        self.persona = Some(persona.into());
        self
    }

    /// Append an extra section after the persona (built-in or overridden).
    pub fn with_persona_append(mut self, append: impl Into<String>) -> Self {
        self.persona_append = Some(append.into());
        self
    }
}
