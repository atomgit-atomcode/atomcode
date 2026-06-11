//! Neutral coding **tools** (L1): fs `read`/`write`/`edit`/`list` + `bash` +
//! `grep`/`glob`, plus a generic approval middleware. Each implements the kernel
//! [`Tool`](atomcode_kernel::tool::Tool) trait against the kernel's MINIMAL
//! [`ToolContext`](atomcode_kernel::tool::ToolContext) (`working_dir` + `cancel`) —
//! deliberately WITHOUT any coding enrichments (no semantic / graph / lsp /
//! file_store / read_cache / file_history / budgets). Those belong to a higher
//! `codeintel` (L1) / `coding` (L2) layer; the neutral fs/exec core lives here.
//!
//! # Trust model (inherited from the kernel)
//!
//! These tools run with the host process's FULL ambient authority — the kernel does
//! not sandbox them (see [`atomcode_kernel::tool`]). Relative paths resolve against
//! `ctx.working_dir`; absolute paths are honored as-is. There is deliberately NO
//! path-escape enforcement here: faking a sandbox at this layer would be FALSE
//! security. OS-level isolation (containers, seccomp, a restricted user) is the
//! EMBEDDER's responsibility.
//!
//! # Risk & approval
//!
//! Each tool declares an arg-aware [`risk`](atomcode_kernel::tool::Tool::risk):
//! read/list/grep/glob are always `Safe`; write/edit are always `Risky` (they mutate
//! the filesystem); `bash` is `Risky` only for commands its danger classifier flags.
//! Risk is advisory metadata — the GATE is the composable [`ApprovalMiddleware`],
//! which reads `risk`, consults an injected [`PermissionStore`], and otherwise
//! round-trips the driver for a decision.

use atomcode_kernel::tool::{ToolRegistry, ToolResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod approval;
pub mod ast_grep;
pub mod bash;
pub mod cd;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod list;
pub mod open_file;
pub mod parallel_edit;
pub mod read;
pub mod report_finding;
pub mod search_replace;
pub mod todo;
pub mod write;
/// Network tools (`web_fetch` / `web_search`). Opt-in `web` feature (HTTP stack).
#[cfg(feature = "web")]
pub mod web_fetch;
#[cfg(feature = "web")]
pub mod web_search;

pub use approval::{
    ApprovalMiddleware, ApprovalRequest, ApprovalResponse, InMemoryPermissionStore,
    PermissionDecision, PermissionStore, APPROVAL_KIND,
};
pub use ast_grep::AstGrepTool;
pub use bash::BashTool;
pub use cd::ChangeDirTool;
pub use edit::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list::ListDirTool;
pub use open_file::OpenFileTool;
pub use parallel_edit::ParallelEditTool;
pub use read::ReadFileTool;
pub use report_finding::{Finding, ReportFindingTool};
pub use search_replace::SearchReplaceTool;
pub use todo::TodoTool;
pub use write::WriteFileTool;
#[cfg(feature = "web")]
pub use web_fetch::WebFetchTool;
#[cfg(feature = "web")]
pub use web_search::WebSearchTool;

/// Names of the full neutral coding toolset — pass to
/// [`ToolRegistry::mount`](atomcode_kernel::tool::ToolRegistry::mount).
pub fn coding_tool_names() -> &'static [&'static str] {
    &["read_file", "write_file", "edit_file", "list_directory", "bash", "grep", "glob"]
}

/// Register the full neutral coding toolset into `reg` (then `mount` the subset a
/// given specialization should expose to the model).
pub fn register_coding_tools(reg: &mut ToolRegistry) {
    reg.register(Arc::new(ReadFileTool));
    reg.register(Arc::new(WriteFileTool));
    reg.register(Arc::new(EditFileTool));
    reg.register(Arc::new(ListDirTool));
    reg.register(Arc::new(BashTool));
    reg.register(Arc::new(GrepTool));
    reg.register(Arc::new(GlobTool));
}

/// Resolve a model-supplied path: absolute → as-is; relative → joined to
/// `working_dir`. NO escape enforcement (see the module trust-model note).
pub(crate) fn resolve_path(raw: &str, working_dir: &Path) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        working_dir.join(p)
    }
}

/// Directories never descended into during a walk (build artifacts / VCS / caches).
/// Mirrors the production walkers so a grep/glob/list does not drown in `target/`
/// or `node_modules/`.
pub(crate) const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".cache",
    "vendor",
    ".venv",
    "venv",
    ".idea",
    ".vscode",
    "datalog",
    "logs",
    "log",
    ".atomcode",
    ".claude",
    "runs",
];

/// Should a directory with this name be skipped during a walk?
pub(crate) fn is_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || name.starts_with(".venv-")
}

/// Heuristic binary sniff over the first 8 KiB: any NUL byte ⇒ binary (the `file(1)`
/// heuristic); otherwise >30% non-text control bytes ⇒ binary. The 30% threshold
/// tolerates UTF-8 multibyte text (CJK / emoji), which a byte-level scan would
/// otherwise misread as "control".
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let nonprint = sample.iter().filter(|&&b| b < 9 || (b > 13 && b < 32)).count();
    nonprint * 100 / sample.len() > 30
}

/// A successful tool result (`is_error: false`). `call_id` is filled by the kernel
/// after `execute` returns.
pub(crate) fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: false }
}
/// A failed tool result (`is_error: true`) — surfaced to the model so it can recover.
pub(crate) fn err(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: true }
}
