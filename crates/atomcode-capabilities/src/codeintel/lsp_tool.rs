//! One read-only model-facing LSP tool. Language servers are selected and started lazily
//! by the shared manager; an unavailable server is a normal, degradable result rather
//! than a failed turn.

use super::lsp::types::{DiagnosticSeverity, Location};
use super::{err, ok, resolve_path, LspManager};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_LOCATIONS: usize = 200;
const MAX_HOVER_CHARS: usize = 12_000;
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

pub struct LspTool {
    manager: Arc<LspManager>,
    ide_binding: Option<PathBuf>,
    ide_discovery: Option<PathBuf>,
    local_enabled: bool,
}

impl LspTool {
    pub fn new(manager: Arc<LspManager>) -> Self {
        Self {
            manager,
            ide_binding: None,
            ide_discovery: None,
            local_enabled: true,
        }
    }
    pub fn with_ide_discovery(mut self, directory: Option<PathBuf>) -> Self {
        self.ide_discovery = directory;
        self
    }
    fn has_ide(&self) -> bool {
        self.ide_binding.is_some() || self.ide_discovery.is_some()
    }
    pub fn with_ide_binding(mut self, path: Option<PathBuf>, local_enabled: bool) -> Self {
        self.ide_binding = path;
        self.local_enabled = local_enabled;
        self
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Definition,
    References,
    Hover,
    Diagnostics,
    DocumentSymbols,
    Inspect,
    Status,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Backend {
    #[default]
    Auto,
    Ide,
    Local,
}

#[derive(Debug, Deserialize, Serialize)]
struct Args {
    operation: Operation,
    #[serde(default)]
    file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_id: Option<String>,
    #[serde(default)]
    backend: Backend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    occurrence: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    character: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    severity: Option<String>,
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        if self.has_ide() {
            "Read-only semantic queries through the configured IDE bridge: status (list live windows; select with window_id when ambiguous), hover, definition, references, diagnostics, document_symbols, or inspect (hover+definition+references in one request). Uses current editor buffers; positions are one-based UTF-16 columns, or line+symbol with occurrence for ambiguous matches. Reports document version/dirty state; diagnostics are editor snapshots, not build verification. Empty results do not prove lack of support. Explicit connection-file bindings take precedence over directory discovery. No writes. If unavailable, do not retry blindly; explicitly choose a configured local backend or text/AST tools (not semantic equivalents)."
        } else {
            "Query a locally installed Language Server for semantic definition, references, hover, or diagnostics. Starts the matching server lazily. Read-only; if unavailable, fall back to read_symbol/find_references/search instead of retrying."
        }
    }

    fn parameters_schema(&self) -> Value {
        let operations = if self.has_ide() {
            vec![
                "definition",
                "references",
                "hover",
                "diagnostics",
                "document_symbols",
                "inspect",
                "status",
            ]
        } else {
            vec!["definition", "references", "hover", "diagnostics"]
        };
        json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": operations },
                "backend": { "type": "string", "enum": ["auto", "ide", "local"], "description": "auto uses the configured IDE bridge; local requires lsp.enabled. No silent fallback." },
                "window_id": { "type": "string", "description": "IDE window ID from status; required to disambiguate multiple live matches. Explicit binding still wins." },
                "symbol": { "type": "string", "description": "IDE only: literal symbol on line instead of character. Uses the editor buffer." },
                "occurrence": { "type": "integer", "minimum": 1, "description": "IDE only: one-based match number, required for ambiguous symbols." },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 120, "description": "IDE request deadline (default 20 seconds)." },
                "file_path": { "type": "string", "description": "Required except for status: file path relative to the workspace or absolute within it" },
                "line": { "type": "integer", "minimum": 1, "description": "One-based line for position queries; omit for diagnostics/document_symbols" },
                "character": { "type": "integer", "minimum": 1, "description": "One-based character (UTF-16 for IDE); or use symbol in IDE mode" },
                "severity": { "type": "string", "enum": ["error", "warning", "all"], "description": "Diagnostics filter (default: error)" }
            },
            "required": if self.has_ide() { vec!["operation"] } else { vec!["operation", "file_path"] }
        })
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let args: Args = match serde_json::from_str(args) {
            Ok(args) => args,
            Err(error) => return err(format!("lsp: invalid arguments: {error}")),
        };
        if !matches!(args.operation, Operation::Status) && args.file_path.trim().is_empty() {
            return err("lsp: file_path must not be empty");
        }
        if matches!(args.operation, Operation::Status) && matches!(args.backend, Backend::Local) {
            return err("lsp: status inspects IDE connections; use backend ide or auto");
        }
        let use_ide = matches!(args.operation, Operation::Status)
            || matches!(args.backend, Backend::Ide)
            || (matches!(args.backend, Backend::Auto) && self.has_ide());
        if use_ide {
            let timeout = args.timeout_secs.unwrap_or(20);
            if !(1..=120).contains(&timeout)
                || args.occurrence == Some(0)
                || args.character == Some(0)
                || args.line == Some(0)
            {
                return err(
                    "lsp: invalid positive position/occurrence or IDE timeout (1-120 seconds)",
                );
            }
            if args.symbol.as_ref().is_some_and(|s| {
                s.is_empty() || s.encode_utf16().count() > 1024 || s.contains(['\r', '\n'])
            }) || (args.symbol.is_some() && args.character.is_some())
                || (args.occurrence.is_some() && args.symbol.is_none())
            {
                return err(
                    "lsp: use a single-line symbol or character; occurrence requires symbol",
                );
            }
            if !matches!(
                args.operation,
                Operation::Diagnostics | Operation::DocumentSymbols | Operation::Status
            ) && (args.line.is_none() || (args.character.is_none() && args.symbol.is_none()))
            {
                return err("lsp: IDE position requires line and character or symbol");
            }
            if args
                .severity
                .as_deref()
                .is_some_and(|s| !matches!(s, "error" | "warning" | "all"))
            {
                return err("lsp: invalid diagnostic severity");
            }
            if args
                .window_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 128)
            {
                return err("lsp: invalid IDE window_id");
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
            let selected = tokio::select! {
                _ = ctx.cancel.cancelled() => Err(json!({"source":"ide", "status":"cancelled"})),
                result = tokio::time::timeout_at(deadline, super::ide_bridge::resolve(
                    self.ide_binding.as_deref(), self.ide_discovery.as_deref(), &ctx.working_dir,
                    args.window_id.as_deref(), matches!(args.operation, Operation::Status), &ctx.cancel)) => result.unwrap_or_else(|_| Err(json!({"source":"ide", "status":"timeout", "message":"IDE discovery timed out; check the selected IDE window."}))),
            };
            let (binding, status) = match selected {
                Ok(selected) => selected,
                Err(status) => return ok(status.to_string()),
            };
            if matches!(args.operation, Operation::Status) {
                return ok(status.to_string());
            }
            let requested = resolve_path(&args.file_path, &ctx.working_dir);
            let request = super::ide_bridge::query(
                &binding,
                &ctx.working_dir,
                &requested,
                {
                    let mut wire =
                        serde_json::to_value(&args).expect("LSP arguments are serializable");
                    wire["window_id"] = status["window_id"].clone();
                    wire
                },
                timeout,
                &ctx.cancel,
            );
            let result = tokio::time::timeout_at(deadline, request).await.unwrap_or_else(|_| json!({"source":"ide", "status":"timeout", "message":"IDE discovery and query exceeded the shared deadline."}));
            return ok(result.to_string());
        }
        if !self.local_enabled {
            return ok(json!({"source":"local", "status":"unavailable", "message":"Local LSP is disabled. Enable lsp.enabled explicitly to start local servers."}).to_string());
        }
        if matches!(
            args.operation,
            Operation::DocumentSymbols | Operation::Inspect | Operation::Status
        ) {
            return ok(json!({"source":"local", "status":"unsupported", "message":"This operation requires an explicitly bound IDE backend."}).to_string());
        }
        if args.symbol.is_some() {
            return err(
                "lsp: symbol positions require the IDE backend; local LSP requires character",
            );
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
            Operation::DocumentSymbols | Operation::Inspect | Operation::Status => {
                unreachable!("IDE-only operations handled above")
            }
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
                    Operation::Diagnostics
                    | Operation::DocumentSymbols
                    | Operation::Inspect
                    | Operation::Status => {
                        unreachable!()
                    }
                };
                if ctx.cancel.is_cancelled() {
                    return ok("LSP query cancelled.");
                }
                degradable(result)
            }
            Operation::DocumentSymbols | Operation::Inspect | Operation::Status => {
                unreachable!("IDE-only operations handled above")
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

    // This exercises real filesystem I/O underneath the tool deadline.
    #[tokio::test(flavor = "current_thread")]
    async fn status_needs_no_source_file_and_does_not_start_local_servers() {
        let root = tempfile::tempdir().unwrap();
        let tool = unavailable_tool();
        let result = tool
            .execute(r#"{"operation":"status"}"#, &ctx(root.path()))
            .await;
        assert!(!result.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap()["status"],
            "not_enabled"
        );
        let tool = tool.with_ide_discovery(Some(root.path().join("missing")));
        let result = tool
            .execute(r#"{"operation":"status"}"#, &ctx(root.path()))
            .await;
        assert!(!result.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap()["status"],
            "not_enabled"
        );
        assert!(tool.parameters_schema()["properties"]["operation"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op == "status"));
        let result = tool
            .execute(
                r#"{"operation":"hover","symbol":"x","line":1}"#,
                &ctx(root.path()),
            )
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn ide_binding_does_not_enable_local_processes_or_expose_credentials() {
        let tool = unavailable_tool()
            .with_ide_binding(Some(PathBuf::from("/missing/connection.json")), false);
        let dir = tempfile::tempdir().unwrap();
        let result = tool.execute(r#"{"operation":"hover","backend":"local","file_path":"a.cj","line":1,"character":1}"#, &ctx(dir.path())).await;
        assert!(result.content.contains("Local LSP is disabled"));
        assert!(tool
            .parameters_schema()
            .to_string()
            .contains("document_symbols"));
        assert!(tool.parameters_schema().to_string().contains("inspect"));
        let result = tool
            .execute(
                r#"{"operation":"hover","file_path":"a.cj","line":1,"symbol":"x","character":1}"#,
                &ctx(dir.path()),
            )
            .await;
        assert!(result.is_error);
        assert!(!result.content.contains("connection.json"));
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
