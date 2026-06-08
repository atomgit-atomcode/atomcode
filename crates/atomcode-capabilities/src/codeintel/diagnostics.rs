//! `diagnostics` — real-time compiler/linter diagnostics from a Language Server.
//! Non-destructive ⇒ always `Safe`. Holds a shared [`LspManager`]; degrades gracefully
//! when the language server is not installed.

use super::lsp::types::DiagnosticSeverity;
use super::lsp::LspManager;
use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub struct DiagnosticsTool {
    manager: Arc<LspManager>,
}

impl DiagnosticsTool {
    pub fn new(manager: Arc<LspManager>) -> Self {
        Self { manager }
    }
}

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    severity: Option<String>,
}

#[async_trait]
impl Tool for DiagnosticsTool {
    fn name(&self) -> &str {
        "diagnostics"
    }
    fn description(&self) -> &str {
        "Get real-time compiler/linter diagnostics from a Language Server (type errors, \
         missing imports, etc.) without running a full build. Optionally filter by \
         file_path and severity. Requires the language server installed; reports \
         gracefully if it is not."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "File to check (absolute or relative). Omit for all project diagnostics." },
                "severity": { "type": "string", "enum": ["error", "warning", "all"], "description": "Filter level (default: error)." }
            }
        })
    }
    // read-only → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("diagnostics: invalid arguments: {e}.")),
        };
        let severity = a.severity.as_deref().unwrap_or("error");

        let mut diags = if let Some(fp) = &a.file_path {
            let path = resolve_path(fp, &ctx.working_dir);
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => return err(format!("diagnostics: cannot read {}: {e}", path.display())),
            };
            if !self.manager.notify_file_changed(&ctx.working_dir, &path, &content).await {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("?");
                return ok(format!(
                    "LSP not available: no language server for .{ext} (not configured, or its binary is not installed)."
                ));
            }
            // Give the server a moment to analyze + publish.
            tokio::time::sleep(Duration::from_millis(self.manager.settle_delay_ms())).await;
            self.manager.diagnostics(&path).await
        } else {
            if !self.manager.has_servers().await {
                return ok("No diagnostics: no language server is running yet. Pass file_path to start one.".to_string());
            }
            self.manager.all_diagnostics().await
        };

        match severity {
            "error" => diags.retain(|d| d.severity == DiagnosticSeverity::Error),
            "warning" => diags.retain(|d| matches!(d.severity, DiagnosticSeverity::Error | DiagnosticSeverity::Warning)),
            _ => {} // "all"
        }
        diags.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.file.cmp(&b.file)).then(a.line.cmp(&b.line)));

        if diags.is_empty() {
            let scope = a.file_path.as_deref().map(|f| format!(" in {f}")).unwrap_or_default();
            return ok(format!("No diagnostics found{scope} (filter: {severity})."));
        }
        let lines: Vec<String> = diags.iter().map(|d| d.display_line()).collect();
        let plural = if diags.len() == 1 { "" } else { "s" };
        ok(format!("Found {} diagnostic{}:\n\n{}", diags.len(), plural, lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codeintel::lsp::registry::{LspServerConfig, LspServerRegistry};
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn missing_binary_manager() -> Arc<LspManager> {
        let mut r = LspServerRegistry::empty();
        r.insert("rs", LspServerConfig { command: "atomcode-no-such-lsp-xyz".into(), args: vec![], root_markers: vec![] });
        Arc::new(LspManager::with_registry(r))
    }
    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext { working_dir: dir.to_path_buf(), cancel: CancellationToken::new(), progress: atomcode_kernel::tool::ProgressSink::noop() }
    }

    #[tokio::test]
    async fn reports_unavailable_when_server_not_installed() {
        let tool = DiagnosticsTool::new(missing_binary_manager());
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {}\n").unwrap();
        let r = tool.execute(r#"{"file_path":"a.rs"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("LSP not available"), "{}", r.content);
    }

    #[tokio::test]
    async fn no_servers_no_filepath() {
        let tool = DiagnosticsTool::new(Arc::new(LspManager::with_registry(LspServerRegistry::empty())));
        let d = tempfile::tempdir().unwrap();
        let r = tool.execute(r#"{}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("no language server is running"), "{}", r.content);
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let tool = DiagnosticsTool::new(missing_binary_manager());
        let d = tempfile::tempdir().unwrap();
        let r = tool.execute(r#"{"file_path":"ghost.rs"}"#, &ctx(d.path())).await;
        assert!(r.is_error);
        assert!(r.content.contains("cannot read"), "{}", r.content);
    }
}
