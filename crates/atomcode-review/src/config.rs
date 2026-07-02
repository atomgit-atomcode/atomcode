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
    /// Hard cap on LLM rounds (tool-call iterations) per turn — the round safety fuse.
    /// `None` (default) ⇒ UNLIMITED, matching the kernel's neutral default: how deep to
    /// dig is a per-deployment perf/latency policy, NOT a library decision. Engineering
    /// callers (e.g. a CI/PR pipeline) set a bound via `--max-rounds` to stop a model from
    /// endlessly grepping a large repo; a bare CLI run stays unbounded.
    pub max_rounds: Option<u32>,
    /// Absolute wall-clock cap on the whole review turn. `None` (default) ⇒ UNLIMITED.
    /// Enforced via the kernel's `cancel_token` seam (a timer cancels the turn on deadline),
    /// NOT a kernel change — it's the only guard that also fires while a provider stalls
    /// mid-stream (keepalive bytes keep `stream_timeout`'s idle timer reset). Engineering
    /// callers set it (e.g. `--max-duration 900`); a bare CLI run stays unbounded.
    pub max_turn_duration: Option<std::time::Duration>,
    /// Disable the `web_search` tool for this review. `false` (default) ⇒ web_search is
    /// mounted as before (behavior unchanged). `true` ⇒ the tool is registered but NOT
    /// mounted, so the model cannot call it — used by runtimes where web egress is blocked
    /// or undesirable, so a web_search attempt can't fail and abort the whole review.
    pub no_web: bool,
    /// Auto-degrade threshold for the code-graph tools: they are mounted only when the repo
    /// has AT MOST this many git-tracked indexable source files. Above it, building the
    /// O(repo) tree-sitter call graph would blow the review's wall-clock budget for no
    /// measured quality gain, so the graph tools are dropped (grep-only). `usize::MAX`
    /// (default) ⇒ NO degrade: the graph is always mounted, matching bare-CLI behavior.
    /// Engineering callers (e.g. the service, which reviews huge repos on NFS) set a bound
    /// like `8000` so a kernel-scale repo degrades automatically. `0` ⇒ never mount.
    pub graph_max_indexed_files: usize,
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
            max_rounds: None,
            max_turn_duration: None,
            no_web: false,
            graph_max_indexed_files: usize::MAX, // no degrade by default (bare-CLI behavior)
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
