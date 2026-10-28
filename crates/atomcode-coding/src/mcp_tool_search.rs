//! Request-only MCP and AtomGit schema selection. The admitted tool catalog and native
//! execution/approval path remain authoritative. Selection is derived from the
//! caller's paired search results, so it needs no session state or disk format.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_config::config::McpToolSearchMode;
use atomcode_harness::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use atomcode_harness::seams::{LlmSvc, ToolsSvc};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::tool::{Tool, ToolContext, ToolDef, ToolResult};
use atomcode_plexus::{Context, Next, Waterfall};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const SEARCH: &str = "mcp_search";

#[derive(Clone, Copy)]
enum Catalog {
    Mcp,
    Builtin,
}
impl Catalog {
    fn search_name(self) -> &'static str {
        match self {
            Self::Mcp => SEARCH,
            Self::Builtin => "builtin_search",
        }
    }
    fn contains(self, name: &str) -> bool {
        match self {
            Self::Mcp => is_mcp(name),
            Self::Builtin => matches!(
                name,
                "atomgit_repo" | "atomgit_issue" | "atomgit_pr" | "atomgit_api"
            ),
        }
    }
}
const DEFAULT_MATCHES: usize = 8;
const MAX_MATCHES: usize = 32;
const DESCRIPTION_CHARS: usize = 240;

fn is_mcp(name: &str) -> bool {
    name.starts_with("mcp__")
}

/// JSON bytes / 4 is a size estimate, not provider billing/tokenization. Use the
/// live caller's model (including delegated realms), with a documented fallback.
fn schema_budget(window: u32) -> usize {
    let window = if window == 0 { 128_000 } else { window };
    (window.max(1_024) / 10) as usize * 4
}

fn schema_bytes(tool: &ToolDef) -> usize {
    // ToolDef contains only JSON values and strings: serialization cannot fail.
    serde_json::to_vec(tool)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<usize>,
    offset: usize,
}

