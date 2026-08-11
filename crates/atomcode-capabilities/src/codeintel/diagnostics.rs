//! Backward-compatible `diagnostics` facade.
//!
//! New coding runtimes expose the unified `lsp` tool instead. This type remains public
//! for embedders compiled against the former feature API, but is intentionally not
//! auto-registered by [`super::register_codeintel_tools`].

use super::lsp::types::DiagnosticSeverity;
use super::{err, ok, LspManager, LspTool};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub struct DiagnosticsTool {
    manager: Arc<LspManager>,
    unified: LspTool,
}

impl DiagnosticsTool {
    pub fn new(manager: Arc<LspManager>) -> Self {
        Self {
            unified: LspTool::new(manager.clone()),
            manager,
        }
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
        "Deprecated compatibility alias for lsp(operation=diagnostics)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "severity": { "type": "string", "enum": ["error", "warning", "all"] }
            }
        })
    }

    async fn execute(&self, arguments: &str, context: &ToolContext) -> ToolResult {
        let arguments: Args = match serde_json::from_str(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return err(format!("diagnostics: invalid arguments: {error}")),
        };
        if let Some(file_path) = arguments.file_path {
            return self
                .unified
                .execute(
                    &json!({
                        "operation": "diagnostics",
                        "file_path": file_path,
                        "severity": arguments.severity.unwrap_or_else(|| "error".into())
                    })
                    .to_string(),
                    context,
                )
                .await;
        }
        if !self.manager.has_servers().await {
            return ok(
                "No diagnostics: no language server is running yet. Pass file_path to start one."
                    .to_string(),
            );
        }
        let severity = arguments.severity.as_deref().unwrap_or("error");
        let mut diagnostics = self.manager.all_diagnostics().await;
        match severity {
            "error" => diagnostics.retain(|item| item.severity == DiagnosticSeverity::Error),
            "warning" => diagnostics.retain(|item| {
                matches!(
                    item.severity,
                    DiagnosticSeverity::Error | DiagnosticSeverity::Warning
                )
            }),
            "all" => {}
            other => return err(format!("diagnostics: invalid severity '{other}'")),
        }
        diagnostics.sort_by(|left, right| {
            left.severity
                .cmp(&right.severity)
                .then(left.file.cmp(&right.file))
                .then(left.line.cmp(&right.line))
        });
        if diagnostics.is_empty() {
            return ok(format!("No diagnostics found (filter: {severity})."));
        }
        ok(format!(
            "Found {} diagnostics:\n{}",
            diagnostics.len(),
            diagnostics
                .iter()
                .map(|item| item.display_line())
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codeintel::lsp::{LspServerConfig, LspServerRegistry};
    use atomcode_kernel::tool::ProgressSink;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn compatibility_alias_degrades_when_server_is_missing() {
        let mut registry = LspServerRegistry::empty();
        registry.insert(
            "rs",
            LspServerConfig {
                command: "atomcode-no-such-lsp-binary-xyz".into(),
                args: vec![],
                root_markers: vec![],
            },
        );
        let tool = DiagnosticsTool::new(Arc::new(LspManager::with_registry(registry)));
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("main.rs"), "fn main() {}\n").unwrap();
        let result = tool
            .execute(
                r#"{"file_path":"main.rs"}"#,
                &ToolContext {
                    working_dir: workspace.path().to_path_buf(),
                    cancel: CancellationToken::new(),
                    progress: ProgressSink::noop(),
                    requester: None,
                },
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("LSP unavailable"));
    }
}
