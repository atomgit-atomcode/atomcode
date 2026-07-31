//! `blast_radius` — estimate the impact of changing a file: direct + indirect
//! dependent files + total. `Safe`.

use super::index::CodeIndex;
use super::{canonical, display_path, err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

pub struct BlastRadiusTool {
    index: Arc<CodeIndex>,
}

impl BlastRadiusTool {
    pub fn new(index: Arc<CodeIndex>) -> Self {
        Self { index }
    }
}

#[derive(Deserialize)]
struct Args {
    file: String,
}

#[async_trait]
impl Tool for BlastRadiusTool {
    fn name(&self) -> &str {
        "blast_radius"
    }
    fn description(&self) -> &str {
        "Estimate the blast radius of changing a file: direct dependents (depth 1), \
         indirect dependents (depth 2-3), and total impacted file count. Relative paths \
         resolve against the working directory."
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
            Err(e) => {
                return err(format!(
                    "blast_radius: invalid arguments: {e}. Expected {{\"file\":\"<path>\"}}."
                ))
            }
        };
        let index = self.index.clone();
        let root = ctx.working_dir.clone();
        let file = resolve_path(&a.file, &ctx.working_dir);
        let display = a.file.clone();
        tokio::task::spawn_blocking(move || render(&index, &root, &file, &display))
            .await
            .unwrap_or_else(|_| err("blast_radius: task failed"))
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
        _ => {
            return err(format!(
                "File '{display}' not found in the code graph (no indexed symbols)."
            ))
        }
    };

    // Direct dependents (depth 1): files whose symbols directly call this file's symbols.
    let mut direct: HashSet<std::path::PathBuf> = HashSet::new();
    for sid in &symbols {
        if let Some(edges) = g.callers(*sid) {
            for e in edges {
                if let Some(node) = g.node(e.to) {
                    if node.file != file {
                        direct.insert(node.file.clone());
                    }
                }
            }
        }
    }
    // Indirect dependents (depth 2-3) = transitive dependents minus the direct ones.
    let indirect: HashSet<std::path::PathBuf> = g
        .file_dependents(file, 3)
        .into_iter()
        .filter(|f| !direct.contains(f))
        .collect();
    let total = direct.len() + indirect.len();

    let mut out = format!("Blast radius for {}:\n\n", display_path(file, root));
    out.push_str(&format!("DIRECT DEPENDENTS ({} files):\n", direct.len()));
    out.push_str(&format_files(&direct, root));
    out.push_str(&format!(
        "\nINDIRECT DEPENDENTS ({} files):\n",
        indirect.len()
    ));
    out.push_str(&format_files(&indirect, root));
    out.push_str(&format!("\nTOTAL IMPACT: {total} files\n"));
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
    async fn reports_dependents() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("core.rs"), "pub fn core_fn() {}\n").unwrap();
        std::fs::write(d.path().join("user.rs"), "fn u() { core_fn(); }\n").unwrap();
        let tool = BlastRadiusTool::new(Arc::new(CodeIndex::new()));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"file":"core.rs"}"#, &ctx).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("Blast radius for core.rs"),
            "{}",
            r.content
        );
        assert!(r.content.contains("user.rs"), "{}", r.content);
        assert!(r.content.contains("TOTAL IMPACT:"), "{}", r.content);
    }

    #[tokio::test]
    async fn unknown_file_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn a() {}\n").unwrap();
        let tool = BlastRadiusTool::new(Arc::new(CodeIndex::new()));
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let r = tool.execute(r#"{"file":"ghost.rs"}"#, &ctx).await;
        assert!(r.is_error);
        assert!(
            r.content.contains("not found in the code graph"),
            "{}",
            r.content
        );
    }
}
