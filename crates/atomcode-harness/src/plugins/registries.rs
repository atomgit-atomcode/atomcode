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
        mount_catalog(ctx, config, None)
    }
}

/// Build this tree's tool catalog and put it in the `tools` slot.
///
/// Shared with the host rows that build the same catalog over switches the host
/// holds (`atomcode-coding`'s `tools-host`): a second copy of "parse the row,
/// apply the policy, register `/tools`" is a second place for the two to drift.
pub fn mount_catalog(
    ctx: &Context,
    config: &Value,
    switches: Option<Arc<crate::seams::ToolSwitches>>,
) -> Result<(), String> {
    let row: ToolsRow = if config.is_null() {
        ToolsRow::default()
    } else {
        serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
    };
    let policy = ToolPolicy::new(row.include, row.exclude);
    let open = policy.is_open();
    // Switches a host holds outlive this catalog; without one they live and die
    // with it, which is the ordinary case for a tree nobody rebuilds.
    let catalog = Arc::new(match switches {
        Some(switches) => ToolBox::with_policy_and_switches(policy, switches),
        None => ToolBox::with_policy(policy),
    });
    let _ = ctx
        .provide::<ToolsSvc>(catalog.clone())
        .map_err(|e| e.to_string())?;
    crate::commands::register(ctx, Arc::new(ToolsCommand(catalog)))?;
    // A narrowed catalog says so, and says it from the live catalog: what an
    // MCP server offered and this tree kept out is only known once that server
    // has answered, and what the person turned off can change any time. Always
    // registered, and silent when there is nothing to report — `open` only
    // decides whether the CONFIG can drop anything, and the person's switch is
    // not the config.
    let _ = open;
    crate::plugins::self_knowledge::describes_live(
        ctx,
        crate::seams::Aspect::Operations,
        "tool-catalog",
        14,
        |ctx| {
            let catalog = ctx.service::<ToolsSvc>()?;
            let dropped = catalog.turned_away();
            let held = catalog.held_back();
            if dropped.is_empty() && held.is_empty() {
                return None;
            }
            let mut said = String::from(
                "TOOL CATALOG — some tools are not in this tree, so they are not yours to \
                 call and saying you have them would be wrong.",
            );
            if !dropped.is_empty() {
                said.push_str(&format!(
                    " Kept out by the config, and only the person editing it can change that: {}.",
                    dropped.join(", ")
                ));
            }
            if !held.is_empty() {
                said.push_str(&format!(
                    " Turned off by the person for this session, who can put them back with \
                     `/toolbox on`: {}.",
                    held.join(", ")
                ));
            }
            Some(said)
        },
    );
    Ok(())
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

/// `/toolbox` — what the model can call right now, and the person's switch over
/// it.
///
/// Not `/tools`: a front end may already use that for how tool output is
/// *shown* (the shipped screen does), and two commands one letter apart in
/// meaning is worse than a name that says "the box the tools are in".
///
/// Registered by the row that owns the catalog, which is the only place that
/// knows what is in it. Deliberately **not** a tool: an agent that can put its
/// own tools back has not been restricted, and one that can take them away has
/// a way to fail quietly that nobody asked for.
struct ToolsCommand(Arc<ToolBox>);

#[async_trait]
impl crate::commands::CatalogCommand for ToolsCommand {
    fn describe(&self) -> atomcode_kernel::agent::CommandDescription {
        atomcode_kernel::agent::CommandDescription {
            name: "toolbox".into(),
            usage: Some("[off|on <名字或 mcp__server__*>]".into()),
            summary: "模型现在能调哪些工具,以及临时关掉/放回其中一些".into(),
            target: atomcode_kernel::agent::CommandTarget::Session,
        }
    }

    async fn run(&self, _agent: Arc<crate::agent::Agent>, args: &str) -> Result<String, String> {
        let args = args.trim();
        let (verb, pattern) = match args.split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (args, ""),
        };
        match verb {
            "" => Ok(self.listing()),
            "off" | "on" if pattern.is_empty() => {
                Err(format!("`{verb}` 要一个名字或模式,例如 `mcp__github__*`"))
            }
            "off" => {
                let hidden = self.0.turn_off(pattern);
                if hidden.is_empty() {
                    // The switch is still recorded: a server that connects later
                    // is what the person is usually aiming at.
                    return Ok(format!(
                        "现在没有工具叫 `{pattern}`。开关已记下,之后注册的同名工具也不会进来。"
                    ));
                }
                Ok(format!("关掉了:{}", hidden.join("、")))
            }
            "on" => {
                let back = self.0.turn_on(pattern);
                if back.is_empty() {
                    let away = self.0.turned_away();
                    if away.iter().any(|name| name.ends_with(pattern)) {
                        return Err(format!(
                            "`{pattern}` 是这棵树的配置排除掉的,不是本次会话关掉的 —— 改配置才能放回来"
                        ));
                    }
                    return Ok(format!("没有被关掉的工具叫 `{pattern}`"));
                }
                Ok(format!("放回来了:{}", back.join("、")))
            }
            other => Err(format!("不认识 `{other}`,只有 `off` 和 `on`")),
        }
    }
}

impl ToolsCommand {
    fn listing(&self) -> String {
        let mut out = String::new();
        let live = self.0.names();
        out.push_str(&format!("能调的({}):{}\n", live.len(), live.join("、")));
        let held = self.0.held_back();
        if !held.is_empty() {
            out.push_str(&format!(
                "本次会话关掉的({}):{} —— `/toolbox on <名字>` 放回来\n",
                held.len(),
                held.join("、")
            ));
        }
        let away = self.0.turned_away();
        if !away.is_empty() {
            out.push_str(&format!(
                "配置排除的({}):{} —— 改配置才能放回来\n",
                away.len(),
                away.join("、")
            ));
        }
        out
    }
}
