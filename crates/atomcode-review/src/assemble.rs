//! The assembly: wire L1 capabilities into a kernel [`Agent`] per the REVIEW policy — a
//! read-only reviewer that reports structured findings.

use crate::config::ReviewAgentConfig;
use crate::persona::review_persona;
use atomcode_capabilities::codeintel::lang::Lang;
use atomcode_capabilities::codeintel::{codeintel_tool_names, register_codeintel_tools};
use std::path::Path;
use std::process::Command;

/// Whether to mount the code-graph tools: only when the repo has AT MOST `max` indexable
/// source files. `max = usize::MAX` (the config default) ⇒ always mount (bare-CLI behavior);
/// engineering callers pass a bound (e.g. 8000) so a kernel-scale repo degrades to grep — its
/// O(repo) tree-sitter graph build would otherwise blow the wall-clock budget for no measured
/// quality gain (kernel A/B: graph off ≥ on, 1080s CPU → 3.94s).
fn should_mount_graph(indexed_file_count: usize, max: usize) -> bool {
    indexed_file_count <= max
}

/// Count git-tracked source files codeintel would parse, via `git ls-files` (reads the git
/// index — NO working-tree walk, so it stays cheap even on an NFS workdir). Returns 0 when git
/// is unavailable / not a repo ⇒ treated as small ⇒ graph mounted (the safe default).
fn count_indexed_sources(working_dir: &Path) -> usize {
    let out = match Command::new("git")
        .arg("-C")
        .arg(working_dir)
        .args(["ls-files", "-z"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return 0,
    };
    out.split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter(|p| Lang::detect(Path::new(p)).is_some_and(|l| l.is_indexed()))
        .count()
}
use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
use atomcode_capabilities::tools::{
    register_coding_tools, AstGrepTool, ReportFindingTool, WebSearchTool,
};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, ToolRegistry};
use std::sync::Arc;

/// The READ-ONLY tools the reviewer sees. Deliberately NO write/edit/bash/change_dir — a
/// reviewer investigates and reports, it never mutates. (The diff itself is injected as
/// the task by the caller, so the agent needs no shell to obtain it.)
fn review_tool_names(no_web: bool, mount_graph: bool) -> Vec<&'static str> {
    let mut names = vec!["read_file", "grep", "glob", "list_directory", "ast_grep", "report_finding"];
    // web_search is mounted by default; `no_web` drops it (the tool stays registered but
    // unmounted), so a blocked/unreachable web egress can't make the model's web_search
    // call abort the whole review.
    if !no_web {
        names.push("web_search");
    }
    // Code-graph tools (find_references/trace_callers/blast_radius/read_symbol/…) build an
    // O(repo) tree-sitter call graph — only mount them when the repo is small enough to index
    // cheaply (see `should_mount_graph`). On a huge repo they blow the wall-clock budget for
    // ~zero quality gain (measured on the 85k-file kernel: 1080s CPU → 3.94s with them off).
    if mount_graph {
        names.extend(codeintel_tool_names().iter().copied());
    }
    names
}

/// Assemble a runnable review agent from `cfg`. Returns the [`Agent`] AND a
/// [`ReportFindingTool`] HANDLE — the caller reads `handle.findings()` after the run to
/// collect the structured findings the agent reported (the handle shares the tool's inner
/// state with the registered instance).
///
/// `Err` only if the provider fails to construct.
pub fn build_review_agent(cfg: ReviewAgentConfig) -> Result<(Agent, ReportFindingTool), String> {
    let mut provider_cfg = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
    provider_cfg.context_window = cfg.context_window;
    let provider =
        OpenAiCompatProvider::new(provider_cfg).map_err(|e| format!("provider init failed: {}", e.message))?;
    Ok(build_review_agent_with(&cfg, Arc::new(provider)))
}

