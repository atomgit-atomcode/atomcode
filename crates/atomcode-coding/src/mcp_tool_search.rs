//! Request-only MCP schema selection. The admitted tool catalog and native
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
const MAX_MATCHES: usize = 5;
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

fn search(tools: &[ToolDef], args: SearchArgs, budget: usize) -> Value {
    let terms = args
        .query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let mut ranked = tools
        .iter()
        .filter(|tool| is_mcp(&tool.name))
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
            (terms.is_empty() || score > 0).then_some((score, tool))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(a_score, a), (b_score, b)| {
        b_score.cmp(a_score).then_with(|| a.name.cmp(&b.name))
    });
    let total = ranked.len();
    let limit = args.limit.unwrap_or(MAX_MATCHES).clamp(1, MAX_MATCHES);
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
            reason: (!loaded).then_some("schema exceeds the remaining load budget; search more narrowly or use limit=1; tools.mcp.tool_search=off restores eager loading"),
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
fn selected_names(messages: &[Message]) -> BTreeSet<String> {
    let mut calls = HashSet::new();
    let mut selected = BTreeSet::new();
    for message in messages {
        if message.role == Role::Assistant {
            calls.extend(
                message
                    .tool_calls
                    .iter()
                    .filter(|call| call.name == SEARCH)
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
                    .filter(|name| is_mcp(name))
                    .take(MAX_MATCHES)
                    .collect();
            }
        }
    }
    selected
}

fn project(request: &mut ModelRequest, mode: McpToolSearchMode, window: u32) {
    // A policy or switch can disable search itself. Preserve usable eager tools
    // in that case rather than hide every schema behind an unavailable door.
    let can_search = request.tools.iter().any(|tool| tool.name == SEARCH);
    let budget = schema_budget(window);
    let deferred = can_search
        && match mode {
            McpToolSearchMode::Off => false,
            McpToolSearchMode::On => true,
            McpToolSearchMode::Auto => {
                request
                    .tools
                    .iter()
                    .filter(|tool| is_mcp(&tool.name))
                    .fold(0usize, |sum, tool| sum.saturating_add(schema_bytes(tool)))
                    > budget
            }
        };
    if !deferred {
        // Keep the small discovery tool in auto mode, even for eager catalogs.
        // This also makes retrying an already-projected request idempotent.
        return;
    }
    let selected = selected_names(&request.messages);
    let mut remaining = budget;
    request.tools.retain(|tool| {
        if !is_mcp(&tool.name) {
            return true;
        }
        let bytes = schema_bytes(tool);
        if selected.contains(&tool.name) && bytes <= remaining {
            remaining -= bytes;
            true
        } else {
            false
        }
    });
}

struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        SEARCH
    }
    fn description(&self) -> &str {
        "Find and load MCP tools by keywords in server/tool names or descriptions. Search before using an external capability whose schema is absent. Empty query browses the catalog; offset pages results. Loads at most five tools for the NEXT request, replacing the previous selection. Does not execute tools."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string","description":"Keywords or an exact mcp__server__tool name; empty string browses."},
            "limit":{"type":"integer","minimum":1,"maximum":5},
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
                    "Expected query (at most 512 characters), optional limit (1..5), and offset."
                        .into(),
                ..Default::default()
            },
        };
        let Some(ctx) = atomcode_harness::agent::current() else {
            return ToolResult {
                is_error: true,
                content: "MCP search requires a running agent context.".into(),
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
            content: search(&tools.defs(), args, schema_budget(window)).to_string(),
            ..Default::default()
        }
    }
}

struct ProjectSchemas {
    ctx: Context,
    mode: McpToolSearchMode,
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
        project(req, self.mode, window);
        next.run(req).await
    }
}

pub(crate) fn mount(ctx: &Context, mode: McpToolSearchMode) -> Result<(), String> {
    if mode == McpToolSearchMode::Off {
        return Ok(());
    }
    atomcode_harness::plugins::tools::mount(ctx, vec![Arc::new(SearchTool)])?;
    let _ = ctx.on_waterfall::<AgentRequest>(
        Arc::new(ProjectSchemas {
            ctx: ctx.clone(),
            mode,
        }),
        true,
    );
    Ok(())
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
            description: SearchTool.description().into(),
            parameters: SearchTool.parameters_schema(),
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
                limit: Some(999),
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
        assert!(!names.is_empty() && names.len() < 5);
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
            SearchTool.execute(r#"{"query":""}"#, &tool_ctx),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        let result: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(result["loaded_tools"], json!(["mcp__child__read"]));
        assert_eq!(result["total_matches"], 1);
        child_tools.unregister("mcp__child__read");
        let withdrawn = atomcode_harness::agent::as_agent(
            child,
            SearchTool.execute(r#"{"query":""}"#, &tool_ctx),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&withdrawn.content).unwrap()["loaded_tools"],
            json!([])
        );
    }
}
