//! `trace_callers` — reverse call graph (who calls a symbol), BFS to a depth. `Safe`.

use super::index::{CodeIndex, IndexLimits};
use super::{ambiguous_symbols, canonical, display_path, err, narrow_by_file, not_indexed, ok};
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
    /// Path suffix that picks one of several same-named symbols.
    #[serde(default)]
    file: Option<String>,
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
                    "trace_callers: invalid arguments: {e}. Expected {{\"symbol\":\"<name>\"}}."
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
            .unwrap_or_else(|_| err("trace_callers: task failed"))
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
        Err(e) => return err(format!("trace_callers: {}", e)),
    };
    let croot = canonical(root);
    let root: &Path = &croot;
    let filter = file.map(str::trim).filter(|f| !f.is_empty());
    let matches = narrow_by_file(g.find_by_name(symbol), filter);
    if matches.is_empty() {
        return err(match filter {
            Some(f) => format!(
                "trace_callers: symbol '{symbol}' not found under '{f}' — the index covers {} \
(only the working directory is indexed).",
                root.display()
            ),
            None => not_indexed("trace_callers", "", symbol, root),
        });
    }
    // Same refusal `trace_chain` gives: dumping every same-named `add`/`main` mixes the
    // answer with unrelated projects and the caller cannot tell which part is theirs.
    if filter.is_none() && matches.len() > 1 {
        return err(ambiguous_symbols("trace_callers", symbol, &matches, root));
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

    /// Two files, each with `main` calling `add`: dumping every same-named symbol buries
    /// the one the caller asked about. `trace_chain` already refuses this way.
    #[tokio::test]
    async fn ambiguous_symbol_names_are_refused_with_candidates() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
        std::fs::write(d.path().join("b.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
        let tool =
            TraceCallersTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"symbol":"add","depth":1}"#, &ctx).await;
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
        std::fs::write(d.path().join("a.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
        std::fs::write(d.path().join("b.rs"), "fn add() {}\nfn main() { add(); }\n").unwrap();
        let tool =
            TraceCallersTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool
            .execute(r#"{"symbol":"add","depth":1,"file":"b.rs"}"#, &ctx)
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("b.rs") && !r.content.contains("a.rs"),
            "the answer must stay inside the requested file: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn traces_callers_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("lib.rs"), "fn target() {}\n").unwrap();
        std::fs::write(d.path().join("a.rs"), "fn caller_a() { target(); }\n").unwrap();
        let tool =
            TraceCallersTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
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
    async fn traces_kotlin_callers_across_files() {
        // End-to-end proof the Kotlin call graph works: .kt files are walked,
        // symbols + call edges extracted, and trace_callers resolves cross-file.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("Target.kt"),
            "fun target() {}\nclass Widget {}\n",
        )
        .unwrap();
        // callerA does a plain call; makeWidget does a constructor call `Widget()`
        // (Kotlin constructors parse as a plain call → must produce a callee edge).
        std::fs::write(
            d.path().join("Caller.kt"),
            "fun callerA() { target() }\nfun makeWidget() { Widget() }\n",
        )
        .unwrap();
        let tool =
            TraceCallersTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"symbol":"target"}"#, &ctx).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("callerA"), "{}", r.content);
        // Constructor call edge: `Widget()` in makeWidget → the Widget class.
        let w = tool.execute(r#"{"symbol":"Widget"}"#, &ctx).await;
        assert!(!w.is_error, "{}", w.content);
        assert!(w.content.contains("makeWidget"), "{}", w.content);
    }

    #[tokio::test]
    async fn missing_symbol_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn x() {}\n").unwrap();
        let tool =
            TraceCallersTool::new(Arc::new(CodeIndex::new(&crate::product_dirs::test_dirs())));
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
