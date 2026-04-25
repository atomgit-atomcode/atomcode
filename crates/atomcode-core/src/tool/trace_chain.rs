use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct TraceChainTool;

#[derive(Deserialize)]
struct TraceChainArgs {
    from: String,
    to: String,
}

fn shorten_path(path: &std::path::Path) -> String {
    let components: Vec<_> = path.components().collect();
    if components.len() <= 3 {
        return path.display().to_string();
    }
    let last3: Vec<_> = components[components.len() - 3..]
        .iter()
        .map(|c| c.as_os_str())
        .collect();
    format!(
        ".../{}",
        last3
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    )
}

#[async_trait]
impl Tool for TraceChainTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "trace_chain",
            description: "Find the shortest call chain between two symbols. Uses BFS to discover \
                the path from `from` to `to` through function calls (max 10 hops).\n\
                Example: {\"from\": \"handle_request\", \"to\": \"save_to_db\"}"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Source symbol name" },
                    "to": { "type": "string", "description": "Target symbol name" }
                },
                "required": ["from", "to"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: TraceChainArgs = serde_json::from_str(args)?;

        let graph = ctx.graph.read().await;

        if !graph.is_ready() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: "Code graph is not yet indexed. The graph will be available after the \
                    background indexer completes. Try again shortly."
                    .to_string(),
                success: false,
            });
        }

        let from_matches = graph.find_by_name(&parsed.from);
        let to_matches = graph.find_by_name(&parsed.to);

        if from_matches.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Source symbol '{}' not found in code graph.", parsed.from),
                success: false,
            });
        }
        if to_matches.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Target symbol '{}' not found in code graph.", parsed.to),
                success: false,
            });
        }

        // Try all combinations of from/to matches to find any path
        let mut out = String::new();
        let mut found_any = false;

        for from_sym in &from_matches {
            for to_sym in &to_matches {
                if let Some(path) = graph.shortest_path(from_sym.id, to_sym.id) {
                    found_any = true;
                    out.push_str(&format!(
                        "Call chain from '{}' to '{}' ({} hops):\n",
                        parsed.from,
                        parsed.to,
                        path.len() - 1
                    ));
                    for (i, &sym_id) in path.iter().enumerate() {
                        if let Some(node) = graph.node(sym_id) {
                            let arrow = if i == 0 { ">" } else { "→" };
                            out.push_str(&format!(
                                "  {} {} ({:?}) — {}\n",
                                arrow,
                                node.name,
                                node.kind,
                                shorten_path(&node.file)
                            ));
                        }
                    }
                    out.push('\n');
                }
            }
        }

        if !found_any {
            out.push_str(&format!(
                "No call chain found from '{}' to '{}' (max 10 hops).\n",
                parsed.from, parsed.to
            ));
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: out,
            success: found_any,
        })
    }
}