/// Same review policy as [`build_review_agent`] but with a CALLER-SUPPLIED provider (a
/// mock for tests, or any custom [`LlmProvider`]).
pub fn build_review_agent_with(
    cfg: &ReviewAgentConfig,
    provider: Arc<dyn LlmProvider>,
) -> (Agent, ReportFindingTool) {
    // One shared findings sink: the registered tool and the returned handle share state.
    let report = ReportFindingTool::new();
    // Auto-degrade the code-graph on huge repos when the caller set a bound. Default
    // (usize::MAX) ⇒ always mount, skip the git-index count entirely (bare-CLI: no overhead,
    // behavior unchanged). Engineering callers set `graph_max_indexed_files` to enable it.
    let max_graph_files = cfg.graph_max_indexed_files;
    let mount_graph = if max_graph_files == usize::MAX {
        true
    } else {
        let indexed = count_indexed_sources(&cfg.working_dir); // cheap: reads git index, no tree walk
        let mount = should_mount_graph(indexed, max_graph_files);
        if !mount {
            eprintln!("[codeintel] {indexed} indexed source file(s) > {max_graph_files} — code-graph tools disabled (grep only)");
        }
        mount
    };
    let tools = mount_review_tools(&report, cfg.no_web, mount_graph);
    let persona = compose_persona(cfg);
    let mut builder = Agent::builder()
        .provider(provider)
        .tools(tools)
        .persona(persona)
        .working_dir(cfg.working_dir.clone())
        // Confine read-only tools to the repo: a model `grep /` / read outside the
        // repo is blocked before it runs (prevents whole-container scans → OOM, and
        // out-of-repo reads). Review-agent only; other specializations are untouched.
        .middleware(Arc::new(crate::confine::PathConfineMiddleware::new(
            cfg.working_dir.clone(),
        )))
        .stream_timeout(cfg.stream_timeout)
        .request_timeout(cfg.request_timeout);
    // Round fuse is opt-in: only bound rounds when the caller asked for it (keeps a bare
    // CLI run unbounded; engineering callers pass `--max-rounds`). When bounded, also mount
    // the round-budget pressure hook so the reviewer LANDS findings before the fuse trips
    // instead of dying empty in a read-exploration loop (see `round_budget`).
    if let Some(n) = cfg.max_rounds {
        builder = builder
            .max_rounds(n)
            .hook(Arc::new(crate::round_budget::RoundBudgetHook::new()));
    }
    // Turn total-time cap via the kernel's cancel seam (no kernel change): inject a cancel
    // token and fire it after the deadline. This trips run_turn's stream-select cancel arm
    // even while a stalled provider keeps the stream alive — the gap `stream_timeout`/`max_rounds`
    // leave open. On cancel the turn stops as `StopReason::Cancelled`, keeping findings so far.
    if let Some(d) = cfg.max_turn_duration {
        let token = tokio_util::sync::CancellationToken::new();
        builder = builder.cancel_token(token.clone());
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            token.cancel();
        });
    }
    let agent = builder.build();
    (agent, report)
}

/// Register the read-only review toolset (+ the shared `report_finding` instance) and
/// mount only the read-only subset — write/edit/bash are registered by
/// `register_coding_tools` but NEVER mounted, so the model cannot mutate.
fn mount_review_tools(report: &ReportFindingTool, no_web: bool, mount_graph: bool) -> MountedTools {
    let mut reg = ToolRegistry::new();
    register_coding_tools(&mut reg); // read_file/grep/glob/list_directory (+ write/edit/bash, unmounted)
    register_codeintel_tools(&mut reg); // registered always; mounted only when `mount_graph`
    reg.register(Arc::new(AstGrepTool));
    reg.register(Arc::new(WebSearchTool::new())); // registered always; mounted only when !no_web
    reg.register(Arc::new(report.clone())); // shares state with the returned handle
    reg.mount(&review_tool_names(no_web, mount_graph))
}

