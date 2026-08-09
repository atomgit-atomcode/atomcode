//! One read-only model-facing LSP tool. Language servers are selected and started lazily
//! by the shared manager; an unavailable server is a normal, degradable result rather
//! than a failed turn.

use super::lsp::types::{DiagnosticSeverity, Location};
use super::{err, ok, resolve_path, LspManager};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MAX_LOCATIONS: usize = 200;
const MAX_HOVER_CHARS: usize = 12_000;
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

pub struct LspTool {
    manager: Arc<LspManager>,
}

impl LspTool {
    pub fn new(manager: Arc<LspManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Definition,
    References,
    Hover,
    Diagnostics,
}

#[derive(Debug, Deserialize)]
struct Args {
    operation: Operation,
    file_path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    character: Option<u32>,
    #[serde(default)]
    severity: Option<String>,
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Query a locally installed Language Server for semantic definition, references, hover, or diagnostics. Starts the matching server lazily. Read-only; if unavailable, fall back to read_symbol/find_references/search instead of retrying."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": ["definition", "references", "hover", "diagnostics"] },
                "file_path": { "type": "string", "description": "File path relative to the workspace or absolute within it" },
                "line": { "type": "integer", "minimum": 1, "description": "One-based line; required except for diagnostics" },
                "character": { "type": "integer", "minimum": 1, "description": "One-based character; required except for diagnostics" },
                "severity": { "type": "string", "enum": ["error", "warning", "all"], "description": "Diagnostics filter (default: error)" }
            },
            "required": ["operation", "file_path"]
        })
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let args: Args = match serde_json::from_str(args) {
            Ok(args) => args,
            Err(error) => return err(format!("lsp: invalid arguments: {error}")),
        };
        if args.file_path.trim().is_empty() {
            return err("lsp: file_path must not be empty");
        }
        // Validate the complete operation before touching the filesystem or lazily
        // starting an external language server. Tool schemas are guidance rather than
        // a trust boundary: direct callers and imperfect model output can still pass
        // values outside the advertised enum.
        let semantic_position = match args.operation {
            Operation::Definition | Operation::References | Operation::Hover => {
                match one_based_position(&args) {
                    Ok(position) => Some(position),
                    Err(error) => return err(error),
                }
            }
            Operation::Diagnostics => None,
        };
        let diagnostic_severity = match args.operation {
            Operation::Diagnostics => match args.severity.as_deref().unwrap_or("error") {
                severity @ ("error" | "warning" | "all") => Some(severity),
                other => {
                    return err(format!(
                        "lsp: invalid severity '{other}'; expected error, warning, or all"
                    ))
                }
            },
            _ => None,
        };
        let requested = resolve_path(&args.file_path, &ctx.working_dir);
        let root =
            std::fs::canonicalize(&ctx.working_dir).unwrap_or_else(|_| ctx.working_dir.clone());
        let path = match std::fs::canonicalize(&requested) {
            Ok(path) => path,
            Err(error) => {
                return err(format!(
                    "lsp: cannot resolve {}: {error}",
                    requested.display()
                ))
            }
        };
        if !path.starts_with(&root) {
            return err(format!(
                "lsp: file must be inside the workspace: {}",
                path.display()
            ));
        }
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(error) => return err(format!("lsp: cannot read {}: {error}", path.display())),
        };
        if content.len() > MAX_DOCUMENT_BYTES {
            return err(format!(
                "lsp: file is too large ({} bytes; limit is {MAX_DOCUMENT_BYTES})",
                content.len()
            ));
        }
        if ctx.cancel.is_cancelled() {
            return ok("LSP query cancelled.");
        }

        match args.operation {
            Operation::Definition | Operation::References | Operation::Hover => {
                let (line, character) = semantic_position.expect("validated semantic position");
                let result = match args.operation {
                    Operation::Definition => self
                        .manager
                        .definition(&root, &path, &content, line, character, &ctx.cancel)
                        .await
                        .map(|locations| render_locations("definitions", locations, &root)),
                    Operation::References => self
                        .manager
                        .references(&root, &path, &content, line, character, &ctx.cancel)
                        .await
                        .map(|locations| render_locations("references", locations, &root)),
                    Operation::Hover => self
                        .manager
                        .hover(&root, &path, &content, line, character, &ctx.cancel)
                        .await
                        .map(render_hover),
                    Operation::Diagnostics => unreachable!(),
                };
                if ctx.cancel.is_cancelled() {
                    return ok("LSP query cancelled.");
                }
                degradable(result)
            }
            Operation::Diagnostics => {
                if let Err(error) = self
                    .manager
                    .sync_document(&root, &path, &content, &ctx.cancel)
                    .await
                {
                    return diagnostics_sync_failure(error, &ctx.cancel);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(self.manager.settle_delay_ms())) => {}
                    _ = ctx.cancel.cancelled() => return ok("LSP diagnostics query cancelled."),
                }
                if let Err(error) = self
                    .manager
                    .refresh_pull_diagnostics(&root, &path, &ctx.cancel)
                    .await
                {
                    if ctx.cancel.is_cancelled() {
                        return ok("LSP diagnostics query cancelled.");
                    }
                    return unavailable(error);
                }
                let severity = diagnostic_severity.expect("validated diagnostics severity");
                let mut diagnostics = self.manager.diagnostics(&path).await;
                match severity {
                    "error" => diagnostics.retain(|d| d.severity == DiagnosticSeverity::Error),
                    "warning" => diagnostics.retain(|d| {
                        matches!(
                            d.severity,
                            DiagnosticSeverity::Error | DiagnosticSeverity::Warning
                        )
                    }),
                    "all" => {}
                    _ => unreachable!("diagnostics severity was validated before side effects"),
                }
                diagnostics.sort_by(|a, b| {
                    a.severity
                        .cmp(&b.severity)
                        .then(a.line.cmp(&b.line))
                        .then(a.column.cmp(&b.column))
                });
                if diagnostics.is_empty() {
                    return ok(format!(
                        "No diagnostics found in {} (filter: {severity}).",
                        display_path(&path, &root)
                    ));
                }
                let lines: Vec<_> = diagnostics
                    .iter()
                    .take(MAX_LOCATIONS)
                    .map(|d| {
                        let mut d = d.clone();
                        d.file = display_path(Path::new(&d.file), &root);
                        d.display_line()
                    })
                    .collect();
                let omitted = diagnostics.len().saturating_sub(MAX_LOCATIONS);
                let suffix = (omitted > 0)
                    .then(|| format!("\n… {omitted} more omitted"))
                    .unwrap_or_default();
                ok(format!(
                    "Found {} diagnostics:\n{}{}",
                    diagnostics.len(),
                    lines.join("\n"),
                    suffix
                ))
            }
        }
    }
}