#[derive(Serialize)]
struct Match {
    name: String,
    description: String,
    schema_bytes: usize,
    loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

#[derive(Serialize, Deserialize)]
struct Selection {
    loaded_tools: Vec<String>,
}

#[cfg(test)]
fn search(tools: &[ToolDef], args: SearchArgs, budget: usize) -> Value {
    search_in(tools, args, budget, Catalog::Mcp)
}
fn search_in(tools: &[ToolDef], args: SearchArgs, budget: usize, catalog: Catalog) -> Value {
    // A named built-in is a deliberate selection, not a keyword to match in
    // other tools' cross-references. Keep MCP's existing keyword semantics.
    let named = match catalog {
        Catalog::Builtin => args
            .query
            .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
            .map(str::to_lowercase)
            .filter(|name| catalog.contains(name))
            .collect::<BTreeSet<_>>(),
        Catalog::Mcp => BTreeSet::new(),
    };
    let terms = args
        .query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let mut ranked = tools
        .iter()
        .filter(|tool| catalog.contains(&tool.name))
        .filter(|tool| named.is_empty() || named.contains(&tool.name))
        .filter_map(|tool| {
            let name = tool.name.to_lowercase();
            let description = tool.description.to_lowercase();
            let score = terms
                .iter()
                .map(|term| {
                    if name == *term {
                        100
                    } else if name.contains(term) {
                        10
                    } else if description.contains(term) {
                        1
                    } else {
                        0
                    }
                })
                .sum::<usize>();
            (!named.is_empty() || terms.is_empty() || score > 0).then_some((score, tool))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(a_score, a), (b_score, b)| {
        b_score.cmp(a_score).then_with(|| a.name.cmp(&b.name))
    });
    let total = ranked.len();
    let limit = args.limit.unwrap_or(DEFAULT_MATCHES).clamp(1, MAX_MATCHES);
    let mut remaining = budget;
    let matches = ranked.into_iter().skip(args.offset).take(limit).map(|(_, tool)| {
        let bytes = schema_bytes(tool);
        let loaded = bytes <= remaining;
        if loaded { remaining -= bytes; }
        Match {
            name: tool.name.clone(),
            description: tool.description.chars().take(DESCRIPTION_CHARS).collect(),
            schema_bytes: bytes,
            loaded,
            reason: (!loaded).then_some(match catalog {
                Catalog::Mcp => "schema exceeds the remaining load budget; search more narrowly or use limit=1; tools.mcp.tool_search=off restores eager loading",
                Catalog::Builtin => "schema exceeds the remaining load budget; search more narrowly or use limit=1; tools.atomgit.tool_search=false restores eager loading",
            }),
        }
    }).collect::<Vec<_>>();
    let selection = Selection {
        loaded_tools: matches
            .iter()
            .filter(|m| m.loaded)
            .map(|m| m.name.clone())
            .collect(),
    };
    let next = args.offset.saturating_add(matches.len());
    json!({
        "loaded_tools": selection.loaded_tools,
        "matches": matches,
        "total_matches": total,
        "next_offset": (next < total).then_some(next),
        "schema_budget_bytes": budget,
        "instruction": "Only loaded_tools will have full schemas in the next model request. Call those tools by their original names with their declared arguments. Each successful search replaces the previous selection."
    })
}

/// Only the latest successful result paired with a real search call selects
/// schemas. User text and results of another tool cannot manufacture selection.
#[cfg(test)]
fn selected_names(messages: &[Message]) -> BTreeSet<String> {
    selected_in(messages, Catalog::Mcp)
}
fn selected_in(messages: &[Message], catalog: Catalog) -> BTreeSet<String> {
    let mut calls = HashSet::new();
    let mut selected = BTreeSet::new();
    for message in messages {
        if message.role == Role::Assistant {
            calls.extend(
                message
                    .tool_calls
                    .iter()
                    .filter(|call| call.name == catalog.search_name())
                    .map(|call| call.id.as_str()),
            );
        } else if message.role == Role::Tool
            && !message.is_error
            && message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| calls.remove(id))
        {
            if let Ok(selection) = serde_json::from_str::<Selection>(&message.text) {
                selected = selection
                    .loaded_tools
                    .into_iter()
                    .filter(|name| catalog.contains(name))
                    .take(MAX_MATCHES)
                    .collect();
            }
        }
    }
    selected
}

#[cfg(test)]
fn project(request: &mut ModelRequest, mode: McpToolSearchMode, window: u32) {
    project_in(request, mode, window, Catalog::Mcp)
}
fn log_projection_result(
    request: &ModelRequest,
    catalog: Catalog,
    admitted_tools: usize,
    admitted_schema_bytes: usize,
    retained_tools: usize,
    retained_schema_bytes: usize,
) {
    tracing::info!(
        turn = request.turn,
        round = request.round,
        catalog = catalog.search_name(),
        admitted_tools,
        admitted_schema_bytes,
        retained_tools,
        retained_schema_bytes,
        hidden_tools = admitted_tools.saturating_sub(retained_tools),
        hidden_schema_bytes = admitted_schema_bytes.saturating_sub(retained_schema_bytes),
        "Tool schema projection result (ToolDef JSON bytes, not provider wire bytes or tokens)"
    );
}

fn project_in(request: &mut ModelRequest, mode: McpToolSearchMode, window: u32, catalog: Catalog) {
    // A policy or switch can disable search itself. Preserve usable eager tools
    // in that case rather than hide every schema behind an unavailable door.
    let can_search = request
        .tools
        .iter()
        .any(|tool| tool.name == catalog.search_name());
    let budget = schema_budget(window);
    let admitted_schema_bytes = request
        .tools
        .iter()
        .filter(|tool| catalog.contains(&tool.name))
        .fold(0usize, |sum, tool| sum.saturating_add(schema_bytes(tool)));
    let admitted_tools = request
        .tools
        .iter()
        .filter(|tool| catalog.contains(&tool.name))
        .count();
    let deferred = can_search
        && match mode {
            McpToolSearchMode::Off => false,
            McpToolSearchMode::On => true,
            McpToolSearchMode::Auto => admitted_schema_bytes > budget,
        };
    tracing::info!(turn = request.turn, round = request.round, mode = ?mode,
        context_window = window, schema_budget_bytes = budget, admitted_schema_bytes,
        admitted_tools, search_available = can_search, deferred,
        catalog = catalog.search_name(), "{}", match catalog { Catalog::Mcp => "MCP schema projection decision (JSON bytes, not tokens)", Catalog::Builtin => "Builtin schema projection decision (JSON bytes, not tokens)" });
    if !deferred {
        // Keep the small discovery tool in auto mode, even for eager catalogs.
        // This also makes retrying an already-projected request idempotent.
        log_projection_result(
            request,
            catalog,
            admitted_tools,
            admitted_schema_bytes,
            admitted_tools,
            admitted_schema_bytes,
        );
        return;
    }
    if let Catalog::Builtin = catalog {
        // Derived from this request's admitted catalog; never advertise denied tools.
        let names = request
            .tools
            .iter()
            .filter(|t| catalog.contains(&t.name))
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(discovery) = request
            .tools
            .iter_mut()
            .find(|t| t.name == catalog.search_name())
        {
            if !discovery.description.contains(" Available: ") {
                discovery.description = format!(
                    "{} Available: {}.",
                    SearchTool(catalog).description(),
                    names
                );
            }
        }
    }
    let selected = selected_in(&request.messages, catalog);
    let mut remaining = budget;
    let mut retained_tools = 0;
    request.tools.retain(|tool| {
        if !catalog.contains(&tool.name) {
            return true;
        }
        let bytes = schema_bytes(tool);
        if selected.contains(&tool.name) && bytes <= remaining {
            remaining -= bytes;
            retained_tools += 1;
            true
        } else {
            false
        }
    });
    log_projection_result(
        request,
        catalog,
        admitted_tools,
        admitted_schema_bytes,
        retained_tools,
        budget - remaining,
    );
}

struct SearchTool(Catalog);

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        self.0.search_name()
    }
    fn description(&self) -> &str {
        if let Catalog::Builtin = self.0 {
            return "Find and load AtomGit tools for repositories, issues, pull requests and REST API operations. Search by keywords or exact tool name; empty query lists available tools. Full schemas load for the NEXT request, replacing the previous selection. Search again if schemas are absent after compaction. Does not execute tools.";
        }
        "Find and load MCP tools by keywords in server/tool names or descriptions. Search before using an external capability whose schema is absent. Empty query browses the catalog; offset pages results. Returns eight matches by default; limit can request up to 32. Full schemas load within the schema byte budget for the NEXT request, replacing the previous selection. Does not execute tools."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string","description":"Keywords or an exact tool name; empty string browses."},
            "limit":{"type":"integer","minimum":1,"maximum":32,"default":8},
            "offset":{"type":"integer","minimum":0}
        },"required":["query"],"additionalProperties":false})
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    fn self_bounds_output(&self) -> bool {
        true
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args = match serde_json::from_str::<SearchArgs>(args) {
            Ok(args) if args.query.chars().count() <= 512 => args,
            _ => return ToolResult {
                is_error: true,
                content:
                    "Expected query (at most 512 characters), optional limit (1..32), and offset."
                        .into(),
                ..Default::default()
            },
        };
        let Some(ctx) = atomcode_harness::agent::current() else {
            return ToolResult {
                is_error: true,
                content: "Tool search requires a running agent context.".into(),
                ..Default::default()
            };
        };
        let Some(tools) = ctx.service::<ToolsSvc>() else {
            return ToolResult {
                is_error: true,
                content: "No tool catalog is mounted for this agent.".into(),
                ..Default::default()
            };
        };
        let window = ctx
            .service::<LlmSvc>()
            .map(|model| model.context_window())
            .unwrap_or(0);
        ToolResult {
            content: search_in(&tools.defs(), args, schema_budget(window), self.0).to_string(),
            ..Default::default()
        }
    }
}

