//! `list_symbols` — outline a source file's symbols (name + kind + line range) via
//! tree-sitter. Non-destructive ⇒ always `Safe`.

use super::lang::Lang;
use super::symbols::extract_symbols;
use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

pub struct ListSymbolsTool;

#[derive(Deserialize)]
struct Args {
    file_path: String,
}

#[async_trait]
impl Tool for ListSymbolsTool {
    fn name(&self) -> &str {
        "list_symbols"
    }
    fn description(&self) -> &str {
        "List the functions, classes, structs, methods and other symbols defined in a \
         source file, each with its line range. Faster and more precise than read_file \
         for understanding a file's structure before editing. Supports Rust, Python, \
         JS/TS/TSX, Go, Java, C/C++, C#, HTML, PHP. Relative paths resolve against the \
         working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to the source file (absolute, or relative to the working directory)" }
            },
            "required": ["file_path"]
        })
    }
    // read-only → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "list_symbols: invalid arguments: {e}. Expected {{\"file_path\":\"<path>\"}}."
                ))
            }
        };
        let path = resolve_path(&a.file_path, &ctx.working_dir);
        let display = a.file_path.clone();
        // tree-sitter parsing is CPU-bound — keep it off the async runtime.
        tokio::task::spawn_blocking(move || render(&path, &display))
            .await
            .unwrap_or_else(|_| err("list_symbols: task failed"))
    }
}

fn render(path: &Path, display: &str) -> ToolResult {
    let lang = match Lang::detect(path) {
        Some(l) => l,
        None => {
            return err(format!(
                "list_symbols: unsupported file type: {display} (no tree-sitter grammar for it)"
            ))
        }
    };
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return err(format!("list_symbols: cannot read {}: {e}", path.display())),
    };
    match extract_symbols(&source, lang) {
        Some(syms) if syms.is_empty() => ok(format!("No symbols found in {display}")),
        Some(syms) => {
            let mut out = format!("Symbols in {display} ({} total):\n\n", syms.len());
            for s in &syms {
                // Both line numbers right-aligned (matches production) so the columns
                // stay aligned across rows for easy scanning.
                out.push_str(&format!(
                    "  {:>4}-{:>4}  {}  ({})\n",
                    s.start_line, s.end_line, s.name, s.kind
                ));
            }
            out.push_str("\n[Use read_symbol to read any symbol's full source.]");
            ok(out)
        }
        None => err(format!("list_symbols: failed to parse {display}")),
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
    async fn lists_rust_symbols() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("m.rs"),
            "struct S;\nfn alpha() {}\nfn beta() {}\n",
        )
        .unwrap();
        let r = ListSymbolsTool
            .execute(r#"{"file_path":"m.rs"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("alpha"), "{}", r.content);
        assert!(r.content.contains("beta"), "{}", r.content);
        assert!(r.content.contains("S"), "{}", r.content);
    }

    #[tokio::test]
    async fn unsupported_extension_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.xyzlang"), "stuff").unwrap();
        let r = ListSymbolsTool
            .execute(r#"{"file_path":"a.xyzlang"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("unsupported file type"), "{}", r.content);
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = ListSymbolsTool
            .execute(r#"{"file_path":"nope.rs"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("cannot read"), "{}", r.content);
    }
}
