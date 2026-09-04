//! Configuration for assembling a review agent. Mirrors `atomcode-coding`'s config but
//! for a READ-ONLY reviewer: provider creds, the repo working dir the read tools are
//! scoped to, and liveness bounds.

use std::path::PathBuf;
use std::time::Duration;

use atomcode_kernel::agent::ToolLoopPolicy;

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
    /// Exact no-progress loop policy. `None` permits intentional identical
    /// repetition; any configured round/duration limits remain independent.
    pub tool_loop_policy: Option<ToolLoopPolicy>,
    /// Absolute wall-clock cap on the whole review turn. `None` (default) ⇒ UNLIMITED.
    /// Enforced via the kernel's `cancel_token` seam (a timer cancels the turn on deadline),
    /// NOT a kernel change — it's the only guard that also fires while a provider stalls
    /// mid-stream (keepalive bytes keep `stream_timeout`'s idle timer reset). Engineering
    /// callers set it (e.g. `--max-duration 900`); a bare CLI run stays unbounded.
    pub max_turn_duration: Option<std::time::Duration>,
    /// Optional live progress sink for an embedding tool/driver. Standalone callers leave it
    /// `None`; the in-session `code_review` tool forwards its parent `ToolContext` sink.
    pub progress: Option<atomcode_kernel::tool::ProgressSink>,
    /// Optional stage label for the live activity line — the deep-mode dimension
    /// id (e.g. `security`) or `verify` — so concurrent reviewers are
    /// distinguishable in the shared progress sink. `None` (default) ⇒ the single
    /// reviewer, shown without a label.
    pub progress_label: Option<String>,
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
    /// Skill directories to load `use_skill` / `list_skills` tools from. Empty (default)
    /// ⇒ NO skill tools mounted, matching bare-CLI behavior: only some deployments / repos
    /// opt into skills via `--skill-dir`. Each dir is scanned for `SKILL.md` (directory
    /// skill with bundled `scripts/` / `references/`) or single `<name>.md` files.
    /// LOW→HIGH priority order (later dirs override earlier on name collision).
    pub skill_dirs: Vec<PathBuf>,
    /// Review-scope path allowlist (repo-relative changed/reviewable files). Empty
    /// (default) ⇒ tools only confined to the repo root. Non-empty ⇒ `read_file` /
    /// `grep` / … may only touch these files and their ancestor directories — stops
    /// the model from re-reading files already dropped from the diff (e.g. notes.md).
    pub review_paths: Vec<String>,
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
            tool_loop_policy: default_tool_loop_policy(),
            max_turn_duration: None,
            progress: None,
            progress_label: None,
            no_web: false,
            graph_max_indexed_files: usize::MAX, // no degrade by default (bare-CLI behavior)
            skill_dirs: Vec::new(),              // no skills by default (bare-CLI behavior)
            review_paths: Vec::new(),            // no file allowlist by default (root-only confine)
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

fn default_tool_loop_policy() -> Option<ToolLoopPolicy> {
    resolve_tool_loop_policy(
        std::env::var("ATOMCODE_TOOL_LOOP_WARNING_THRESHOLD")
            .ok()
            .as_deref(),
        std::env::var("ATOMCODE_TOOL_LOOP_STOP_THRESHOLD")
            .ok()
            .as_deref(),
    )
}

pub(crate) fn resolve_tool_loop_policy(
    warning_env: Option<&str>,
    stop_env: Option<&str>,
) -> Option<ToolLoopPolicy> {
    let requested_stop = stop_env.and_then(|value| value.trim().parse::<u32>().ok());
    if requested_stop == Some(0) {
        return None;
    }
    let stop = requested_stop.filter(|value| *value >= 3).unwrap_or(4);
    let fallback_warning = 3.min(stop - 1).max(2);
    let warning = warning_env
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 2 && *value < stop)
        .unwrap_or(fallback_warning);
    Some(
        ToolLoopPolicy::new(warning, stop)
            .expect("resolved review tool-loop thresholds satisfy the policy invariant"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_loop_policy_is_validated_and_can_be_disabled() {
        let custom = resolve_tool_loop_policy(Some("10"), Some("12")).unwrap();
        assert_eq!(custom.warning_threshold(), 10);
        assert_eq!(custom.stop_threshold(), 12);
        assert!(resolve_tool_loop_policy(Some("10"), Some("0")).is_none());

        let fallback = resolve_tool_loop_policy(Some("99"), Some("4")).unwrap();
        assert_eq!(fallback, ToolLoopPolicy::default());
    }
}
