//! `file_dependencies` — which files a file USES (its symbols' callees) and which files
//! USE it (callers). `Safe`.

use super::index::CodeIndex;
use super::{canonical, display_path, err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

pub struct FileDependenciesTool {
    index: Arc<CodeIndex>,
}

impl FileDependenciesTool {
    pub fn new(index: Arc<CodeIndex>) -> Self {
        Self { index }
    }
}

#[derive(Deserialize)]
struct Args {
    file: String,
}

#[async_trait]
impl Tool for FileDependenciesTool {
    fn name(&self) -> &str {
        "file_dependencies"
    }
    fn description(&self) -> &str {
        "Show a file's dependencies: which files it USES (its symbols' callees) and which \
         files USE it (callers). Relative paths resolve against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "File path (absolute or relative to the working directory)" }
            },
            "required": ["file"]
        })
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("file_dependencies: invalid arguments: {e}. Expected {{\"file\":\"<path>\"}}.")),
        };
        let index = self.index.clone();
        let root = ctx.working_dir.clone();
        let file = resolve_path(&a.file, &ctx.working_dir);
        let display = a.file.clone();
        tokio::task::spawn_blocking(move || render(&index, &root, &file, &display))
            .await
            .unwrap_or_else(|_| err("file_dependencies: task failed"))
    }
}

fn render(index: &CodeIndex, root: &Path, file: &Path, display: &str) -> ToolResult {
    let g = index.get(root);
    let croot = canonical(root);
    let root: &Path = &croot;
    let cfile = canonical(file);
    let file: &Path = &cfile;
    let symbols = match g.symbols_in_file(file) {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return err(format!("File '{display}' not found in the code graph (no indexed symbols).")),
    };

    let mut uses: HashSet<std::path::PathBuf> = HashSet::new();
    let mut used_by: HashSet<std::path::PathBuf> = HashSet::new();
    for sid in &symbols {
        if let Some(edges) = g.callees(*sid) {
            for e in edges {
                if let Some(node) = g.node(e.to) {
                    if node.file != file {
                        uses.insert(node.file.clone());
                    }
                }
            }
        }
        if let Some(edges) = g.callers(*sid) {
            for e in edges {
                if let Some(node) = g.node(e.to) {
                    if node.file != file {
                        used_by.insert(node.file.clone());
                    }
                }
            }
        }
    }

    let mut out = format!("File dependencies for {}:\n\n", display_path(file, root));
    out.push_str(&format!("USES ({} files):\n", uses.len()));
    out.push_str(&format_files(&uses, root));
    out.push_str(&format!("\nUSED BY ({} files):\n", used_by.len()));
    out.push_str(&format_files(&used_by, root));
    ok(out)
}

fn format_files(files: &HashSet<std::path::PathBuf>, root: &Path) -> String {
    if files.is_empty() {
        return "  (none)\n".to_string();
    }
    let mut sorted: Vec<String> = files.iter().map(|f| display_path(f, root)).collect();
    sorted.sort();
    sorted.iter().map(|f| format!("  {f}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn reports_uses_and_used_by() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("dep.rs"), "pub fn dep_fn() {}\n").unwrap();
        std::fs::write(d.path().join("mid.rs"), "fn mid() { dep_fn(); }\n").unwrap();
        std::fs::write(d.path().join("top.rs"), "fn top() { mid(); }\n").unwrap();
        let tool = FileDependenciesTool::new(Arc::new(CodeIndex::new()));
        let ctx = ToolContext { working_dir: d.path().to_path_buf(), cancel: CancellationToken::new(), progress: atomcode_kernel::tool::ProgressSink::noop() };
        let r = tool.execute(r#"{"file":"mid.rs"}"#, &ctx).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("File dependencies for mid.rs"), "{}", r.content);
        // mid uses dep.rs, used by top.rs
        assert!(r.content.contains("dep.rs"), "uses: {}", r.content);
        assert!(r.content.contains("top.rs"), "used by: {}", r.content);
    }
}
