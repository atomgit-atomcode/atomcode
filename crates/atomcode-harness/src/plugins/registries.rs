//! The three registry plugins. Each one does nothing but own a slot — which is
//! exactly why they are separable: a deployment can swap the session store for a
//! persistent one without the tool catalog knowing.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

use serde::Deserialize;

use crate::seams::{
    CommandsSvc, Descriptions, OperationsSvc, PromptRegistry, SystemPromptSvc, ToolBox, ToolPolicy,
    ToolsSvc,
};

pub struct ToolsPlugin;

/// Which tool names this tree's catalog admits.
///
/// On the catalog's own row rather than on each tool row, because the switch a
/// tree has is the row and a row mounts several tools — and because an MCP
/// server's tools are published at runtime, by a row whose name list nobody
/// wrote. `*` matches any run of characters:
///
/// ```toml
/// [[patch]]
/// id = "tools"
/// config = { exclude = ["write_file", "mcp__github__*"] }
/// ```
#[derive(Debug, Deserialize, Default)]
struct ToolsRow {
    /// If non-empty, the only names admitted.
    #[serde(default)]
    include: Vec<String>,
    /// Names never admitted. Beats `include`.
    #[serde(default)]
    exclude: Vec<String>,
}

#[async_trait]
impl Plugin for ToolsPlugin {
    fn name(&self) -> &'static str {
        "tools"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Only to say what it dropped, and only when it dropped something.
        &["operations"]
    }
    fn description(&self) -> &'static str {
        "the live tool catalog every tool plugin registers into"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ToolsRow = if config.is_null() {
            ToolsRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let policy = ToolPolicy::new(row.include, row.exclude);
        let open = policy.is_open();
        let _ = ctx
            .provide::<ToolsSvc>(Arc::new(ToolBox::with_policy(policy)))
            .map_err(|e| e.to_string())?;
        // A narrowed catalog says so, and says it from the live catalog rather
        // than from the config: what an MCP server offered and this policy kept
        // out is only known once that server has answered.
        if !open {
            crate::plugins::self_knowledge::describes_live(
                ctx,
                crate::seams::Aspect::Operations,
                "tool-catalog",
                14,
                |ctx| {
                    let dropped = ctx.service::<ToolsSvc>()?.turned_away();
                    if dropped.is_empty() {
                        return Some(
                            "TOOL CATALOG — this tree was configured to narrow the tool \
                             catalog (`tools` row, `include` / `exclude`). Nothing has been \
                             kept out so far."
                                .to_string(),
                        );
                    }
                    Some(format!(
                        "TOOL CATALOG — the `tools` row was configured to keep these out of \
                         this tree, so they are not yours to call and saying you have them \
                         would be wrong: {}. The person changes that where the rows are \
                         configured, not at runtime.",
                        dropped.join(", ")
                    ))
                },
            );
        }
        Ok(())
    }
}

pub struct SystemPromptPlugin;

#[async_trait]
impl Plugin for SystemPromptPlugin {
    fn name(&self) -> &'static str {
        "system-prompt"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "ordered prompt fragments, contributed by whoever owns the behaviour"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<SystemPromptSvc>(Arc::new(PromptRegistry::new()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub struct CommandsPlugin;

#[async_trait]
impl Plugin for CommandsPlugin {
    fn name(&self) -> &'static str {
        "commands"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["commands"]
    }
    fn description(&self) -> &'static str {
        "the commands a person can run from a front end, each registered by the row it belongs to"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<CommandsSvc>(Arc::new(crate::commands::CommandCatalog::new()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub struct OperationsPlugin;

#[async_trait]
impl Plugin for OperationsPlugin {
    fn name(&self) -> &'static str {
        "operations"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["operations"]
    }
    fn description(&self) -> &'static str {
        "where each row describes itself, for `describe_self` to answer with"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<OperationsSvc>(Arc::new(Descriptions::new()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
