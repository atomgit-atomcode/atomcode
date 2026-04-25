use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct TraceCallersTool;

#[derive(Deserialize)]
struct TraceCallersArgs {
    symbol: String,
    depth: Option<usize>,
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
impl Tool for TraceCallersTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "trace_callers",
            description: "Trace all callers of a symbol (reverse call graph). Uses BFS to find \
                functions/methods that directly or transitively call the given symbol.\n\
                Returns a tree showing caller chains up to the specified depth.\n\
                Example: {\"symbol\": \"process_data\", \"depth\": 3}"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name to trace callers for" },
                    "depth": { "type": "integer", "description": "Max traversal depth (default: 3, max: 5)" }
                },
                "required": ["symbol"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: TraceCallersArgs = serde_json::from_str(args)?;
        let depth = parsed.depth.unwrap_or(3).min(5);

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

        let matches = graph.find_by_name(&parsed.symbol);
        if matches.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Symbol '{}' not found in code graph ({} symbols indexed).",
                    parsed.symbol,
                    graph.node_count()
                ),
                success: false,
            });
        }

        let mut out = String::new();
        for sym in &matches {
            out.push_str(&format!(
                "Callers of {} ({:?}) in {}:\n",
                sym.name,
                sym.kind,
                shorten_path(&sym.file)
            ));

            let callers = graph.trace_callers(sym.id, depth);
            if callers.is_empty() {
                out.push_str("  (no callers found)\n");
            } else {
                for (caller_id, d) in &callers {
                    if let Some(node) = graph.node(*caller_id) {
                        let indent = "  ".repeat(*d);
                        out.push_str(&format!(
                            "{}[depth {}] {} ({:?}) — {}\n",
                            indent,
                            d,
                            node.name,
                            node.kind,
                            shorten_path(&node.file)
                        ));
                    }
                }
            }
            out.push('\n');
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: out,
            success: true,
        })
    }
}
