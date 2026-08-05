//! `read_symbol` — read the full source of a named symbol (function/class/struct/…)
//! via tree-sitter, with line numbers. Non-destructive ⇒ always `Safe`.

use super::lang::Lang;
use super::symbols::extract_symbols;
use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

pub struct ReadSymbolTool;

#[derive(Deserialize)]
struct Args {
    file_path: String,
    symbol: String,
}

#[async_trait]
impl Tool for ReadSymbolTool {
    fn name(&self) -> &str {
        "read_symbol"
    }
    fn description(&self) -> &str {
        "Read the complete source of a single symbol (function / class / struct / method) \
         by name, with line numbers. More precise than read_file — returns exactly the \
         symbol. Use list_symbols first to discover names. Relative paths resolve against \
         the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to the source file (absolute, or relative to the working directory)" },
                "symbol": { "type": "string", "description": "Name of the symbol to read" }
            },
            "required": ["file_path", "symbol"]
        })
    }
    // read-only → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "read_symbol: invalid arguments: {e}. Expected {{\"file_path\":\"<path>\",\"symbol\":\"<name>\"}}."
                ))
            }
        };
        let path = resolve_path(&a.file_path, &ctx.working_dir);
        let display = a.file_path.clone();
        let symbol = a.symbol.clone();
        tokio::task::spawn_blocking(move || render(&path, &display, &symbol))
            .await
            .unwrap_or_else(|_| err("read_symbol: task failed"))
    }
}

fn render(path: &Path, display: &str, symbol: &str) -> ToolResult {
    let lang = match Lang::detect(path) {
        Some(l) => l,
        // NOT an error — same reasoning as `list_symbols`: no bundled grammar is a capability
        // boundary, not a failure. Point at the tool that does work.
        None => {
            return ok(format!(
                "no symbol index for {display} — no tree-sitter grammar is bundled for this \
                 file type. Read it with read_file instead."
            ))
        }
    };
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return err(format!("read_symbol: cannot read {}: {e}", path.display())),
    };
    let syms = match extract_symbols(&source, lang) {
        Some(s) => s,
        None => return err(format!("read_symbol: failed to parse {display}")),
    };
    match syms.iter().find(|s| s.name == symbol) {
        Some(s) => {
            let lines: Vec<&str> = source.lines().collect();
            let start = s.start_line.saturating_sub(1);
            let end = s.end_line.min(lines.len());
            let mut out = format!(
                "{} ({}) [lines {}-{}]:\n",
                s.name, s.kind, s.start_line, s.end_line
            );
            for (i, line) in lines[start..end.max(start)].iter().enumerate() {
                out.push_str(&format!("{:>4}| {}\n", s.start_line + i, line));
            }
            ok(out)
        }
        None => {
            let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
            err(format!(
                "read_symbol: symbol '{symbol}' not found in {display}.\nAvailable symbols: {}",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn reads_a_symbol_body_with_line_numbers() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("m.rs"),
            "fn alpha() {}\nfn beta(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .unwrap();
        let r = ReadSymbolTool
            .execute(r#"{"file_path":"m.rs","symbol":"beta"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("beta"), "{}", r.content);
        assert!(r.content.contains("   2| fn beta"), "{}", r.content);
        assert!(r.content.contains("   3|     x + 1"), "{}", r.content);
        assert!(
            !r.content.contains("alpha"),
            "should only return beta: {}",
            r.content
        );
    }

    /// 与 `list_symbols` 同款降级:没打包语法 ≠ 出错。见该模块对应测试的理由。
    #[tokio::test]
    async fn unsupported_extension_is_not_an_error_and_points_to_read_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.xyzlang"), "stuff").unwrap();
        let r = ReadSymbolTool
            .execute(
                r#"{"file_path":"a.xyzlang","symbol":"whatever"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("no symbol index"), "{}", r.content);
        assert!(r.content.contains("read_file"), "{}", r.content);
    }

    #[tokio::test]
    async fn missing_symbol_lists_available() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("m.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let r = ReadSymbolTool
            .execute(r#"{"file_path":"m.rs","symbol":"three"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("not found"), "{}", r.content);
        assert!(
            r.content.contains("one") && r.content.contains("two"),
            "{}",
            r.content
        );
    }
}
