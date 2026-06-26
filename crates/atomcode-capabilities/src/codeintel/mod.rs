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
use std::collections::HashMap;
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

/// A user-configured language server for one file extension (`[lsp.servers]`). The
/// neutral, feature-agnostic mirror of core's `config::LspServerConfig` — drivers map
/// their config type into this so L1 never depends on the driver's config crate.
#[derive(Debug, Clone)]
pub struct LspServerSetting {
    pub command: String,
    pub args: Vec<String>,
    pub root_markers: Vec<String>,
}

/// Driver-supplied LSP policy, threaded down to [`register_codeintel_tools`]. Neutral and
/// NOT behind the `lsp` feature (the registration signature is the same in both builds), so
/// the driver maps its `[lsp]` config into this once. Off by default — opt-in only, matching
/// the config schema and the production v1 `build_lsp_manager` gate.
#[derive(Debug, Clone)]
pub struct LspSettings {
    /// Master switch. When false, the `diagnostics` tool is NOT registered (so it never
    /// mounts and no language-server binary is ever spawned).
    pub enabled: bool,
    /// Seed the built-in server set (rust-analyzer / gopls / …). When false, only the
    /// explicit [`servers`](Self::servers) are known — the user opts in per language.
    pub auto_detect: bool,
    /// Custom / override servers, keyed by file extension. Merged over the (optional)
    /// defaults, so a user entry for an extension wins.
    pub servers: HashMap<String, LspServerSetting>,
    /// Settle delay (ms) before reading diagnostics after a document sync.
    pub settle_delay_ms: u64,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self { enabled: false, auto_detect: false, servers: HashMap::new(), settle_delay_ms: 350 }
    }
}

/// Build the language-server registry from [`LspSettings`] — defaults (when `auto_detect`)
/// with the user's explicit servers merged on top. Mirrors v1's `build_registry`.
#[cfg(feature = "lsp")]
fn build_lsp_registry(lsp: &LspSettings) -> lsp::LspServerRegistry {
    let mut registry = if lsp.auto_detect {
        lsp::LspServerRegistry::with_defaults()
    } else {
        lsp::LspServerRegistry::empty()
    };
    for (ext, s) in &lsp.servers {
        registry.insert(
            ext.clone(),
            lsp::LspServerConfig {
                command: s.command.clone(),
                args: s.args.clone(),
                root_markers: s.root_markers.clone(),
            },
        );
    }
    registry
}

/// Register all code-intelligence tools. The 5 graph tools SHARE one lazily-built
/// [`CodeIndex`]; the symbol tools and `find_references` are stateless. With the `lsp`
/// feature AND `lsp.enabled`, the `diagnostics` tool (sharing one [`LspManager`] built from
/// `lsp`) is also registered; when disabled it is left out entirely (the static
/// `codeintel_tool_names` still lists it, but [`ToolRegistry::mount`] silently skips an
/// unregistered name, so the model never sees it).
pub fn register_codeintel_tools(reg: &mut ToolRegistry, lsp: &LspSettings) {
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
    if lsp.enabled {
        let manager = LspManager::with_registry_and_delay(build_lsp_registry(lsp), lsp.settle_delay_ms);
        reg.register(Arc::new(DiagnosticsTool::new(Arc::new(manager))));
    }
    #[cfg(not(feature = "lsp"))]
    let _ = lsp; // settings are only consulted under the `lsp` feature
}

// Local path/result helpers (kept independent of the `tools` feature).
pub(crate) fn resolve_path(raw: &str, working_dir: &Path) -> PathBuf {
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
            comps[comps.len() - 3..].iter().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
        )
    }
}

pub(crate) fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: false }
}
pub(crate) fn err(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: true }
}

#[cfg(test)]
mod tool_name_tests {
    use super::codeintel_tool_names;

    #[cfg(feature = "lsp")]
    #[test]
    fn diagnostics_is_listed_when_lsp_enabled() {
        assert!(
            codeintel_tool_names().contains(&"diagnostics"),
            "diagnostics must be mounted when the lsp feature is on"
        );
    }
}

#[cfg(all(test, feature = "lsp"))]
mod lsp_settings_tests {
    use super::{build_lsp_registry, LspServerSetting, LspSettings};

    fn settings(enabled: bool, auto_detect: bool) -> LspSettings {
        LspSettings { enabled, auto_detect, servers: Default::default(), settle_delay_ms: 150 }
    }

    #[test]
    fn auto_detect_yields_builtin_defaults() {
        let reg = build_lsp_registry(&settings(true, true));
        assert_eq!(reg.get("rs").unwrap().command, "rust-analyzer");
        assert!(reg.get("go").is_some(), "auto-detect must include the built-in server set");
    }

    #[test]
    fn no_auto_detect_starts_empty() {
        let reg = build_lsp_registry(&settings(true, false));
        assert!(reg.get("rs").is_none(), "without auto-detect, defaults must NOT be present");
    }

    #[test]
    fn user_servers_merge_without_auto_detect() {
        let mut s = settings(true, false);
        s.servers.insert(
            "rb".into(),
            LspServerSetting { command: "solargraph".into(), args: vec!["stdio".into()], root_markers: vec![] },
        );
        let reg = build_lsp_registry(&s);
        assert!(reg.get("rs").is_none(), "defaults stay off");
        let rb = reg.get("rb").unwrap();
        assert_eq!(rb.command, "solargraph");
        assert_eq!(rb.args, vec!["stdio".to_string()]);
    }

    #[test]
    fn user_servers_override_defaults_under_auto_detect() {
        let mut s = settings(true, true);
        s.servers.insert(
            "rs".into(),
            LspServerSetting { command: "custom-ra".into(), args: vec![], root_markers: vec!["Cargo.toml".into()] },
        );
        let reg = build_lsp_registry(&s);
        assert_eq!(reg.get("rs").unwrap().command, "custom-ra", "user override must win over the default");
        assert!(reg.get("go").is_some(), "other defaults remain");
    }

    #[test]
    fn enabled_gates_diagnostics_registration() {
        use atomcode_kernel::tool::ToolRegistry;

        let mut off = ToolRegistry::new();
        super::register_codeintel_tools(&mut off, &settings(false, true));
        assert!(
            off.mount(&["diagnostics"]).get("diagnostics").is_none(),
            "disabled ⇒ diagnostics tool must NOT be registered"
        );
        // the non-LSP codeintel tools are always present, gate or not.
        assert!(off.mount(&["list_symbols"]).get("list_symbols").is_some());

        let mut on = ToolRegistry::new();
        super::register_codeintel_tools(&mut on, &settings(true, true));
        assert!(
            on.mount(&["diagnostics"]).get("diagnostics").is_some(),
            "enabled ⇒ diagnostics tool must be registered"
        );
    }
}
