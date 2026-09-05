//! The plugin catalog this build ships. Which of them *run* is the config's call.

pub mod agent_loop;
pub mod agents;
pub mod capabilities;
pub mod findings;
pub mod llm;
pub mod loop_policy;
pub mod persona;
pub mod policy;
pub mod policy_rows;
pub mod recovery;
pub mod registries;
pub mod session;
pub mod subagent;
pub mod tool_exec;
pub mod tools;
pub mod truncation;
pub mod ui;
pub mod ui_jsonrpc;
pub mod ui_tui;
pub mod ui_web;
pub mod world;
pub mod world_tools;

use std::sync::Arc;

use atomcode_plexus::PluginRegistry;

/// Every plugin compiled into this binary.
///
/// The registry is the build's contribution; the config tree is the user's. A
/// plugin here that no row names is inert — it costs a `HashMap` entry and
/// nothing else.
pub fn catalog() -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    registry
        .register(Arc::new(registries::ToolsPlugin))
        .register(Arc::new(registries::SystemPromptPlugin))
        .register(Arc::new(agents::AgentsPlugin))
        .register(Arc::new(session::SessionPlugin))
        .register(Arc::new(session::SessionProjectionsPlugin))
        .register(Arc::new(session::SessionPersistenceJsonlPlugin))
        .register(Arc::new(llm::OpenAiCompatPlugin))
        .register(Arc::new(llm::AtomcodeConfigPlugin))
        .register(Arc::new(llm::ReplayPlugin))
        .register(Arc::new(world::FsLocalPlugin))
        .register(Arc::new(world::FsReadOnlyPlugin))
        .register(Arc::new(world::SubprocessLocalPlugin))
        .register(Arc::new(world::BashLocalPlugin))
        .register(Arc::new(world_tools::FsWorldToolsPlugin))
        .register(Arc::new(world_tools::BashWorldToolPlugin))
        .register(Arc::new(tools::FsToolsPlugin))
        .register(Arc::new(tools::SearchToolsPlugin))
        .register(Arc::new(tools::AstGrepPlugin))
        .register(Arc::new(tools::BashToolPlugin))
        .register(Arc::new(capabilities::SkillsPlugin))
        .register(Arc::new(capabilities::CodeIntelPlugin))
        .register(Arc::new(capabilities::CodeGraphPlugin))
        .register(Arc::new(capabilities::WebPlugin))
        .register(Arc::new(capabilities::MemoryPlugin))
        .register(Arc::new(capabilities::McpPlugin))
        .register(Arc::new(findings::FindingsPlugin))
        .register(Arc::new(subagent::SubagentPlugin))
        .register(Arc::new(persona::CodingPersonaPlugin))
        .register(Arc::new(persona::ReviewPersonaPlugin))
        .register(Arc::new(persona::SecurityPersonaPlugin))
        .register(Arc::new(agent_loop::AgentLoopPlugin))
        .register(Arc::new(policy::RepairArgsPlugin))
        .register(Arc::new(policy::ApprovalPlugin))
        .register(Arc::new(policy::ResultCapPlugin))
        .register(Arc::new(tool_exec::ParallelToolsPlugin))
        .register(Arc::new(loop_policy::RoundCapPlugin))
        .register(Arc::new(loop_policy::RetryPlugin))
        .register(Arc::new(recovery::RateLimitPlugin))
        .register(Arc::new(recovery::OverflowPlugin))
        .register(Arc::new(recovery::RequestTimeoutPlugin))
        .register(Arc::new(truncation::TruncationPlugin))
        .register(Arc::new(recovery::ReasoningFilterPlugin))
        .register(Arc::new(loop_policy::CompactionPlugin))
        .register(Arc::new(loop_policy::ToolLoopGuardPlugin))
        .register(Arc::new(loop_policy::RepeatFusePlugin))
        .register(Arc::new(policy_rows::TodoPlugin))
        .register(Arc::new(policy_rows::PermissionsPlugin))
        .register(Arc::new(policy_rows::PlanModePlugin))
        .register(Arc::new(policy_rows::SessionTitlePlugin))
        .register(Arc::new(policy_rows::UnattendedQuestionsPlugin))
        .register(Arc::new(policy_rows::InteractiveApprovalPlugin))
        .register(Arc::new(policy_rows::TelemetryPlugin))
        .register(Arc::new(policy_rows::TokenBudgetPlugin))
        .register(Arc::new(ui::OneShotUiPlugin))
        .register(Arc::new(ui::ReplUiPlugin))
        .register(Arc::new(ui::TerminalQuestionsPlugin))
        .register(Arc::new(ui::QuietUiPlugin))
        .register(Arc::new(ui_jsonrpc::JsonRpcUiPlugin))
        .register(Arc::new(ui_tui::TuiUiPlugin))
        .register(Arc::new(ui_tui::TuiQuestionsPlugin))
        .register(Arc::new(ui_web::WebUiPlugin))
        .register(Arc::new(trace::TracePlugin));
    registry
}

pub mod trace;