fn one_based_position(args: &Args) -> Result<(u32, u32), String> {
    let line = args
        .line
        .filter(|value| *value > 0)
        .ok_or_else(|| "lsp: line must be a one-based positive integer".to_string())?;
    let character = args
        .character
        .filter(|value| *value > 0)
        .ok_or_else(|| "lsp: character must be a one-based positive integer".to_string())?;
    Ok((line, character))
}

fn degradable(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ok(content),
        Err(error) => unavailable(error),
    }
}

fn unavailable(error: String) -> ToolResult {
    ok(format!(
        "LSP unavailable: {error}. Fall back to read_symbol, find_references, grep, or read_file; do not retry this LSP call in the current runtime."
    ))
}

fn diagnostics_sync_failure(
    error: String,
    cancel: &tokio_util::sync::CancellationToken,
) -> ToolResult {
    if cancel.is_cancelled() {
        ok("LSP diagnostics query cancelled.")
    } else {
        unavailable(error)
    }
}

fn render_locations(kind: &str, locations: Vec<Location>, root: &Path) -> String {
    if locations.is_empty() {
        return format!("No semantic {kind} found.");
    }
    let total = locations.len();
    let mut lines: Vec<_> = locations
        .into_iter()
        .take(MAX_LOCATIONS)
        .map(|location| {
            format!(
                "{}:{}:{}",
                display_path(Path::new(&location.file), root),
                location.line,
                location.column
            )
        })
        .collect();
    if total > MAX_LOCATIONS {
        lines.push(format!("… {} more omitted", total - MAX_LOCATIONS));
    }
    format!("Found {total} semantic {kind}:\n{}", lines.join("\n"))
}

