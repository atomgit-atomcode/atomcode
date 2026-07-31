//! Code-intelligence capability (L1): tree-sitter symbol extraction + a cross-file
//! code graph, exposed as read-only tools. Sibling of `tools`/`provider` — depends only
//! on the kernel + tree-sitter/ignore.
//!
//! # Layers
//!
//! - **symbol layer** (single-file, STATELESS): `list_symbols` / `read_symbol` parse one
//!   file on demand — no shared state, nothing from the kernel `ToolContext` beyond
//!   `working_dir`.
//! - **graph layer** (cross-file): `find_references` (whole-word text scan) plus
//!   `trace_callers` / `trace_callees` / `trace_chain` / `blast_radius` /
//!   `file_dependencies`, backed by a shared, lazily-built [`CodeIndex`] (the symbol
//!   layer's statelessness ends here — these tools HOLD an `Arc<CodeIndex>`).
//!
//! Deferred vs production: LSP diagnostics; visibility inference; import-aware call
//! resolution; background/incremental indexing (we rebuild on mtime change). Behind the
//! opt-in `codeintel` cargo feature (12 grammars = heavy C compilation).

use atomcode_kernel::tool::{ToolRegistry, ToolResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod blast_radius;
pub mod file_deps;
pub mod find_references;
pub mod graph;
pub mod index;
pub mod lang;
pub mod list_symbols;
pub mod read_symbol;
pub mod symbols;
pub mod trace_callees;
pub mod trace_callers;
pub mod trace_chain;

/// LSP diagnostics (spawns external language servers). Opt-in `lsp` feature.
#[cfg(feature = "lsp")]
pub mod diagnostics;
#[cfg(feature = "lsp")]
pub mod lsp;

pub use blast_radius::BlastRadiusTool;
pub use file_deps::FileDependenciesTool;
pub use find_references::FindReferencesTool;
pub use graph::{CodeGraph, Edge, EdgeKind, SymbolId, SymbolKind, SymbolNode, Visibility};
pub use index::{build_graph, CodeIndex};
pub use lang::Lang;
pub use list_symbols::ListSymbolsTool;
pub use read_symbol::ReadSymbolTool;
pub use symbols::{extract_symbol, extract_symbols, skeleton, Symbol};
pub use trace_callees::TraceCalleesTool;
pub use trace_callers::TraceCallersTool;
pub use trace_chain::TraceChainTool;

#[cfg(feature = "lsp")]
pub use diagnostics::DiagnosticsTool;
#[cfg(feature = "lsp")]
pub use lsp::LspManager;

/// Names of the code-intelligence tools — pass to
/// [`ToolRegistry::mount`](atomcode_kernel::tool::ToolRegistry::mount). Includes
/// `diagnostics` only when the `lsp` feature is enabled.
#[cfg(feature = "lsp")]
pub fn codeintel_tool_names() -> &'static [&'static str] {
    &[
        "list_symbols",
        "read_symbol",
        "find_references",
        "trace_callers",
        "trace_callees",
        "trace_chain",
        "blast_radius",
        "file_dependencies",
        "diagnostics",
    ]
}
#[cfg(not(feature = "lsp"))]
pub fn codeintel_tool_names() -> &'static [&'static str] {
    &[
        "list_symbols",
        "read_symbol",
        "find_references",
        "trace_callers",
        "trace_callees",
        "trace_chain",
        "blast_radius",
        "file_dependencies",
    ]
}

/// Register all code-intelligence tools. The 5 graph tools SHARE one lazily-built
/// [`CodeIndex`]; the symbol tools and `find_references` are stateless. With the `lsp`
/// feature, the `diagnostics` tool (sharing one [`LspManager`]) is also registered.
pub fn register_codeintel_tools(reg: &mut ToolRegistry) {
    reg.register(Arc::new(ListSymbolsTool));
    reg.register(Arc::new(ReadSymbolTool));
    reg.register(Arc::new(FindReferencesTool));
    let index = Arc::new(CodeIndex::new());
    reg.register(Arc::new(TraceCallersTool::new(index.clone())));
    reg.register(Arc::new(TraceCalleesTool::new(index.clone())));
    reg.register(Arc::new(TraceChainTool::new(index.clone())));
    reg.register(Arc::new(BlastRadiusTool::new(index.clone())));
    reg.register(Arc::new(FileDependenciesTool::new(index)));
    #[cfg(feature = "lsp")]
    reg.register(Arc::new(DiagnosticsTool::new(Arc::new(LspManager::new()))));
}

// Local path/result helpers (kept independent of the `tools` feature). Leading-`~`
// expansion routes through the crate-shared `pathutil` so codeintel tools
// (`read_symbol`, `blast_radius`, …) resolve `~/x` the SAME as `read_file`/`bash`.
pub(crate) fn resolve_path(raw: &str, working_dir: &Path) -> PathBuf {
    if let Some(home) = crate::pathutil::expand_tilde(raw) {
        return home;
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        working_dir.join(p)
    }
}

/// Canonicalize a path (resolve symlinks / `.`/`..`), falling back to the original on
/// error. The graph build AND the tool lookups both canonicalize, so a file referenced
/// via a different alias (e.g. macOS `/var` vs `/private/var`) still matches the graph's
/// stored paths instead of a false "not found".
pub(crate) fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Display a path relative to `root` when possible, else shortened to `.../last3`.
pub(crate) fn display_path(p: &Path, root: &Path) -> String {
    if let Ok(rel) = p.strip_prefix(root) {
        return rel.display().to_string();
    }
    let comps: Vec<_> = p.components().collect();
    if comps.len() <= 3 {
        p.display().to_string()
    } else {
        format!(
            ".../{}",
            comps[comps.len() - 3..]
                .iter()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        )
    }
}

pub(crate) fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        images: vec![],
    }
}
pub(crate) fn err(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: true,
        images: vec![],
    }
}
