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
pub mod repair;
pub mod report_finding;
pub mod search_replace;
pub mod sensitive_path;
pub mod todo;
pub mod write;
pub mod write_approval;
/// Network tools (`web_fetch` / `web_search`). Opt-in `web` feature (HTTP stack).
#[cfg(feature = "web")]
pub mod web_fetch;
#[cfg(feature = "web")]
pub mod web_search;
/// AtomGit REST tools (repo / pr / issue). Opt-in `atomgit` feature.
#[cfg(feature = "atomgit")]
pub mod atomgit;

pub use approval::{
    ApprovalMiddleware, ApprovalRequest, ApprovalResponse, InMemoryPermissionStore,
    PermissionDecision, PermissionStore, APPROVAL_KIND,
};
pub use repair::{repair_tool_args, RepairToolArgsMiddleware};
pub use ast_grep::AstGrepTool;
pub use bash::BashTool;
pub use cd::ChangeDirTool;
pub use edit::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list::ListDirTool;
pub use open_file::{OpenFileTool, OpenFileWorkspaceGate};
pub use parallel_edit::ParallelEditTool;
pub use read::ReadFileTool;
pub use report_finding::{Finding, ReportFindingTool};
pub use search_replace::SearchReplaceTool;
pub use sensitive_path::{path_is_sensitive, references_sensitive_path, SensitivePathGate};
pub use todo::TodoTool;
pub use write::WriteFileTool;
pub use write_approval::WriteApprovalGate;
#[cfg(feature = "web")]
pub use web_fetch::WebFetchTool;
#[cfg(feature = "web")]
pub use web_search::WebSearchTool;
#[cfg(feature = "atomgit")]
pub use atomgit::{atomgit_tool_names, register_atomgit_tools, AtomgitIssueTool, AtomgitPrTool, AtomgitRepoTool};

/// Names of the full neutral coding toolset — pass to
/// [`ToolRegistry::mount`](atomcode_kernel::tool::ToolRegistry::mount).
pub fn coding_tool_names() -> &'static [&'static str] {
    &["read_file", "write_file", "edit_file", "list_directory", "open_file", "bash", "grep", "glob", "search_replace", "ast_grep", "todo"]
}

/// Register the full neutral coding toolset into `reg` (then `mount` the subset a
/// given specialization should expose to the model). Vision support OFF — `read_file`
/// reports images as binary (use [`register_coding_tools_with_vision`] for a VL model).
pub fn register_coding_tools(reg: &mut ToolRegistry) {
    register_coding_tools_with_vision(reg, false);
}

/// Like [`register_coding_tools`], but `vision` gates whether `read_file` hands an
/// image file back to the model as an actual picture (a VISION model SEES it) instead
/// of the "binary, cannot display" text. The caller decides the flag from the model
/// (the coding layer uses `atomcode_core::provider::model_name_suggests_vision`, the
/// same detector as the user-paste path) — kept out of this crate so it stays core-free.
pub fn register_coding_tools_with_vision(reg: &mut ToolRegistry, vision: bool) {
    reg.register(Arc::new(ReadFileTool::new(vision)));
    reg.register(Arc::new(WriteFileTool));
    reg.register(Arc::new(EditFileTool));
    reg.register(Arc::new(ListDirTool));
    reg.register(Arc::new(OpenFileTool));
    reg.register(Arc::new(BashTool));
    reg.register(Arc::new(GrepTool));
    reg.register(Arc::new(GlobTool));
    reg.register(Arc::new(SearchReplaceTool));
    reg.register(Arc::new(AstGrepTool));
    reg.register(Arc::new(TodoTool::new()));
}

/// Apply `CREATE_NO_WINDOW` on Windows so a spawned child does not pop a console window;
/// no-op elsewhere. Critical in headless/daemon mode (e.g. the WeChat clawbot / OpenClaw
/// bridge): with no console to inherit, each `cmd.exe` spawn would otherwise allocate a
/// NEW console window — the "一对话桌面就闪" flash the user reported. Re-exported from the
/// crate-shared [`crate::process_utils`] so there is ONE implementation (this module's
/// local copy was deduped into that home, which also carries the `std` `_sync` variant).
pub(crate) use crate::process_utils::suppress_console_window;

/// Resolve a model-supplied path: leading `~`/`~/` → home dir; absolute → as-is;
/// relative → joined to `working_dir`. NO escape enforcement (see the module
/// trust-model note). `~` expansion (via the crate-shared [`crate::pathutil`], so
/// `tools` and `codeintel` agree) gives parity with the shell the `bash` tool relies
/// on — fixing `read_file("~/.atomcode/x")` resolving to the broken `<cwd>/~/…`.
pub(crate) fn resolve_path(raw: &str, working_dir: &Path) -> PathBuf {
    if let Some(home) = crate::pathutil::expand_tilde(raw) {
        return home;
    }
    if is_absolute_path(raw) {
        PathBuf::from(raw)
    } else {
        working_dir.join(raw)
    }
}

