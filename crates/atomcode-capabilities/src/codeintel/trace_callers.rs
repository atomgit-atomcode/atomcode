//! `trace_callers` — reverse call graph (who calls a symbol), BFS to a depth. `Safe`.

use super::index::CodeIndex;
use super::{canonical, display_path, err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

pub struct TraceCallersTool {
    index: Arc<CodeIndex>,
}

impl TraceCallersTool {
    pub fn new(index: Arc<CodeIndex>) -> Self {
        Self { index }
    }
}

#[derive(Deserialize)]
struct Args {
    symbol: String,
    #[serde(default)]
    depth: Option<usize>,
}

#[async_trait]
impl Tool for TraceCallersTool {
    fn name(&self) -> &str {
        "trace_callers"
    }
    fn description(&self) -> &str {
        "Trace all callers of a symbol (reverse call graph), BFS up to a depth. Shows the \
         caller chain with depth + defining file. Example: {\"symbol\":\"process\",\"depth\":3}."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol to trace callers for" },
                "depth": { "type": "integer", "description": "Max traversal depth (default 3, max 5)" }
            },
            "required": ["symbol"]
        })
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "trace_callers: invalid arguments: {e}. Expected {{\"symbol\":\"<name>\"}}."
                ))
            }
        };
        let depth = a.depth.unwrap_or(3).min(5);
        let index = self.index.clone();
        let root = ctx.working_dir.clone();
        let symbol = a.symbol.clone();
        tokio::task::spawn_blocking(move || render(&index, &root, &symbol, depth))
            .await
            .unwrap_or_else(|_| err("trace_callers: task failed"))
    }
}

fn render(index: &CodeIndex, root: &Path, symbol: &str, depth: usize) -> ToolResult {
    let g = index.get(root);
    let croot = canonical(root);
    let root: &Path = &croot;
    let matches = g.find_by_name(symbol);
    if matches.is_empty() {
        return err(format!(
            "Symbol '{symbol}' not found in code graph ({} symbols indexed).",
            g.node_count()
        ));
    }
    let mut out = String::new();
    for sym in &matches {
        out.push_str(&format!(
            "Callers of {} ({:?}) in {}:\n",
            sym.name,
            sym.kind,
            display_path(&sym.file, root)
        ));
        let callers = g.trace_callers(sym.id, depth);
        if callers.is_empty() {
            out.push_str("  (no callers found)\n");
        } else {
            for (caller_id, d) in &callers {
                if let Some(node) = g.node(*caller_id) {
                    let indent = "  ".repeat(*d);
                    out.push_str(&format!(
                        "{}[depth {}] {} ({:?}) — {}\n",
                        indent,
                        d,
                        node.name,
                        node.kind,
                        display_path(&node.file, root)
                    ));
                }
            }
        }
    }
    ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn traces_callers_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("lib.rs"), "fn target() {}\n").unwrap();
        std::fs::write(d.path().join("a.rs"), "fn caller_a() { target(); }\n").unwrap();
        let tool = TraceCallersTool::new(Arc::new(CodeIndex::new()));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"symbol":"target"}"#, &ctx).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("Callers of target"), "{}", r.content);
        assert!(r.content.contains("caller_a"), "{}", r.content);
    }

    #[tokio::test]
    async fn missing_symbol_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn x() {}\n").unwrap();
        let tool = TraceCallersTool::new(Arc::new(CodeIndex::new()));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"symbol":"nope"}"#, &ctx).await;
        assert!(r.is_error);
        assert!(
            r.content.contains("not found in code graph"),
            "{}",
            r.content
        );
    }
}
