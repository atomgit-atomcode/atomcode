//! Code-intelligence capability (L1): tree-sitter symbol extraction + the
//! `list_symbols` / `read_symbol` tools. Sibling of `tools`/`provider` — a neutral
//! capability that depends only on the kernel.
//!
//! # Scope (first slice)
//!
//! Only the SINGLE-FILE symbol layer. Cross-file intelligence (find_references /
//! callers / callees / blast-radius) needs a repo-wide index, and diagnostics need an
//! external LSP — those are later, bigger batches. The symbol tools here are STATELESS:
//! each call parses one file on demand, so they need no shared index and nothing from
//! the kernel `ToolContext` beyond `working_dir`.
//!
//! 12 grammars (Rust / Python / JS / TS / TSX / Go / Java / C / C++ / C# / HTML / PHP),
//! behind the opt-in `codeintel` cargo feature (heavy C compilation).

use atomcode_kernel::tool::{ToolRegistry, ToolResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod lang;
pub mod list_symbols;
pub mod read_symbol;
pub mod symbols;

pub use lang::Lang;
pub use list_symbols::ListSymbolsTool;
pub use read_symbol::ReadSymbolTool;
pub use symbols::{extract_symbol, extract_symbols, Symbol};

/// Names of the code-intelligence tools — pass to
/// [`ToolRegistry::mount`](atomcode_kernel::tool::ToolRegistry::mount).
pub fn codeintel_tool_names() -> &'static [&'static str] {
    &["list_symbols", "read_symbol"]
}

/// Register the code-intelligence tools into `reg`.
pub fn register_codeintel_tools(reg: &mut ToolRegistry) {
    reg.register(Arc::new(ListSymbolsTool));
    reg.register(Arc::new(ReadSymbolTool));
}

// Local path/result helpers (duplicated tiny copies of the `tools` ones so codeintel
// does not couple to the `tools` feature). Same semantics: relative → working_dir,
// absolute → as-is, NO escape enforcement (kernel trust model).
pub(crate) fn resolve_path(raw: &str, working_dir: &Path) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        working_dir.join(p)
    }
}
pub(crate) fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: false }
}
pub(crate) fn err(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: true }
}