struct ProjectSchemas {
    ctx: Context,
    mode: McpToolSearchMode,
    catalog: Catalog,
}

#[async_trait]
impl Waterfall<AgentRequest> for ProjectSchemas {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let ctx = atomcode_harness::agent::scoped(&self.ctx);
        let window = ctx
            .service::<LlmSvc>()
            .map(|model| model.context_window())
            .unwrap_or(0);
        project_in(req, self.mode, window, self.catalog);
        next.run(req).await
    }
}

pub(crate) fn mount(ctx: &Context, mode: McpToolSearchMode) -> Result<(), String> {
    if mode == McpToolSearchMode::Off {
        return Ok(());
    }
    atomcode_harness::plugins::tools::mount(ctx, vec![Arc::new(SearchTool(Catalog::Mcp))])?;
    let _ = ctx.on_waterfall::<AgentRequest>(
        Arc::new(ProjectSchemas {
            ctx: ctx.clone(),
            mode,
            catalog: Catalog::Mcp,
        }),
        true,
    );
    Ok(())
}

/// Request projection has its own row, independent of MCP availability.
pub(crate) struct BuiltinSearchPlugin;
#[async_trait]
impl atomcode_plexus::Plugin for BuiltinSearchPlugin {
    fn name(&self) -> &'static str {
        "builtin-tool-search"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        #[derive(Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Config {
            enabled: bool,
        }
        impl Default for Config {
            fn default() -> Self {
                Self { enabled: true }
            }
        }
        let config: Config = if config.is_null() {
            Config::default()
        } else {
            serde_json::from_value(config.clone())
                .map_err(|e| format!("bad builtin discovery config: {e}"))?
        };
        if !config.enabled {
            return Ok(());
        }
        // Host tools precede this row. Do not add an empty discovery door to
        // embeddings, feature-disabled builds or agents with no admitted AtomGit tools.
        let tools = ctx
            .service::<ToolsSvc>()
            .ok_or("builtin discovery requires tools")?;
        if !tools
            .defs()
            .iter()
            .any(|tool| Catalog::Builtin.contains(&tool.name))
        {
            return Ok(());
        }
        atomcode_harness::plugins::tools::mount(ctx, vec![Arc::new(SearchTool(Catalog::Builtin))])?;
        let _ = ctx.on_waterfall::<AgentRequest>(
            Arc::new(ProjectSchemas {
                ctx: ctx.clone(),
                mode: McpToolSearchMode::On,
                catalog: Catalog::Builtin,
            }),
            true,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::tool::ToolCall;

    fn tool(name: &str, padding: usize) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: format!("{name} decompile inspect instructions"),
            parameters: json!({"type":"object","properties":{"address":{"type":"string","description":"x".repeat(padding)}}}),
        }
    }
    fn request(tools: Vec<ToolDef>, messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            tools,
            messages,
            options: ChatOptions::default(),
            turn: 1,
            round: 1,
            answering_a_nudge: false,
        }
    }
    fn discovery() -> ToolDef {
        ToolDef {
            name: SEARCH.into(),
            description: SearchTool(Catalog::Mcp).description().into(),
            parameters: SearchTool(Catalog::Mcp).parameters_schema(),
        }
    }
    fn pair(id: &str, name: &str, result: Value) -> Vec<Message> {
        vec![
            Message::assistant(
                "",
                vec![ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result(id, result.to_string(), false),
        ]
    }

    #[test]
    fn default_mode_defers_moderate_catalogs_on_a_million_token_model() {
        let mut tools = (0..34)
            .map(|i| tool(&format!("mcp__pilot__tool_{i:02}"), 1500))
            .collect::<Vec<_>>();
        tools.push(discovery());
        tools.push(tool("read_file", 0));
        let original = request(tools.clone(), vec![Message::user("inspect")]);
        let mut auto = original.clone();
        project(&mut auto, McpToolSearchMode::Auto, 1_000_000);
        assert_eq!(
            auto.tools.len(),
            tools.len(),
            "auto retains its existing threshold"
        );
        let mut off = original.clone();
        project(&mut off, McpToolSearchMode::Off, 1_000_000);
        assert_eq!(off.tools.len(), tools.len());
        let mut first = original.clone();
        project(&mut first, McpToolSearchMode::default(), 1_000_000);
        assert_eq!(
            first
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec![SEARCH, "read_file"]
        );
        let eager_bytes = serde_json::to_vec(&original.tools).unwrap().len();
        let lazy_bytes = serde_json::to_vec(&first.tools).unwrap().len();
        assert!(
            lazy_bytes * 10 < eager_bytes,
            "schema reduction: {eager_bytes} -> {lazy_bytes}"
        );
        let found = search(
            &tools,
            SearchArgs {
                query: "mcp__pilot__tool_17".into(),
                limit: Some(1),
                ..Default::default()
            },
            schema_budget(1_000_000),
        );
        let mut next = request(tools.clone(), pair("pilot-search", SEARCH, found));
        project(&mut next, McpToolSearchMode::On, 1_000_000);
        let loaded = next
            .tools
            .iter()
            .find(|tool| tool.name == "mcp__pilot__tool_17")
            .unwrap();
        assert_eq!(
            serde_json::to_value(loaded).unwrap(),
            serde_json::to_value(&tools[17]).unwrap()
        );
        assert!(next.tools.iter().any(|tool| tool.name == "read_file"));
        assert_eq!(
            next.tools.iter().filter(|tool| is_mcp(&tool.name)).count(),
            1
        );
    }

    #[test]
    fn a_large_catalog_is_lazy_and_only_search_matches_load_next_request() {
        let mut tools = (0..34)
            .map(|i| tool(&format!("mcp__rea__tool_{i:02}"), 12_000))
            .collect::<Vec<_>>();
        tools.push(discovery());
        tools.push(tool("read_file", 0));
        let mut first = request(tools.clone(), vec![Message::user("inspect a binary")]);
        project(&mut first, McpToolSearchMode::Auto, 128_000);
        assert_eq!(
            first
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec![SEARCH, "read_file"]
        );
        assert!(serde_json::to_vec(&first.tools).unwrap().len() < 2_048);
        let found = search(
            &tools,
            SearchArgs {
                query: "mcp__rea__tool_17".into(),
                limit: Some(1),
                ..Default::default()
            },
            schema_budget(128_000),
        );
        assert_eq!(found["loaded_tools"], json!(["mcp__rea__tool_17"]));
        assert!(
            found.to_string().len() < 2_048,
            "search must never write the complete schema into history"
        );
        let mut next = request(tools, pair("search1", SEARCH, found));
        project(&mut next, McpToolSearchMode::Auto, 128_000);
        assert!(next
            .tools
            .iter()
            .any(|t| t.name == "mcp__rea__tool_17" && t.parameters.to_string().len() > 12_000));
        assert_eq!(next.tools.iter().filter(|t| is_mcp(&t.name)).count(), 1);
        let before_retry = next.tools.clone();
        project(&mut next, McpToolSearchMode::Auto, 128_000);
        assert_eq!(next.tools, before_retry);
    }

    #[test]
    fn small_catalogs_and_explicit_off_keep_all_schemas() {
        for mode in [McpToolSearchMode::Auto, McpToolSearchMode::Off] {
            let original = tool("mcp__docs__read", 0);
            let mut req = request(vec![original.clone(), discovery()], vec![]);
            project(&mut req, mode, 128_000);
            assert!(req.tools.contains(&original));
        }
        let mut req = request(vec![tool("mcp__docs__read", 100_000)], vec![]);
        let original = req.tools.clone();
        project(&mut req, McpToolSearchMode::On, 128_000);
        assert_eq!(
            req.tools, original,
            "excluded search cannot strand MCP tools"
        );
    }

    #[test]
    fn search_is_paged_bounded_and_never_truncates_an_oversized_schema() {
        let tools = (0..34)
            .map(|i| tool(&format!("mcp__rea__{i:02}"), 200))
            .collect::<Vec<_>>();
        let result = search(
            &tools,
            SearchArgs {
                limit: Some(5),
                offset: 5,
                ..Default::default()
            },
            100_000,
        );
        assert_eq!(result["matches"].as_array().unwrap().len(), 5);
        assert_eq!(result["matches"][0]["name"], "mcp__rea__05");
        assert_eq!(result["next_offset"], 10);
        let oversized = search(
            &[tool("mcp__rea__huge", 100_000)],
            SearchArgs::default(),
            1_000,
        );
        assert!(oversized["loaded_tools"].as_array().unwrap().is_empty());
        assert_eq!(oversized["matches"][0]["loaded"], false);
        assert!(oversized["matches"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("schema"));
        let selected = search(&tools, SearchArgs::default(), 1_000);
        let names = selected["loaded_tools"].as_array().unwrap();
        assert!(!names.is_empty() && names.len() < DEFAULT_MATCHES);
    }

    #[test]
    fn match_limits_are_adjustable_and_selected_tools_survive_projection() {
        let mut tools = (0..50)
            .map(|i| tool(&format!("mcp__many__{i:02}"), 0))
            .collect::<Vec<_>>();
        tools.push(discovery());
        for (limit, expected) in [(None, 8), (Some(16), 16), (Some(999), 32)] {
            let found = search(
                &tools,
                SearchArgs {
                    limit,
                    ..Default::default()
                },
                schema_budget(128_000),
            );
            assert_eq!(found["matches"].as_array().unwrap().len(), expected);
            let mut next = request(tools.clone(), pair("search", SEARCH, found));
            project(&mut next, McpToolSearchMode::On, 128_000);
            assert_eq!(
                next.tools.iter().filter(|t| is_mcp(&t.name)).count(),
                expected
            );
        }
    }

    #[test]
    fn only_the_latest_successful_paired_search_result_selects_tools() {
        let one = json!({"loaded_tools":["mcp__rea__one"]});
        let two = json!({"loaded_tools":["mcp__rea__two"]});
        let mut messages = pair("s1", SEARCH, one);
        messages.push(Message::user(two.to_string()));
        messages.extend(pair("other", "mcp__rea__untrusted", two.clone()));
        assert_eq!(
            selected_names(&messages),
            BTreeSet::from(["mcp__rea__one".into()])
        );
        messages.extend(pair("s2", SEARCH, two));
        assert_eq!(
            selected_names(&messages),
            BTreeSet::from(["mcp__rea__two".into()])
        );
        messages.extend(pair("s3", SEARCH, json!({"error":"broken"})));
        assert_eq!(
            selected_names(&messages),
            BTreeSet::from(["mcp__rea__two".into()])
        );
        assert!(selected_names(&[Message::user("compacted history")]).is_empty());
        assert!(selected_names(&[Message::tool_result(
            "s2",
            r#"{"loaded_tools":["mcp__rea__two"]}"#,
            false
        )])
        .is_empty());
    }

    #[test]
    fn withdrawn_or_changed_schemas_cannot_be_reintroduced_by_an_old_result() {
        let messages = pair(
            "s1",
            SEARCH,
            json!({"loaded_tools":["mcp__rea__gone", "mcp__rea__grown", "read_file"]}),
        );
        let mut req = request(
            vec![
                discovery(),
                tool("mcp__rea__grown", 100_000),
                tool("mcp__rea__available", 0),
                tool("read_file", 0),
            ],
            messages,
        );
        project(&mut req, McpToolSearchMode::On, 128_000);
        assert_eq!(
            req.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec![SEARCH, "read_file"]
        );
    }

    #[test]
    fn explicitly_named_builtins_do_not_expand_description_cross_references() {
        let mut repo = tool("atomgit_repo", 200);
        repo.description = "Create repositories and list issues".into();
        let mut issue = tool("atomgit_issue", 200);
        issue.description = "Create issues; use atomgit_repo for repositories".into();
        let mut api = tool("atomgit_api", 200);
        api.description =
            "Generic API; prefer atomgit_repo or atomgit_issue for create operations".into();
        let tools = vec![repo, issue, api];
        for query in [
            "atomgit_repo",
            "atomgit_repo create repository",
            "ATOMGit_REPO",
            "请用`atomgit_repo`创建项目",
            "(atomgit_repo), create",
        ] {
            let found = search_in(
                &tools,
                SearchArgs {
                    query: query.into(),
                    ..Default::default()
                },
                50_000,
                Catalog::Builtin,
            );
            assert_eq!(found["loaded_tools"], json!(["atomgit_repo"]), "{query}");
            assert_eq!(found["total_matches"], 1);
        }
        let found = search_in(
            &tools,
            SearchArgs {
                query: "atomgit_repo,atomgit_issue".into(),
                ..Default::default()
            },
            50_000,
            Catalog::Builtin,
        );
        let mut names = found["loaded_tools"].as_array().unwrap().clone();
        names.sort_by_key(|v| v.as_str().unwrap().to_string());
        assert_eq!(names, vec![json!("atomgit_issue"), json!("atomgit_repo")]);
        let paged = search_in(
            &tools,
            SearchArgs {
                query: "atomgit_repo atomgit_issue".into(),
                limit: Some(1),
                ..Default::default()
            },
            50_000,
            Catalog::Builtin,
        );
        assert_eq!(paged["matches"].as_array().unwrap().len(), 1);
        assert_eq!(paged["next_offset"], 1);
        for query in ["", "create"] {
            let found = search_in(
                &tools,
                SearchArgs {
                    query: query.into(),
                    ..Default::default()
                },
                50_000,
                Catalog::Builtin,
            );
            assert_eq!(
                found["total_matches"], 3,
                "keyword search and browsing remain broad"
            );
        }
        let denied = search_in(
            &tools[1..],
            SearchArgs {
                query: "atomgit_repo".into(),
                ..Default::default()
            },
            50_000,
            Catalog::Builtin,
        );
        assert_eq!(
            denied["loaded_tools"],
            json!([]),
            "missing named tools must not be substituted"
        );
        let oversized = search_in(
            &tools,
            SearchArgs {
                query: "atomgit_repo".into(),
                ..Default::default()
            },
            1,
            Catalog::Builtin,
        );
        assert_eq!(oversized["total_matches"], 1);
        assert_eq!(oversized["loaded_tools"], json!([]));
        // MCP keeps its established fuzzy name/description matching.
        let mut mcp_api = tool("mcp__service__api", 0);
        mcp_api.description = "Use mcp__service__repo to create repositories".into();
        let mcp = search(
            &[tool("mcp__service__repo", 0), mcp_api],
            SearchArgs {
                query: "mcp__service__repo".into(),
                ..Default::default()
            },
            50_000,
        );
        assert_eq!(mcp["total_matches"], 2);
    }

    #[test]
    fn builtin_catalog_is_small_exact_and_independent_of_mcp_selection() {
        let builtin = Catalog::Builtin;
        let discovery = ToolDef {
            name: builtin.search_name().into(),
            description: SearchTool(builtin).description().into(),
            parameters: SearchTool(builtin).parameters_schema(),
        };
        let mut tools = vec![
            discovery,
            super::tests::discovery(),
            tool("bash", 500),
            tool("atomgit_issue", 4500),
            tool("atomgit_pr", 4500),
            tool("atomgit_api", 1500),
            tool("atomgit_repo", 1500),
            tool("atomgit_other", 0),
            tool("mcp__docs__read", 500),
        ];
        let original = tools.clone();
        let mut first = request(tools.clone(), vec![]);
        project_in(&mut first, McpToolSearchMode::On, 128_000, builtin);
        assert_eq!(
            first
                .tools
                .iter()
                .filter(|t| builtin.contains(&t.name))
                .count(),
            0
        );
        let directory = &first
            .tools
            .iter()
            .find(|t| t.name == builtin.search_name())
            .unwrap()
            .description;
        assert!(directory.contains("atomgit_issue") && directory.contains("atomgit_repo"));
        assert!(!directory.contains("atomgit_other"));
        assert!(
            serde_json::to_vec(&first.tools).unwrap().len() + 9000
                < serde_json::to_vec(&original).unwrap().len()
        );
        let retry = first.tools.clone();
        project_in(&mut first, McpToolSearchMode::On, 128_000, builtin);
        assert_eq!(first.tools, retry);
        let found = search_in(
            &tools,
            SearchArgs {
                query: "atomgit_issue".into(),
                limit: Some(1),
                ..Default::default()
            },
            schema_budget(128_000),
            builtin,
        );
        assert_eq!(found["loaded_tools"], json!(["atomgit_issue"]));
        assert!(found.to_string().len() < 1500);
        let mut messages = pair("builtin", builtin.search_name(), found);
        messages.extend(pair(
            "mcp",
            SEARCH,
            json!({"loaded_tools":["mcp__docs__read", "atomgit_pr"]}),
        ));
        let mut next = request(tools.clone(), messages);
        project_in(&mut next, McpToolSearchMode::On, 128_000, builtin);
        project(&mut next, McpToolSearchMode::On, 128_000);
        assert_eq!(
            next.tools
                .iter()
                .filter(|t| builtin.contains(&t.name))
                .cloned()
                .collect::<Vec<_>>(),
            vec![original[3].clone()]
        );
        assert!(next.tools.contains(&original[8]));
        assert!(next.tools.contains(&original[2]));
        assert!(next.tools.contains(&original[7]));
        // Compaction discards selection, not the discovery route.
        next = request(tools.clone(), vec![Message::user("summary")]);
        project_in(&mut next, McpToolSearchMode::On, 128_000, builtin);
        assert_eq!(
            next.tools
                .iter()
                .filter(|t| builtin.contains(&t.name))
                .count(),
            0
        );
        assert!(next.tools.iter().any(|t| t.name == builtin.search_name()));
        // Removed tools cannot be resurrected by a prior result.
        tools.retain(|t| t.name != "atomgit_issue");
        let mut withdrawn = request(
            tools.clone(),
            pair(
                "old",
                builtin.search_name(),
                json!({"loaded_tools":["atomgit_issue"]}),
            ),
        );
        project_in(&mut withdrawn, McpToolSearchMode::On, 128_000, builtin);
        assert!(!withdrawn.tools.iter().any(|t| t.name == "atomgit_issue"));
        assert!(!withdrawn
            .tools
            .iter()
            .find(|t| t.name == builtin.search_name())
            .unwrap()
            .description
            .contains("atomgit_issue"));
        // Policy disabling discovery falls back to usable eager definitions.
        tools.retain(|t| t.name != builtin.search_name());
        let mut no_search = request(tools.clone(), vec![]);
        project_in(&mut no_search, McpToolSearchMode::On, 128_000, builtin);
        assert_eq!(no_search.tools, tools);
        let mut off = request(original.clone(), vec![]);
        project_in(&mut off, McpToolSearchMode::Off, 128_000, builtin);
        assert_eq!(off.tools, original);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn builtin_search_uses_current_child_policy_and_rediscovers_after_compaction() {
        use atomcode_harness::seams::{ToolBox, ToolPolicy};
        use atomcode_plexus::{App, ConfigTree, Layer, Plugin, PluginRegistry};
        let app = App::new(
            PluginRegistry::new(),
            ConfigTree::from_layers(vec![Layer::new()]).unwrap(),
        );
        let root = app.context();
        let parent = Arc::new(ToolBox::new());
        parent
            .register(Arc::new(Stub(tool("atomgit_api", 0))))
            .unwrap();
        let _root = root.provide::<ToolsSvc>(parent).unwrap();
        let child = root.isolate();
        let child_tools = Arc::new(ToolBox::with_policy(ToolPolicy::new(
            vec![],
            vec!["atomgit_pr".into()],
        )));
        for name in ["atomgit_issue", "atomgit_pr", "atomgit_repo"] {
            child_tools.register(Arc::new(Stub(tool(name, 0)))).unwrap();
        }
        child_tools.turn_off("atomgit_repo");
        let _child = child.provide::<ToolsSvc>(child_tools.clone()).unwrap();
        BuiltinSearchPlugin
            .apply(&child, &json!({"enabled": true}))
            .await
            .unwrap();
        let ctx = ToolContext {
            working_dir: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        for _ in 0..2 {
            let result = atomcode_harness::agent::as_agent(
                child.clone(),
                SearchTool(Catalog::Builtin).execute(r#"{"query":""}"#, &ctx),
            )
            .await;
            assert!(!result.is_error, "{}", result.content);
            assert_eq!(
                serde_json::from_str::<Value>(&result.content).unwrap()["loaded_tools"],
                json!(["atomgit_issue"])
            );
        }
        child_tools.unregister("atomgit_issue");
        let result = atomcode_harness::agent::as_agent(
            child.clone(),
            SearchTool(Catalog::Builtin).execute(r#"{"query":""}"#, &ctx),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap()["loaded_tools"],
            json!([])
        );
        let off = root.isolate();
        let off_tools = Arc::new(ToolBox::new());
        let _off = off.provide::<ToolsSvc>(off_tools.clone()).unwrap();
        BuiltinSearchPlugin
            .apply(&off, &json!({"enabled": false}))
            .await
            .unwrap();
        assert!(!off_tools.defs().iter().any(|t| t.name == "builtin_search"));
    }

    struct Stub(ToolDef);
    #[async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            &self.0.name
        }
        fn description(&self) -> &str {
            &self.0.description
        }
        fn parameters_schema(&self) -> Value {
            self.0.parameters.clone()
        }
        async fn execute(&self, _: &str, _: &ToolContext) -> ToolResult {
            unreachable!("search must not execute MCP tools")
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn searches_obey_child_catalog_policy_and_session_switches() {
        use atomcode_harness::seams::{ToolBox, ToolPolicy};
        use atomcode_plexus::{App, ConfigTree, Layer, PluginRegistry};
        let app = App::new(
            PluginRegistry::new(),
            ConfigTree::from_layers(vec![Layer::new()]).unwrap(),
        );
        let root = app.context();
        let parent = Arc::new(ToolBox::new());
        parent
            .register(Arc::new(Stub(tool("mcp__parent__private", 0))))
            .unwrap();
        let _parent_slot = root.provide::<ToolsSvc>(parent).unwrap();
        let child = root.isolate();
        let child_tools = Arc::new(ToolBox::with_policy(ToolPolicy::new(
            vec![],
            vec!["mcp__child__denied".into()],
        )));
        for name in [
            "mcp__child__read",
            "mcp__child__hidden",
            "mcp__child__denied",
        ] {
            child_tools.register(Arc::new(Stub(tool(name, 0)))).unwrap();
        }
        child_tools.turn_off("mcp__child__hidden");
        let _child_slot = child.provide::<ToolsSvc>(child_tools.clone()).unwrap();
        let tool_ctx = ToolContext {
            working_dir: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let result = atomcode_harness::agent::as_agent(
            child.clone(),
            SearchTool(Catalog::Mcp).execute(r#"{"query":""}"#, &tool_ctx),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let result: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(result["loaded_tools"], json!(["mcp__child__read"]));
        assert_eq!(result["total_matches"], 1);
        child_tools.unregister("mcp__child__read");
        let withdrawn = atomcode_harness::agent::as_agent(
            child,
            SearchTool(Catalog::Mcp).execute(r#"{"query":""}"#, &tool_ctx),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&withdrawn.content).unwrap()["loaded_tools"],
            json!([])
        );
    }
}
