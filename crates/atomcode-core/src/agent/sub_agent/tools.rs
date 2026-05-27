//! Sub-agent tool filtering logic.
//!
//! Builds a filtered [`ToolRegistry`] for a sub-agent from the parent's
//! tool registry, applying per-policy allow lists and unconditionally
//! removing the `invoke_subagent` tool to prevent recursive spawning.

use crate::agent::sub_agent::types::SubAgentToolPolicy;
use crate::tool::ToolRegistry;

/// Build a filtered [`ToolRegistry`] for a sub-agent.
///
/// * `invoke_subagent` is **always** removed (recursive protection).
/// * `None`              — empty registry (no tools at all).
/// * `ReadOnly`          — read_file, grep, glob, list_dir.
/// * `ReadOnlyWithWeb`   — ReadOnly plus web_fetch, web_search.
/// * `Custom(names)`     — explicit whitelist by tool name.
pub fn build_subagent_tools(
    parent_tools: &ToolRegistry,
    policy: &SubAgentToolPolicy,
) -> ToolRegistry {
    let allowed = match policy {
        SubAgentToolPolicy::None => Vec::new(),
        SubAgentToolPolicy::ReadOnly => {
            vec![
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "list_dir".to_string(),
            ]
        }
        SubAgentToolPolicy::ReadOnlyWithWeb => {
            vec![
                "read_file".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "list_dir".to_string(),
                "web_fetch".to_string(),
                "web_search".to_string(),
            ]
        }
        SubAgentToolPolicy::Custom(names) => names.clone(),
    };

    let registry = ToolRegistry::new();

    // ToolRegistry methods (iter, register_arc) are async.  This function
    // is synchronous by design, so we block on the current tokio runtime —
    // safe because the agent loop that calls this always runs inside one.
    let handle = tokio::runtime::Handle::current();
    handle.block_on(async {
        let parent_entries: Vec<_> = parent_tools.iter().await.collect();

        for (name, tool) in parent_entries {
            // Recursive protection: never propagate invoke_subagent.
            if name == "invoke_subagent" {
                continue;
            }
            // Only keep tools on the allow-list.
            if allowed.contains(&name) {
                registry.register_arc(name, tool).await;
            }
        }
    });

    registry
}