/// Final system prompt: full override wins (else the built-in reviewer persona), then the
/// append section (the normal customization channel: domain rules / ignore lists / repo
/// style guides / PR metadata) composes on top of either base.
fn compose_persona(cfg: &ReviewAgentConfig) -> String {
    let mut persona = cfg.persona.clone().unwrap_or_else(|| review_persona(&cfg.model));
    if let Some(append) = cfg.persona_append.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        persona.push_str("\n\n");
        persona.push_str(append);
    }
    persona
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use atomcode_kernel::agent::AutoRespond;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::{ToolCall, ToolDef};
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;

    fn cfg() -> ReviewAgentConfig {
        ReviewAgentConfig::new("k", "https://x.test", "mock-model", std::env::temp_dir())
    }

    /// Scripted provider: round 1 emits a `report_finding` tool call, round 2 a final text.
    struct ScriptedReviewProvider;
    #[async_trait]
    impl LlmProvider for ScriptedReviewProvider {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            // After the tool result comes back, the history grows → emit the final answer.
            let has_tool_result = messages.iter().any(|m| matches!(m.role, atomcode_kernel::message::Role::Tool));
            let evs = if has_tool_result {
                vec![StreamEvent::TextDelta("Review complete: 1 P1 finding.".into()), StreamEvent::Done { truncated: false }]
            } else {
                vec![
                    StreamEvent::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "report_finding".into(),
                        arguments: r#"{"title":"fix: unchecked unwrap","body":"x may be None","priority":"P1","confidence":0.9,"file_path":"src/a.rs","line_start":10,"line_end":12}"#.into(),
                    }),
                    StreamEvent::Done { truncated: false },
                ]
            };
            Ok(stream::iter(evs).boxed())
        }
    }

    #[tokio::test]
    async fn review_agent_collects_findings_via_handle() {
        let (agent, report) = build_review_agent_with(&cfg(), Arc::new(ScriptedReviewProvider));
        let outcome = agent
            .run_to_completion("Review this diff:\n+ x.unwrap()", AutoRespond::AllowAll)
            .await;
        assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
        // The finding the agent reported is readable through the returned handle.
        let findings = report.findings();
        assert_eq!(findings.len(), 1, "one finding collected");
        assert_eq!(findings[0].priority, "P1");
        assert_eq!(findings[0].file_path, "src/a.rs");
        assert!(findings[0].title.contains("unchecked unwrap"));
    }

    /// Never terminates on its own: every round emits another tool call. Only `max_rounds`
    /// can stop it — guards that the review path actually wires the round fuse through.
    struct LoopingProvider;
    #[async_trait]
    impl LlmProvider for LoopingProvider {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let evs = vec![
                StreamEvent::ToolCall(ToolCall {
                    id: format!("loop{}", messages.len()),
                    name: "grep".into(),
                    arguments: r#"{"pattern":"zzz"}"#.into(),
                }),
                StreamEvent::Done { truncated: false },
            ];
            Ok(stream::iter(evs).boxed())
        }
    }

    #[tokio::test]
    async fn max_rounds_stops_runaway_review() {
        let mut c = cfg();
        c.max_rounds = Some(2);
        let (agent, _report) = build_review_agent_with(&c, Arc::new(LoopingProvider));
        let outcome = agent
            .run_to_completion("Review this diff:\n+ x", AutoRespond::AllowAll)
            .await;
        assert_eq!(
            outcome.stop,
            atomcode_kernel::event::StopReason::MaxRounds,
            "endless tool calls must be capped by max_rounds"
        );
    }

    /// Never yields a stream event (a provider that holds the connection but makes no
    /// progress — what `stream_timeout` can't catch if keepalive bytes arrive). Only the
    /// turn-duration cancel can stop it.
    struct StallProvider;
    #[async_trait]
    impl LlmProvider for StallProvider {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            _m: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            Ok(stream::pending::<StreamEvent>().boxed())
        }
    }

    #[tokio::test]
    async fn max_turn_duration_stops_stalled_review() {
        let mut c = cfg();
        c.max_turn_duration = Some(std::time::Duration::from_millis(150));
        let (agent, _report) = build_review_agent_with(&c, Arc::new(StallProvider));
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent.run_to_completion("Review this diff:\n+ x", AutoRespond::AllowAll),
        )
        .await
        .expect("turn-duration cancel must end the run, not hang");
        assert_eq!(
            outcome.stop,
            atomcode_kernel::event::StopReason::Cancelled,
            "a stalled stream must be cut by the turn-duration cancel"
        );
    }

    /// Records the messages of the LAST request it received, then stops (text-only round).
    /// Lets a test observe what `pre_request` hooks projected onto the wire.
    #[derive(Clone)]
    struct CapturingProvider {
        seen: Arc<std::sync::Mutex<Vec<Message>>>,
    }
    #[async_trait]
    impl LlmProvider for CapturingProvider {
        fn model_name(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            *self.seen.lock().unwrap() = messages.to_vec();
            let evs = vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::Done { truncated: false },
            ];
            Ok(stream::iter(evs).boxed())
        }
    }

    #[tokio::test]
    async fn round_budget_reminder_injected_when_bounded() {
        // max_rounds=1 → round 1 IS the final round → the budget hook fires this request.
        let mut c = cfg();
        c.max_rounds = Some(1);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider { seen: seen.clone() };
        let (agent, _report) = build_review_agent_with(&c, Arc::new(provider));
        let _ = agent
            .run_to_completion("Review this diff:\n+ x", AutoRespond::AllowAll)
            .await;
        let text: String = seen.lock().unwrap().iter().map(|m| m.text.clone()).collect();
        assert!(
            text.contains("[review budget]"),
            "bounded run must project the round-budget reminder onto the wire: {text}"
        );
    }

    #[tokio::test]
    async fn no_round_budget_reminder_when_unbounded() {
        // No max_rounds → no fuse, no budget hook mounted → clean wire.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = CapturingProvider { seen: seen.clone() };
        let (agent, _report) = build_review_agent_with(&cfg(), Arc::new(provider));
        let _ = agent
            .run_to_completion("Review this diff:\n+ x", AutoRespond::AllowAll)
            .await;
        let text: String = seen.lock().unwrap().iter().map(|m| m.text.clone()).collect();
        assert!(
            !text.contains("[review budget]"),
            "unbounded run must not inject the reminder: {text}"
        );
    }

    #[test]
    fn review_mounts_readonly_set_only() {
        // The mounted names are read-only — no mutation tools.
        let names = review_tool_names(false, true);
        assert!(names.contains(&"read_file") && names.contains(&"report_finding") && names.contains(&"ast_grep"));
        for forbidden in ["write_file", "edit_file", "bash", "change_dir", "search_replace", "parallel_edit_files"] {
            assert!(!names.contains(&forbidden), "reviewer must not mount `{forbidden}`");
        }
    }

    #[test]
    fn no_web_drops_web_search_only() {
        // 默认挂 web_search；no_web=true 时仅去掉 web_search，其余只读工具不变。
        let with_web = review_tool_names(false, true);
        assert!(with_web.contains(&"web_search"), "默认应挂 web_search");

        let without_web = review_tool_names(true, true);
        assert!(!without_web.contains(&"web_search"), "no_web 应去掉 web_search");
        // 其它只读工具仍在
        for keep in ["read_file", "grep", "glob", "list_directory", "ast_grep", "report_finding"] {
            assert!(without_web.contains(&keep), "no_web 不应误伤 `{keep}`");
        }
    }

    #[test]
    fn graph_tools_gated_by_mount_flag() {
        // mount_graph=false drops the code-graph tools; the base read-only set stays.
        let with_graph = review_tool_names(true, true);
        let without_graph = review_tool_names(true, false);
        // codeintel names present with the flag on, absent with it off.
        let graph_names = atomcode_capabilities::codeintel::codeintel_tool_names();
        assert!(graph_names.iter().all(|g| with_graph.contains(g)), "graph tools mounted when on");
        assert!(graph_names.iter().all(|g| !without_graph.contains(g)), "graph tools dropped when off");
        // Base tools unaffected either way.
        for keep in ["read_file", "grep", "report_finding"] {
            assert!(without_graph.contains(&keep), "base tool `{keep}` must survive");
        }
    }

    #[test]
    fn should_mount_graph_thresholds() {
        // Unlimited (bare-CLI default) → always mount, even kernel-scale.
        assert!(should_mount_graph(85_000, usize::MAX), "unlimited → always mount");
        // Bounded (engineering caller, e.g. service sets 8000).
        assert!(should_mount_graph(0, 8000), "empty/unknown repo → mount");
        assert!(should_mount_graph(8000, 8000), "at threshold → still mount");
        assert!(!should_mount_graph(8001, 8000), "over threshold → degrade");
        assert!(!should_mount_graph(85_000, 8000), "kernel-scale → degrade");
        assert!(!should_mount_graph(1, 0), "max=0 → never mount");
    }

    #[test]
    fn count_indexed_sources_counts_only_source_files() {
        // Skip cleanly if git isn't on PATH.
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            Command::new("git").current_dir(root).args(args).output().unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        for f in ["a.rs", "b.go", "c.py", "d.md", "e.lock", "sub/f.ts"] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        // Only indexable sources counted: a.rs, b.go, c.py, sub/f.ts = 4 (md/lock excluded).
        assert_eq!(count_indexed_sources(root), 4);
    }
}

#[cfg(test)]
mod persona_compose_tests {
    use super::*;

    fn cfg() -> ReviewAgentConfig {
        ReviewAgentConfig::new("k", "https://x.test", "m1", std::env::temp_dir())
    }

    #[test]
    fn default_is_builtin() {
        assert_eq!(compose_persona(&cfg()), review_persona("m1"));
    }

    #[test]
    fn append_composes_on_builtin() {
        let c = cfg().with_persona_append("## Domain Rules\n- no fmt nits");
        let p = compose_persona(&c);
        assert!(p.starts_with("You are AtomCode Reviewer"), "built-in stays as the base");
        assert!(p.ends_with("## Domain Rules\n- no fmt nits"), "append goes last");
    }

    #[test]
    fn override_replaces_builtin_and_append_still_composes() {
        let c = cfg().with_persona("CUSTOM").with_persona_append("EXTRA");
        assert_eq!(compose_persona(&c), "CUSTOM\n\nEXTRA");
    }

    #[test]
    fn blank_append_leaves_no_residue() {
        let c = cfg().with_persona_append("   \n  ");
        assert_eq!(compose_persona(&c), review_persona("m1"));
    }
}
