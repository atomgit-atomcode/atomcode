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
///
/// This function is async because it calls async `ToolRegistry` methods.
/// It must NOT use `Handle::block_on`, which panics when called from
/// within an async context (e.g. inside a `tokio::spawn` task).
pub async fn build_subagent_tools(
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
        SubAgentToolPolicy::Custom(names) => {
            const DESTRUCTIVE: &[&str] = &[
                "bash", "write_file", "edit_file", "search_replace",
                "parallel_edit_files", "delete_file", "rename_file",
                "create_file", "move_file", "copy_file",
                "git_auto_commit", "git_checkpoint",
            ];
            names.iter()
                .filter(|n| {
                    if DESTRUCTIVE.contains(&n.as_str()) {
                        tracing::warn!(
                            "SubAgentToolPolicy::Custom: rejected destructive tool '{}'",
                            n,
                        );
                        false
                    } else {
                        true
                    }
                })
                .cloned()
                .collect()
        }
    };

    let registry = ToolRegistry::new();

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

    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::sub_agent::types::SubAgentToolPolicy;

    /// Build a parent ToolRegistry with the given tool names (sync because
    /// tokio::test is already in a runtime — we use block_on to register).
    async fn register_tools(names: &[&str]) -> ToolRegistry {
        use crate::tool::{Tool, ToolDef, ToolContext, ToolResult, ApprovalRequirement};
        use async_trait::async_trait;

        struct NamedTool {
            name: String,
        }

        #[async_trait]
        impl Tool for NamedTool {
            fn definition(&self) -> ToolDef {
                ToolDef {
                    name: Box::leak(self.name.clone().into_boxed_str()),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                }
            }

            fn approval(&self, _args: &str) -> ApprovalRequirement {
                ApprovalRequirement::AutoApprove
            }

            async fn execute(
                &self,
                _args: &str,
                _ctx: &ToolContext,
            ) -> anyhow::Result<ToolResult> {
                Ok(ToolResult {
                    call_id: String::new(),
                    output: "ok".into(),
                    success: true,
                })
            }
        }

        let reg = ToolRegistry::new();
        for &name in names {
            let tool: Box<dyn Tool> = Box::new(NamedTool { name: name.into() });
            reg.register(tool).await;
        }
        reg
    }

    /// Determine allowed tool names for a policy, mirroring
    /// `build_subagent_tools` logic, but async-friendly so we don't
    /// need `Handle::block_on` inside a tokio runtime.
    async fn allowed_for_policy(
        parent: &ToolRegistry,
        policy: &SubAgentToolPolicy,
    ) -> Vec<String> {
        let whitelist = match policy {
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

        let parent_entries: Vec<_> = parent.iter().await.collect();
        parent_entries
            .into_iter()
            .filter(|(name, _)| name != "invoke_subagent")
            .filter(|(name, _)| whitelist.contains(name))
            .map(|(name, _)| name)
            .collect()
    }

    #[tokio::test]
    async fn test_readonly_policy_has_no_write_tools() {
        let parent = register_tools(&[
            "read_file", "write_file", "grep", "glob", "list_dir",
        ])
        .await;

        let entries = allowed_for_policy(&parent, &SubAgentToolPolicy::ReadOnly).await;
        assert!(entries.contains(&"read_file".to_string()));
        assert!(entries.contains(&"grep".to_string()));
        assert!(entries.contains(&"glob".to_string()));
        assert!(entries.contains(&"list_dir".to_string()));
        assert!(
            !entries.contains(&"write_file".to_string()),
            "write_file must NOT be in ReadOnly filtered tools"
        );
    }

    #[tokio::test]
    async fn test_readonly_with_web_policy() {
        let parent = register_tools(&[
            "read_file", "write_file", "grep", "glob", "list_dir",
            "web_fetch", "web_search",
        ])
        .await;

        let entries = allowed_for_policy(&parent, &SubAgentToolPolicy::ReadOnlyWithWeb).await;
        assert!(entries.contains(&"read_file".to_string()));
        assert!(entries.contains(&"web_fetch".to_string()));
        assert!(entries.contains(&"web_search".to_string()));
        assert!(
            !entries.contains(&"write_file".to_string()),
            "write_file must NOT be in ReadOnlyWithWeb filtered tools"
        );
    }

    #[tokio::test]
    async fn test_invoke_subagent_always_removed() {
        let parent = register_tools(&["read_file", "invoke_subagent"]).await;

        let entries = allowed_for_policy(&parent, &SubAgentToolPolicy::ReadOnly).await;
        assert!(entries.contains(&"read_file".to_string()));
        assert!(
            !entries.contains(&"invoke_subagent".to_string()),
            "invoke_subagent must always be removed from subagent tools"
        );
    }
}
