//! Configuration for assembling a coding agent.

use std::path::PathBuf;
use std::time::Duration;

/// Everything [`build_coding_agent`](crate::build_coding_agent) needs: provider
/// credentials, the working directory the tools are scoped to, and liveness bounds.
///
/// Timeouts default to sane non-infinite values — the kernel itself defaults to
/// unbounded, and the assembly map flagged "L2 MUST set stream/request timeouts" so a
/// stalled provider or silent driver can never park a turn forever.
#[derive(Clone, Debug)]
pub struct CodingAgentConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// Directory the agent's tools see as their working dir — PINNED (via the kernel
    /// `working_dir` seam), not the process-global cwd, so concurrent agents don't race.
    pub working_dir: PathBuf,
    /// Model context window in tokens (forwarded to the provider). Default 128k.
    pub context_window: u32,
    /// Liveness: max wait for the next stream event (first-token + inter-token). Default 120s.
    pub stream_timeout: Duration,
    /// Liveness: max wait for a driver approval response before it degrades to deny. Default 300s.
    pub request_timeout: Duration,
    /// Safety fuse: max edit-then-verify continuations per turn (kernel default is 50).
    pub max_continuations: u32,
}

impl CodingAgentConfig {
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
            max_continuations: 50,
        }
    }
}