/// Coerce every line ending in `s` to `eol` (`"\n"` or `"\r\n"`): collapse any `\r\n`
/// to `\n`, then expand to the target. Idempotent for `"\n"`. Used by the editors so a
/// model that copied LF text from `read_file` (which strips `\r` via `str::lines()`) can
/// still match — and not corrupt — a CRLF file on disk.
pub(crate) fn coerce_eol(s: &str, eol: &str) -> String {
    if eol == "\r\n" {
        s.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        s.replace("\r\n", "\n")
    }
}

/// Windows-aware absolute-path test. `Path::is_absolute()` is **platform-dependent**:
/// on a Unix build it rejects `G:\foo` (treats the whole thing as one relative name),
/// so `working_dir.join("G:\\…")` silently produces garbage. A coding agent receives
/// paths for the USER's platform, which may differ from the build target (and tests
/// must be reproducible off Windows), so we additionally recognize Windows roots:
/// drive-letter (`C:\`, `C:/`) and UNC (`\\server\share`).
pub(crate) fn is_absolute_path(raw: &str) -> bool {
    if Path::new(raw).is_absolute() {
        return true;
    }
    let b = raw.as_bytes();
    // Drive-rooted: `X:\` or `X:/` (a bare `X:` is drive-RELATIVE, not absolute).
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return true;
    }
    // UNC: `\\server\share` (the `//…` form is already caught by is_absolute on Unix).
    b.len() >= 2 && b[0] == b'\\' && b[1] == b'\\'
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
    ToolResult { call_id: String::new(), content: content.into(), is_error: false, images: vec![] }
}
/// A successful tool result that also carries inline `images` for a VISION model to
/// SEE (e.g. `read_file` returning a picture). The agent loop lifts these onto a
/// follow-up `Role::User` message — the only role a provider serializes images on.
pub(crate) fn ok_with_images(
    content: impl Into<String>,
    images: Vec<atomcode_kernel::message::ImageContent>,
) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: false, images }
}
/// A failed tool result (`is_error: true`) — surfaced to the model so it can recover.
pub(crate) fn err(content: impl Into<String>) -> ToolResult {
    ToolResult { call_id: String::new(), content: content.into(), is_error: true, images: vec![] }
}

