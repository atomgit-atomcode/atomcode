//! Diagnostics tool — surfaces real-time compiler/linter diagnostics from
//! the Language Server without running a full build.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};
use crate::lsp::types::DiagnosticSeverity;

pub struct DiagnosticsTool;

#[derive(Deserialize)]
struct DiagnosticsArgs {
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    severity: Option<String>,
}

#[async_trait]
impl Tool for DiagnosticsTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "diagnostics",
            description: "Get real-time compiler/linter diagnostics from the Language Server. Returns type errors, missing imports, and other issues without running a full build. Optionally filter by file path and severity.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Absolute path to check. Omit for all project diagnostics."
                    },
                    "severity": {
                        "type": "string",
                        "enum": ["error", "warning", "all"],
                        "description": "Filter level. Default: error."
                    }
                },
                "required": []
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<DiagnosticsArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let Some(file_path) = parsed.file_path.as_deref() else {
            return self.approval(args);
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        match super::approval_for_path(file_path, &working_dir, super::ExternalPathAction::Read) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: DiagnosticsArgs = serde_json::from_str(args).unwrap_or(DiagnosticsArgs {
            file_path: None,
            severity: None,
        });

        let lsp = match &ctx.lsp {
            Some(lsp) => lsp,
            None => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "LSP not available. No language servers are configured or enabled."
                        .into(),
                    success: true,
                });
            }
        };

        let severity_filter = parsed.severity.as_deref().unwrap_or("error");

        // If a file path is given, sync the current file contents before reading
        // the diagnostics cache. LSP diagnostics are notification-driven.
        if let Some(ref fp) = parsed.file_path {
            let path = std::path::Path::new(fp);
            if let Ok(content) = tokio::fs::read_to_string(path).await {
                if lsp.notify_file_changed(path, &content).await? {
                    let delay = lsp.diagnostics_settle_delay_ms();
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            } else {
                let _ = lsp.ensure_server(path).await;
            }
        }

        // Collect diagnostics.
        let mut diagnostics = if let Some(ref fp) = parsed.file_path {
            let path = std::path::Path::new(fp);
            lsp.diagnostics(path).await
        } else {
            lsp.all_diagnostics().await
        };

        // Apply severity filter.
        match severity_filter {
            "error" => {
                diagnostics.retain(|d| d.severity == DiagnosticSeverity::Error);
            }
            "warning" => {
                diagnostics.retain(|d| {
                    d.severity == DiagnosticSeverity::Error
                        || d.severity == DiagnosticSeverity::Warning
                });
            }
            // "all" — keep everything.
            _ => {}
        }

        // Sort by severity (errors first), then by file, then by line.
        diagnostics.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line.cmp(&b.line))
        });

        if diagnostics.is_empty() {
            let scope = if let Some(ref fp) = parsed.file_path {
                format!(" in {}", fp)
            } else {
                String::new()
            };
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "No diagnostics found{} (filter: {}).",
                    scope, severity_filter
                ),
                success: true,
            });
        }

        let count = diagnostics.len();
        let lines: Vec<String> = diagnostics.iter().map(|d| d.display_line()).collect();
        let mut output = format!(
            "Found {} diagnostic{}:\n\n",
            count,
            if count == 1 { "" } else { "s" }
        );
        output.push_str(&lines.join("\n"));

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::types::Diagnostic;

    #[test]
    fn definition_has_correct_name() {
        let tool = DiagnosticsTool;
        assert_eq!(tool.definition().name, "diagnostics");
    }

    #[test]
    fn approval_is_auto() {
        let tool = DiagnosticsTool;
        assert!(matches!(
            tool.approval("{}"),
            ApprovalRequirement::AutoApprove
        ));
    }

    #[tokio::test]
    async fn approval_auto_without_file_path() {
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));

        assert!(matches!(
            DiagnosticsTool.approval_with_context("{}", &ctx),
            ApprovalRequirement::AutoApprove
        ));
    }

    #[tokio::test]
    async fn approval_requires_read_confirmation_for_external_file() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = serde_json::json!({ "file_path": file }).to_string();

        assert!(matches!(
            DiagnosticsTool.approval_with_context(&args, &ctx),
            ApprovalRequirement::RequireApproval(_)
        ));
    }

    #[tokio::test]
    async fn returns_lsp_not_available_when_no_lsp() {
        let tool = DiagnosticsTool;
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));
        let result = tool.execute("{}", &ctx).await.unwrap();
        assert!(result.output.contains("LSP not available"));
        assert!(result.success);
    }

    #[test]
    fn diagnostic_display_includes_line_info() {
        let d = Diagnostic {
            file: "src/main.rs".into(),
            line: 10,
            column: 5,
            end_line: None,
            end_column: None,
            severity: DiagnosticSeverity::Error,
            message: "type mismatch".into(),
            source: Some("rustc".into()),
            code: Some("E0308".into()),
        };
        let line = d.display_line();
        assert!(line.contains("src/main.rs:10:5"));
        assert!(line.contains("[ERROR]"));
        assert!(line.contains("E0308"));
        assert!(line.contains("type mismatch"));
    }
}
