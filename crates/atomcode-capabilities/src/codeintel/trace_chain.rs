//! `trace_chain` — shortest call chain between two symbols (BFS, ≤10 hops). `Safe`.

use super::index::{CodeIndex, IndexLimits};
use super::{canonical, display_path, err, not_indexed, ok};
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
    /// Path suffix that picks one of several same-named symbols.
    #[serde(default)]
    file: Option<String>,
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
                "to": { "type": "string", "description": "Target symbol name" },
                "file": { "type": "string", "description": "Optional path suffix (e.g. \"src/db.rs\") that picks one of several same-named symbols. Required when a name is ambiguous." }
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
        let file = a.file.clone();
        tokio::task::spawn_blocking(move || render(&index, &root, &from, &to, file.as_deref()))
            .await
            .unwrap_or_else(|_| err("trace_chain: task failed"))
    }
}

/// Local alias so all three trace tools narrow through the one shared implementation.
fn narrow<'a>(
    nodes: Vec<&'a super::graph::SymbolNode>,
    file: Option<&str>,
) -> Vec<&'a super::graph::SymbolNode> {
    super::narrow_by_file(nodes, file)
}

fn render(index: &CodeIndex, root: &Path, from: &str, to: &str, file: Option<&str>) -> ToolResult {
    let g = match index.get_limited(root, &IndexLimits::default()) {
        Ok(g) => g,
        Err(e) => return err(format!("trace_chain: {}", e)),
    };
    let croot = canonical(root);
    let root: &Path = &croot;
    let filter = file.map(str::trim).filter(|f| !f.is_empty());
    let from_matches = narrow(g.find_by_name(from), filter);
    let to_matches = narrow(g.find_by_name(to), filter);
    if from_matches.is_empty() {
        return err(match filter {
            Some(f) => format!(
                "Source symbol '{from}' not found in code graph under '{f}' — the index covers {} \
(only the working directory is indexed).",
                root.display()
            ),
            None => not_indexed("trace_chain", "Source", from, root),
        });
    }
    if to_matches.is_empty() {
        return err(match filter {
            Some(f) => format!(
                "Target symbol '{to}' not found in code graph under '{f}' — the index covers {} \
(only the working directory is indexed).",
                root.display()
            ),
            None => not_indexed("trace_chain", "Target", to, root),
        });
    }
    // Two same-named symbols in different files: answering with the first one found
    // is a guess presented as a fact (the smoke run reported a chain that crossed
    // files). Refuse and list the candidates so the caller can pass `file`.
    if filter.is_none() && (from_matches.len() > 1 || to_matches.len() > 1) {
        let mut out = String::from(
            "trace_chain: several symbols share these names — pass \"file\" (a path suffix) to disambiguate:\n",
        );
        for (label, hits) in [("from", &from_matches), ("to", &to_matches)] {
            if hits.len() > 1 {
                for n in hits.iter().take(5) {
                    out.push_str(&format!(
                        "  {label} {} ({:?}) — {}\n",
                        n.name,
                        n.kind,
                        display_path(&n.file, root)
                    ));
                }
            }
        }
        return err(out);
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
        let tool = TraceChainTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
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
        let tool = TraceChainTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
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

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    /// Two files, each with `main` calling `add`: without a file filter the tool
    /// currently answers with ONE of them, which is a guess presented as a fact.
    fn ambiguous_fixture(dir: &Path) {
        std::fs::write(dir.join("a.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
    }

    #[tokio::test]
    async fn ambiguous_symbol_names_are_refused_with_candidates() {
        let d = tempfile::tempdir().unwrap();
        ambiguous_fixture(d.path());
        let tool = TraceChainTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let r = tool
            .execute(r#"{"from":"main","to":"add"}"#, &ctx(d.path()))
            .await;
        assert!(
            r.is_error,
            "ambiguous names must be refused, got: {}",
            r.content
        );
        assert!(
            r.content.contains("a.rs") && r.content.contains("b.rs"),
            "the error must list every candidate: {}",
            r.content
        );
        assert!(
            r.content.contains("file"),
            "the error must point at the `file` filter: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn file_filter_selects_one_candidate() {
        let d = tempfile::tempdir().unwrap();
        ambiguous_fixture(d.path());
        let tool = TraceChainTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let r = tool
            .execute(
                r#"{"from":"main","to":"add","file":"b.rs"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("b.rs") && !r.content.contains("a.rs"),
            "the chain must stay inside the requested file: {}",
            r.content
        );
    }
}