/// Max wall-clock a permission gate may spend on blocking filesystem classification
/// (path canonicalization). The workspace can sit on a stalled mount (e.g. a hung
/// network share) where `std::fs::canonicalize` blocks for minutes; bounding it keeps
/// the kernel's turn loop responsive (Esc/Ctrl-C stay live) instead of freezing — the
/// exact symptom of a `before()` gate hanging on `/Volumes/<share>`.
pub(crate) const GATE_FS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Run blocking `f` OFF the async worker (so a stalled syscall can't pin the runtime
/// thread mid-poll), bounded by `timeout`. Returns `default` if `f` doesn't finish in
/// time or its thread panics — a hung filesystem degrades to a safe fallback, never a
/// hang. The orphaned blocking thread is abandoned (it finishes when the syscall
/// eventually returns); acceptable for the rare stalled-mount case.
pub(crate) async fn run_bounded<T, F>(timeout: std::time::Duration, default: T, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(v)) => v,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolRegistry;

    #[tokio::test]
    async fn run_bounded_yields_default_when_blocking_exceeds_timeout() {
        // A stalled syscall (simulated by a long sleep on the blocking thread) must NOT
        // hang the caller: the bound fires and returns the safe default well before the
        // closure would finish.
        let got = run_bounded(std::time::Duration::from_millis(50), false, || {
            std::thread::sleep(std::time::Duration::from_millis(800));
            true
        })
        .await;
        assert!(!got, "exceeding the timeout must return the default, not block");
    }

    #[tokio::test]
    async fn run_bounded_returns_value_when_fast() {
        let got = run_bounded(std::time::Duration::from_secs(5), false, || true).await;
        assert!(got, "a fast closure returns its real value");
    }

    #[test]
    fn resolve_path_treats_windows_drive_and_unc_as_absolute() {
        let wd = Path::new("/work/proj");
        // A Windows drive path (either slash style) must NOT be joined onto the
        // working dir — doing so produces garbage like `/work/proj/G:\VR2024\…`
        // and makes the agent report an existing file as "does not exist".
        assert_eq!(resolve_path(r"G:\VR2024\keystore", wd), PathBuf::from(r"G:\VR2024\keystore"));
        assert_eq!(resolve_path("G:/VR2024/keystore", wd), PathBuf::from("G:/VR2024/keystore"));
        // UNC paths are absolute too.
        assert_eq!(resolve_path(r"\\server\share\f", wd), PathBuf::from(r"\\server\share\f"));
        // Plain relative paths still join onto the working dir.
        assert_eq!(resolve_path("src/main.rs", wd), PathBuf::from("/work/proj/src/main.rs"));
    }

    /// The canonical list of tool names `register_coding_tools` and
    /// `coding_tool_names` must agree on — the single source of truth for these
    /// tests. Adding/removing a tool updates only this list; the assertions below
    /// fail if the code doesn't match.
    const EXPECTED_TOOL_NAMES: &[&str] = &[
        "read_file",
        "write_file",
        "edit_file",
        "list_directory",
        "open_file",
        "bash",
        "grep",
        "glob",
        "search_replace",
        "ast_grep",
        "todo",
    ];

    #[test]
    fn coding_tool_names_matches_expected_list() {
        let mut names = coding_tool_names().to_vec();
        names.sort();
        let mut expected = EXPECTED_TOOL_NAMES.to_vec();
        expected.sort();
        assert_eq!(names, expected, "coding_tool_names() must match EXPECTED_TOOL_NAMES");
    }

    #[test]
    fn register_coding_tools_has_no_extra_or_missing_tools() {
        let mut reg = ToolRegistry::new();
        register_coding_tools(&mut reg);

        // Mount all names from the expected list.
        let mounted = reg.mount(EXPECTED_TOOL_NAMES);
        for name in EXPECTED_TOOL_NAMES {
            assert!(
                mounted.get(name).is_some(),
                "registered tool '{name}' must be mountable by name"
            );
            // Each mounted tool's name() must match the key it was registered under.
            let tool = mounted.get(name).unwrap();
            assert_eq!(
                tool.name(),
                *name,
                "tool.name() must match the registration key '{name}'"
            );
        }
        // The mount must have resolved exactly the expected number of tools.
        assert_eq!(
            mounted.defs().len(),
            EXPECTED_TOOL_NAMES.len(),
            "mount must resolve all expected tools"
        );
    }

    #[test]
    fn register_coding_tools_all_tools_have_valid_defs() {
        let mut reg = ToolRegistry::new();
        register_coding_tools(&mut reg);
        let mounted = reg.mount(EXPECTED_TOOL_NAMES);

        for def in mounted.defs() {
            assert!(!def.name.is_empty(), "tool name must not be empty");
            assert!(!def.description.is_empty(), "tool '{}' must have a description", def.name);
            assert!(
                def.parameters.get("type").and_then(|v| v.as_str()) == Some("object"),
                "tool '{}' parameters must be a JSON object with type=object",
                def.name
            );
        }
    }

    #[test]
    fn unmounted_tools_are_not_resolvable() {
        let mut reg = ToolRegistry::new();
        register_coding_tools(&mut reg);

        // Mount only a subset; tools not in this list must not resolve.
        let subset = &["read_file", "bash", "grep"];
        let mounted = reg.mount(subset);
        assert!(mounted.get("read_file").is_some());
        assert!(mounted.get("bash").is_some());
        assert!(mounted.get("grep").is_some());
        // An unmounted tool must not be resolvable.
        assert!(mounted.get("write_file").is_none(), "unmounted tool must not resolve");
        assert!(mounted.get("edit_file").is_none(), "unmounted tool must not resolve");
        assert!(mounted.get("open_file").is_none(), "unmounted tool must not resolve");
    }

    /// A `/model` swap re-registers `read_file` (see `coding::parts::assemble`) to refresh
    /// its vision flag. This guards the mechanism that fix relies on: re-registering with a
    /// new `vision` value OVERWRITES the prior `read_file`, so a model swap from text→vision
    /// (or vision→text) actually changes how it treats an image — it does not go stale.
    #[tokio::test]
    async fn re_registering_read_file_overwrites_its_vision_flag() {
        use atomcode_kernel::tool::{ToolContext, ProgressSink};
        let d = tempfile::tempdir().unwrap();
        // JPEG-ish blob with a NUL so `looks_binary` flags it.
        std::fs::write(d.path().join("c.jpg"), [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00]).unwrap();
        let ctx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
        };

        // First mount: text-only model → read of an image stays the binary-text dead-end.
        let mut reg = ToolRegistry::new();
        register_coding_tools_with_vision(&mut reg, false);
        let r = reg.mount(&["read_file"]).get("read_file").unwrap()
            .execute(r#"{"file_path":"c.jpg"}"#, &ctx).await;
        assert!(r.images.is_empty() && r.content.starts_with("Binary file"), "{}", r.content);

        // Re-register on the SAME registry as if the model swapped to a VL model → the read
        // tool must now hand over the image, proving the swap takes effect (no stale flag).
        register_coding_tools_with_vision(&mut reg, true);
        let r = reg.mount(&["read_file"]).get("read_file").unwrap()
            .execute(r#"{"file_path":"c.jpg"}"#, &ctx).await;
        assert_eq!(r.images.len(), 1, "after re-register with vision, image must be attached: {}", r.content);
    }
}