fn render_hover(value: Value) -> String {
    let contents = value.get("contents").unwrap_or(&value);
    let rendered = match contents {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| contents.to_string()),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map.get("value").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Null => String::new(),
        _ => contents.to_string(),
    };
    if rendered.is_empty() {
        return "No hover information found.".into();
    }
    let truncated: String = rendered.chars().take(MAX_HOVER_CHARS).collect();
    if truncated.chars().count() < rendered.chars().count() {
        format!("{truncated}\n… hover output truncated")
    } else {
        truncated
    }
}

fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codeintel::lsp::{LspServerConfig, LspServerRegistry};
    use atomcode_kernel::tool::ProgressSink;
    use tokio_util::sync::CancellationToken;

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            working_dir: root.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: ProgressSink::noop(),
            requester: None,
        }
    }

    fn unavailable_tool() -> LspTool {
        let mut registry = LspServerRegistry::empty();
        registry.insert(
            "rs",
            LspServerConfig {
                command: "atomcode-no-such-lsp-binary-xyz".into(),
                args: Vec::new(),
                root_markers: Vec::new(),
            },
        );
        LspTool::new(Arc::new(LspManager::with_registry(registry)))
    }

    #[tokio::test]
    async fn unavailable_server_is_a_degradable_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let result = unavailable_tool()
            .execute(
                r#"{"operation":"definition","file_path":"a.rs","line":1,"character":1}"#,
                &ctx(dir.path()),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("LSP unavailable"));
        assert!(result.content.contains("do not retry"));
    }

    #[tokio::test]
    async fn path_outside_workspace_is_rejected_before_reading() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "secret").unwrap();
        let result = unavailable_tool()
            .execute(
                &serde_json::json!({
                    "operation": "diagnostics",
                    "file_path": outside.path(),
                })
                .to_string(),
                &ctx(workspace.path()),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("inside the workspace"));
    }

    #[tokio::test]
    async fn semantic_operations_require_one_based_position() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let result = unavailable_tool()
            .execute(
                r#"{"operation":"hover","file_path":"a.rs"}"#,
                &ctx(dir.path()),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("line must be"));
    }

    #[tokio::test]
    async fn invalid_diagnostics_severity_is_rejected_before_file_or_server_work() {
        let dir = tempfile::tempdir().unwrap();
        let result = unavailable_tool()
            .execute(
                r#"{"operation":"diagnostics","file_path":"missing.rs","severity":"fatal"}"#,
                &ctx(dir.path()),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("invalid severity"));
        assert!(!result.content.contains("cannot resolve"));
        assert!(!result.content.contains("LSP unavailable"));
    }

    #[test]
    fn cancelled_diagnostics_sync_is_not_reported_as_unavailable() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result =
            diagnostics_sync_failure("language server startup cancelled".to_string(), &cancel);
        assert!(!result.is_error);
        assert!(result.content.contains("cancelled"));
        assert!(!result.content.contains("unavailable"));
        assert!(!result.content.contains("do not retry"));
    }

    #[test]
    fn hover_and_locations_are_bounded_and_readable() {
        assert_eq!(
            render_hover(json!({"contents":{"kind":"markdown","value":"`u32`"}})),
            "`u32`"
        );
        let root = if cfg!(windows) {
            std::path::PathBuf::from("C:\\workspace")
        } else {
            std::path::PathBuf::from("/workspace")
        };
        let file = root.join("src/lib.rs").display().to_string();
        let output = render_locations(
            "definitions",
            vec![Location {
                file,
                line: 2,
                column: 4,
            }],
            &root,
        );
        assert!(output.contains("src/lib.rs:2:4"));
    }
}
