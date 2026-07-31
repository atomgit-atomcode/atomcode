//! `trace_chain` — shortest call chain between two symbols (BFS, ≤10 hops). `Safe`.

use super::index::CodeIndex;
use super::{canonical, display_path, err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

pub struct TraceChainTool {
    index: Arc<CodeIndex>,
}

impl TraceChainTool {
    pub fn new(index: Arc<CodeIndex>) -> Self {
        Self { index }
    }
}

#[derive(Deserialize)]
struct Args {
    from: String,
    to: String,
}

#[async_trait]
impl Tool for TraceChainTool {
    fn name(&self) -> &str {
        "trace_chain"
    }
    fn description(&self) -> &str {
        "Find the shortest call chain between two symbols (BFS, max 10 hops). \
         Example: {\"from\":\"main\",\"to\":\"db_query\"}."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string", "description": "Source symbol name" },
                "to": { "type": "string", "description": "Target symbol name" }
            },
            "required": ["from", "to"]
        })
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("trace_chain: invalid arguments: {e}. Expected {{\"from\":\"<a>\",\"to\":\"<b>\"}}.")),
        };
        let index = self.index.clone();
        let root = ctx.working_dir.clone();
        let (from, to) = (a.from.clone(), a.to.clone());
        tokio::task::spawn_blocking(move || render(&index, &root, &from, &to))
            .await
            .unwrap_or_else(|_| err("trace_chain: task failed"))
    }
}

fn render(index: &CodeIndex, root: &Path, from: &str, to: &str) -> ToolResult {
    let g = index.get(root);
    let croot = canonical(root);
    let root: &Path = &croot;
    let from_matches = g.find_by_name(from);
    let to_matches = g.find_by_name(to);
    if from_matches.is_empty() {
        return err(format!("Source symbol '{from}' not found in code graph."));
    }
    if to_matches.is_empty() {
        return err(format!("Target symbol '{to}' not found in code graph."));
    }
    for from_sym in &from_matches {
        for to_sym in &to_matches {
            if let Some(path) = g.shortest_path(from_sym.id, to_sym.id) {
                let mut out = format!(
                    "Call chain from '{from}' to '{to}' ({} hops):\n",
                    path.len().saturating_sub(1)
                );
                for (i, sid) in path.iter().enumerate() {
                    if let Some(node) = g.node(*sid) {
                        let arrow = if i == 0 { "" } else { "→ " };
                        out.push_str(&format!(
                            "  {}{} ({:?}) — {}\n",
                            arrow,
                            node.name,
                            node.kind,
                            display_path(&node.file, root)
                        ));
                    }
                }
                return ok(out);
            }
        }
    }
    ok(format!(
        "No call chain found from '{from}' to '{to}' (max 10 hops)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn finds_chain() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.rs"),
            "fn c() {}\nfn b() { c(); }\nfn a() { b(); }\n",
        )
        .unwrap();
        let tool = TraceChainTool::new(Arc::new(CodeIndex::new()));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"from":"a","to":"c"}"#, &ctx).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("Call chain from 'a' to 'c'"),
            "{}",
            r.content
        );
        assert!(
            r.content.contains("a (") && r.content.contains("b (") && r.content.contains("c ("),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn no_chain_is_reported() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let tool = TraceChainTool::new(Arc::new(CodeIndex::new()));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"from":"a","to":"b"}"#, &ctx).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No call chain"), "{}", r.content);
    }
}
