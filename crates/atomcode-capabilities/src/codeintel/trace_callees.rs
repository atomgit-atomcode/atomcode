//! `trace_callees` — forward call graph (what a symbol calls), BFS to a depth. `Safe`.

use super::index::{CodeIndex, IndexLimits};
use super::{ambiguous_symbols, canonical, display_path, err, narrow_by_file, not_indexed, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

pub struct TraceCalleesTool {
    index: Arc<CodeIndex>,
}

impl TraceCalleesTool {
    pub fn new(index: Arc<CodeIndex>) -> Self {
        Self { index }
    }
}

#[derive(Deserialize)]
struct Args {
    symbol: String,
    #[serde(default)]
    depth: Option<usize>,
    /// Path suffix that picks one of several same-named symbols.
    #[serde(default)]
    file: Option<String>,
}

#[async_trait]
impl Tool for TraceCalleesTool {
    fn name(&self) -> &str {
        "trace_callees"
    }
    fn description(&self) -> &str {
        "Trace all callees of a symbol (forward call graph), BFS up to a depth. Shows the \
         callee chain with depth + defining file. Example: {\"symbol\":\"main\",\"depth\":2}."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol to trace callees for" },
                "depth": { "type": "integer", "description": "Max traversal depth (default 3, max 5)" },
                "file": { "type": "string", "description": "Optional path suffix (e.g. \"src/db.rs\") that picks one of several same-named symbols. Required when a name is ambiguous." }
            },
            "required": ["symbol"]
        })
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "trace_callees: invalid arguments: {e}. Expected {{\"symbol\":\"<name>\"}}."
                ))
            }
        };
        let depth = a.depth.unwrap_or(3).min(5);
        let index = self.index.clone();
        let root = ctx.working_dir.clone();
        let symbol = a.symbol.clone();
        let file = a.file.clone();
        tokio::task::spawn_blocking(move || render(&index, &root, &symbol, file.as_deref(), depth))
            .await
            .unwrap_or_else(|_| err("trace_callees: task failed"))
    }
}

fn render(
    index: &CodeIndex,
    root: &Path,
    symbol: &str,
    file: Option<&str>,
    depth: usize,
) -> ToolResult {
    let g = match index.get_limited(root, &IndexLimits::default()) {
        Ok(g) => g,
        Err(e) => return err(format!("trace_callees: {}", e)),
    };
    let croot = canonical(root);
    let root: &Path = &croot;
    let filter = file.map(str::trim).filter(|f| !f.is_empty());
    let matches = narrow_by_file(g.find_by_name(symbol), filter);
    if matches.is_empty() {
        return err(match filter {
            Some(f) => format!(
                "trace_callees: symbol '{symbol}' not found under '{f}' — the index covers {} \
(only the working directory is indexed).",
                root.display()
            ),
            None => not_indexed("trace_callees", "", symbol, root),
        });
    }
    // Same refusal `trace_chain` gives: several same-named `main`s would otherwise be
    // concatenated into one answer where the caller cannot tell which is theirs.
    if filter.is_none() && matches.len() > 1 {
        return err(ambiguous_symbols("trace_callees", symbol, &matches, root));
    }
    let mut out = String::new();
    for sym in &matches {
        out.push_str(&format!(
            "Callees of {} ({:?}) in {}:\n",
            sym.name,
            sym.kind,
            display_path(&sym.file, root)
        ));
        let callees = g.trace_callees(sym.id, depth);
        if callees.is_empty() {
            out.push_str("  (no callees found)\n");
        } else {
            for (callee_id, d) in &callees {
                if let Some(node) = g.node(*callee_id) {
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

    fn two_mains(dir: &Path) {
        std::fs::write(dir.join("a.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
    }

    /// Concatenating every same-named `main` gives an answer the caller cannot use.
    #[tokio::test]
    async fn ambiguous_symbol_names_are_refused_with_candidates() {
        let d = tempfile::tempdir().unwrap();
        two_mains(d.path());
        let tool =
            TraceCalleesTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"symbol":"main","depth":1}"#, &ctx).await;
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
        assert!(r.content.contains("file"), "{}", r.content);
    }

    #[tokio::test]
    async fn file_filter_narrows_to_one_symbol() {
        let d = tempfile::tempdir().unwrap();
        two_mains(d.path());
        let tool =
            TraceCalleesTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool
            .execute(r#"{"symbol":"main","depth":1,"file":"b.rs"}"#, &ctx)
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("b.rs") && !r.content.contains("a.rs"),
            "the answer must stay inside the requested file: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn traces_callees() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.rs"),
            "fn leaf() {}\nfn root() { leaf(); }\n",
        )
        .unwrap();
        let tool =
            TraceCalleesTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"symbol":"root"}"#, &ctx).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("Callees of root"), "{}", r.content);
        assert!(r.content.contains("leaf"), "{}", r.content);
    }
}
