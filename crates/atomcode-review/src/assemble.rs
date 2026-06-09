//! The assembly: wire L1 capabilities into a kernel [`Agent`] per the REVIEW policy — a
//! read-only reviewer that reports structured findings.

use crate::config::ReviewAgentConfig;
use crate::persona::review_persona;
use atomcode_capabilities::codeintel::{codeintel_tool_names, register_codeintel_tools};
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
fn review_tool_names() -> Vec<&'static str> {
    let mut names = vec!["read_file", "grep", "glob", "list_directory", "ast_grep", "web_search", "report_finding"];
    names.extend(codeintel_tool_names().iter().copied());
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
    let tools = mount_review_tools(&report);
    // Full override wins; else the built-in reviewer persona.
    let persona = cfg.persona.clone().unwrap_or_else(|| review_persona(&cfg.model));
    let agent = Agent::builder()
        .provider(provider)
        .tools(tools)
        .persona(persona)
        .working_dir(cfg.working_dir.clone())
        .stream_timeout(cfg.stream_timeout)
        .request_timeout(cfg.request_timeout)
        .build();
    (agent, report)
}

/// Register the read-only review toolset (+ the shared `report_finding` instance) and
/// mount only the read-only subset — write/edit/bash are registered by
/// `register_coding_tools` but NEVER mounted, so the model cannot mutate.
fn mount_review_tools(report: &ReportFindingTool) -> MountedTools {
    let mut reg = ToolRegistry::new();
    register_coding_tools(&mut reg); // read_file/grep/glob/list_directory (+ write/edit/bash, unmounted)
    register_codeintel_tools(&mut reg);
    reg.register(Arc::new(AstGrepTool));
    reg.register(Arc::new(WebSearchTool::new()));
    reg.register(Arc::new(report.clone())); // shares state with the returned handle
    reg.mount(&review_tool_names())
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

    #[test]
    fn review_mounts_readonly_set_only() {
        // The mounted names are read-only — no mutation tools.
        let names = review_tool_names();
        assert!(names.contains(&"read_file") && names.contains(&"report_finding") && names.contains(&"ast_grep"));
        for forbidden in ["write_file", "edit_file", "bash", "change_dir", "search_replace", "parallel_edit_files"] {
            assert!(!names.contains(&forbidden), "reviewer must not mount `{forbidden}`");
        }
    }
}
