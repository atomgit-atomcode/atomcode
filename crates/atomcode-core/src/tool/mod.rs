pub mod auto_fix;
pub mod bash;
pub mod blast_radius;
pub mod cd;
pub mod edit;
pub mod file_deps;
pub mod file_history;
pub mod find_references;
pub mod glob;
pub mod grep;
pub mod list_dir;
pub mod list_symbols;
pub mod read;
pub mod read_symbol;
pub mod result_store;
pub mod search_replace;
pub mod trace_callees;
pub mod trace_callers;
pub mod trace_chain;
pub mod use_skill;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// Directories to skip when scanning file trees (build artifacts, caches, VCS).
/// Used by glob, list_dir, and collect_project_files.
pub const SKIP_DIRS: &[&str] = &[
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
    ".DS_Store",
    ".env",
    "datalog",
    "logs",
    "log",
    ".atomcode",
    ".claude",
    "runs",
];

/// Prefixes — any directory whose name starts with one of these is skipped.
/// Covers `.venv-*` variants (`.venv-test`, `.venv-swebench`, etc.).
pub const SKIP_DIR_PREFIXES: &[&str] = &[".venv-"];

/// Check if a directory name should be skipped (exact match OR prefix match).
/// Use this instead of `SKIP_DIRS.contains()` for complete coverage.
pub fn should_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || SKIP_DIR_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Lightweight sensitive-path precheck for raw tool arguments before a
/// workspace-aware approval pass is available.
pub(crate) fn is_sensitive_input_path(path: &str) -> bool {
    let base_dir = std::env::current_dir().ok();
    let home_dir = dirs::home_dir();
    is_sensitive_input_path_with_context(path, base_dir.as_deref(), home_dir.as_deref())
}

fn is_sensitive_input_path_with_context(
    path: &str,
    base_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> bool {
    if is_windows_sensitive_path(path) {
        return true;
    }

    let mut expanded = expand_home_path(path, home_dir);
    if !expanded.is_absolute() {
        if let Some(base_dir) = base_dir {
            expanded = base_dir.join(expanded);
        }
    }

    let normalized = lexical_normalize(&expanded);
    if is_windows_sensitive_path(&normalized.to_string_lossy()) {
        return true;
    }

    is_sensitive_path(&normalized)
}

fn expand_home_path(path: &str, home_dir: Option<&Path>) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home_dir) = home_dir {
            return home_dir.join(stripped);
        }
    }

    if path == "~" {
        if let Some(home_dir) = home_dir {
            return home_dir.to_path_buf();
        }
    }

    PathBuf::from(path)
}

fn expand_user_path(path: &str) -> PathBuf {
    let home_dir = dirs::home_dir();
    expand_home_path(path, home_dir.as_deref())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut prefix: Option<OsString> = None;
    let mut has_root = false;
    let mut parts: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix_component) => {
                prefix = Some(prefix_component.as_os_str().to_os_string());
                parts.clear();
            }
            Component::RootDir => {
                has_root = true;
                parts.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|part| part != OsStr::new("..")) {
                    parts.pop();
                } else if !has_root {
                    parts.push(OsString::from(".."));
                }
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }

    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    if has_root {
        normalized.push(std::path::MAIN_SEPARATOR.to_string());
    }
    for part in parts {
        normalized.push(part);
    }
    normalized
}

fn is_windows_sensitive_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\");
    let normalized = normalized.strip_prefix(r"\\?\").unwrap_or(&normalized);
    let lowercase = normalized.to_ascii_lowercase();
    let sensitive_roots = [
        r"\windows",
        r"\program files",
        r"\program files (x86)",
        r"\programdata",
    ];
    let Some(path_without_drive) = strip_windows_drive_prefix(&lowercase) else {
        return false;
    };

    sensitive_roots
        .iter()
        .any(|root| windows_path_starts_with(path_without_drive, root))
}

fn windows_path_starts_with(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn strip_windows_drive_prefix(path: &str) -> Option<&str> {
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
    {
        return None;
    }

    Some(&path[2..])
}

/// Count of leading characters shared between two paths. Used by read_file
/// and glob 404 recovery to rank candidate suggestions.
pub fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock};

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = normalized
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if can_pop {
                    normalized.pop();
                } else if normalized.as_os_str().is_empty() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

fn canonicalize_candidate_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("Failed to resolve path {}", path.display()));
    }

    let mut missing_parts = Vec::new();
    let mut current = path;

    loop {
        if current.exists() {
            let mut resolved = std::fs::canonicalize(current)
                .with_context(|| format!("Failed to resolve parent path {}", current.display()))?;
            for part in missing_parts.iter().rev() {
                resolved.push(part);
            }
            return Ok(resolved);
        }

        let name = current.file_name().ok_or_else(|| {
            anyhow::anyhow!("Path {} has no existing parent directory", path.display())
        })?;
        missing_parts.push(name.to_os_string());
        current = current.parent().ok_or_else(|| {
            anyhow::anyhow!("Path {} has no existing parent directory", path.display())
        })?;
    }
}

pub struct ResolvedPath {
    pub path: PathBuf,
    pub workspace_root: PathBuf,
    pub within_workspace: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalPathAction {
    Enumerate,
    Read,
    Write,
}

pub fn inspect_path_access(raw_path: &str, working_dir: &Path) -> Result<ResolvedPath> {
    let workspace_root = std::fs::canonicalize(working_dir).with_context(|| {
        format!(
            "Failed to resolve working directory {}",
            working_dir.display()
        )
    })?;
    let expanded = expand_user_path(raw_path);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        working_dir.join(expanded)
    };
    let candidate = normalize_path(&candidate);
    let resolved = canonicalize_candidate_path(&candidate)?;

    Ok(ResolvedPath {
        within_workspace: resolved.starts_with(&workspace_root),
        path: resolved,
        workspace_root,
    })
}

pub fn resolve_workspace_path(raw_path: &str, working_dir: &Path) -> Result<PathBuf> {
    let resolved = inspect_path_access(raw_path, working_dir)?;
    if resolved.within_workspace {
        Ok(resolved.path)
    } else {
        bail!(
            "Access denied: {} resolves outside working directory {}",
            raw_path,
            resolved.workspace_root.display()
        );
    }
}

fn is_sensitive_path(path: &Path) -> bool {
    const SYSTEM_PROTECTED_PREFIXES: &[&str] = &[
        "/System",
        "/bin",
        "/sbin",
        "/usr",
        "/var",
        "/private/etc",
        "/private/var",
        "/etc",
        "/root",
        "/var/root",
        "/private/var/root",
    ];
    const SYSTEM_PROTECTED_EXCEPTIONS: &[&str] = &[
        "/usr/local",
        "/private/usr/local",
        "/Applications",
        "/Library",
        "/var/folders",
        "/private/var/folders",
        "/var/tmp",
        "/private/var/tmp",
    ];
    const SECRET_HOME_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".config"];
    const SECRET_FILE_NAMES: &[&str] = &[
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".zprofile",
        ".zshenv",
        ".npmrc",
        ".pypirc",
        ".env",
        ".env.local",
        "credentials",
        "config",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ];
    const SECRET_EXTS: &[&str] = &["pem", "key", "p12", "pfx", "der", "crt", "cer"];

    let has_protected_prefix = SYSTEM_PROTECTED_PREFIXES
        .iter()
        .any(|prefix| path == Path::new(prefix) || path.starts_with(prefix));
    let has_exception_prefix = SYSTEM_PROTECTED_EXCEPTIONS
        .iter()
        .any(|prefix| path == Path::new(prefix) || path.starts_with(prefix));

    if has_protected_prefix && !has_exception_prefix {
        return true;
    }

    if let Some(home) = dirs::home_dir() {
        for dir in SECRET_HOME_DIRS {
            if path.starts_with(home.join(dir)) {
                return true;
            }
        }

        for file in SECRET_FILE_NAMES {
            if path == home.join(file) {
                return true;
            }
        }
    }

    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| SECRET_FILE_NAMES.contains(&name))
    {
        return true;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            SECRET_EXTS
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
}

pub fn approval_for_path(
    raw_path: &str,
    working_dir: &Path,
    action: ExternalPathAction,
) -> Result<ApprovalRequirement> {
    let access = inspect_path_access(raw_path, working_dir)?;
    if access.within_workspace {
        return Ok(ApprovalRequirement::AutoApprove);
    }

    let sensitive = is_sensitive_path(&access.path);
    let action_label = match action {
        ExternalPathAction::Enumerate => "Accessing",
        ExternalPathAction::Read => "Reading",
        ExternalPathAction::Write => "Writing",
    };
    let base_reason = format!(
        "{} path outside working directory: {} (working dir: {})",
        action_label,
        raw_path,
        access.workspace_root.display()
    );

    Ok(match action {
        ExternalPathAction::Enumerate => {
            if sensitive {
                ApprovalRequirement::RequireApprovalAlways(format!(
                    "{}. This path looks sensitive and always requires confirmation.",
                    base_reason
                ))
            } else {
                ApprovalRequirement::AutoApprove
            }
        }
        ExternalPathAction::Read => {
            if sensitive {
                ApprovalRequirement::RequireApprovalAlways(format!(
                    "{}. This path looks sensitive and always requires confirmation.",
                    base_reason
                ))
            } else {
                ApprovalRequirement::RequireApproval(format!("{base_reason}."))
            }
        }
        ExternalPathAction::Write => ApprovalRequirement::RequireApprovalAlways(format!(
            "{}. Writing outside the workspace always requires confirmation.",
            base_reason
        )),
    })
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub struct ToolCallBuffer {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// True once we've extracted and sent a path hint — avoids resending on every delta.
    pub hint_sent: bool,
}

pub enum ApprovalRequirement {
    AutoApprove,
    RequireApproval(String),
    RequireApprovalAlways(String),
}

/// Coarse-grained permission level for a tool, stored in `PermissionStore`.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionLevel {
    /// Never ask — always execute automatically.
    AlwaysAllow,
    /// Ask every time (default for destructive operations).
    Ask,
    /// Allowed for the duration of the current session.
    SessionAllow,
    /// Never execute.
    AlwaysDeny,
}

/// The resolved decision returned by `PermissionStore::check`.
#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Allow,
    /// Ask the user — carries the reason string from `ApprovalRequirement`.
    Ask(String),
    Deny,
}

/// Stores per-tool permission overrides and session-level grants.
pub struct PermissionStore {
    /// Per-tool level overrides: tool_name → level.
    overrides: HashMap<String, PermissionLevel>,
    /// Session-level grants: tool names approved with [A]lways for this session.
    session_grants: HashSet<String>,
}

impl PermissionStore {
    pub fn new() -> Self {
        Self {
            overrides: HashMap::new(),
            session_grants: HashSet::new(),
        }
    }

    /// Check whether a tool call should be auto-approved, needs asking, or denied.
    pub fn check(&self, tool_name: &str, approval: &ApprovalRequirement) -> PermissionDecision {
        if let ApprovalRequirement::RequireApprovalAlways(reason) = approval {
            return PermissionDecision::Ask(reason.clone());
        }

        // 1. Session grant (user pressed [A] during this session).
        //    This overrides RequireApproval — the user explicitly chose "Always"
        //    for this tool, so don't prompt again. Bash still has its own
        //    destructive-command detection as a separate safety layer.
        if self.session_grants.contains(tool_name) {
            return PermissionDecision::Allow;
        }

        // 2. Destructive commands (RequireApproval) prompt unless session-granted.
        if let ApprovalRequirement::RequireApproval(reason) = approval {
            return PermissionDecision::Ask(reason.clone());
        }
        // 3. Explicit per-tool override (only reached for AutoApprove tools).
        if let Some(level) = self.overrides.get(tool_name) {
            match level {
                PermissionLevel::AlwaysAllow | PermissionLevel::SessionAllow => {
                    return PermissionDecision::Allow;
                }
                PermissionLevel::AlwaysDeny => return PermissionDecision::Deny,
                PermissionLevel::Ask => {} // fall through to normal logic
            }
        }

        // 4. Defer to the tool's own approval requirement.
        PermissionDecision::Allow
    }

    /// Grant session-level permission for a tool (user pressed [A]).
    pub fn grant_session(&mut self, tool_name: &str) {
        self.session_grants.insert(tool_name.to_string());
    }

    /// Set an explicit override level for a tool.
    pub fn set_override(&mut self, tool_name: &str, level: PermissionLevel) {
        self.overrides.insert(tool_name.to_string(), level);
    }
}

/// Shared execution context passed to every tool invocation.
/// Read cache key: (canonical path, offset, limit). offset/limit are the raw
/// args the model sent — different slicing windows cache separately.
pub type ReadCacheKey = (PathBuf, Option<usize>, Option<usize>);

/// Read cache entry: (file mtime at cache time, rendered tool output).
/// mtime acts as the invalidation signal — if disk mtime differs on next read,
/// the cache is stale regardless of other state (edit/write tools change mtime).
pub type ReadCacheEntry = (std::time::SystemTime, String);

/// Holds a shared working directory that tools can read (and `CdTool` can write).
#[derive(Clone)]
pub struct ToolContext {
    pub working_dir: Arc<RwLock<PathBuf>>,
    pub semantic: Arc<Mutex<crate::semantic::SemanticSearcher>>,
    pub file_history: Arc<Mutex<file_history::FileHistory>>,
    pub graph: Arc<RwLock<crate::graph::CodeGraph>>,
    /// Remaining context tokens budget. Set by TurnRunner before each tool batch.
    /// read_file uses this to decide full content vs skeleton.
    pub ctx_budget_hint: Arc<std::sync::atomic::AtomicUsize>,
    /// Per-file token budget for read_file. Set by runner.rs Layer B before each
    /// tool batch: `ctx_budget / (5 * num_reads)`. read.rs compares file_tokens
    /// against this to decide full vs skeleton. Defaults to ctx_budget/5 (single file).
    pub read_budget_tokens: Arc<std::sync::atomic::AtomicUsize>,
    /// Per-session read-file output cache. Hit is valid only when on-disk mtime
    /// still matches. Avoids redoing UTF-8 parsing + semantic skeleton generation
    /// when the model re-reads the same file — these are CPU-heavy, not just I/O.
    pub read_cache: Arc<RwLock<std::collections::HashMap<ReadCacheKey, ReadCacheEntry>>>,
    /// Top-5 most-distinctive lines captured from the first failed bash call
    /// this session. Used for effect-based "error resolved" detection (P0 #5):
    /// when a later bash succeeds and ≥3 of these 5 lines no longer appear,
    /// the framework appends a hint nudging the model to summarize + stop.
    ///
    /// Why 5 lines with a majority threshold instead of 1 line (initial
    /// design from 2026-04-22 morning): cargo / npm / pytest output
    /// interleaves real diagnostics with ambient status (`Blocking waiting
    /// for file lock`, `Checking crate v0.1.0`). A single-line signature
    /// routinely caught a status line that appears on success too, so the
    /// nudge never fired. Multi-line + majority absent is robust to noise
    /// overlap without per-tool pattern matching.
    ///
    /// Stays set once captured — "original failure" anchor, not rolling.
    pub first_error_signatures: Arc<RwLock<Vec<String>>>,
    /// Shared telemetry handle. Always present (possibly in disabled state).
    pub telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
}

impl ToolContext {
    /// Create a `ToolContext` with a disabled (no-op) telemetry handle.
    /// Prefer `with_telemetry` in production so real events are emitted.
    pub fn new(working_dir: PathBuf) -> Self {
        let telemetry = disabled_telemetry();
        Self::with_telemetry(working_dir, "default", telemetry)
    }

    pub fn with_session(working_dir: PathBuf, session_id: &str) -> Self {
        let telemetry = disabled_telemetry();
        Self::with_telemetry(working_dir, session_id, telemetry)
    }

    pub fn with_telemetry(
        working_dir: PathBuf,
        session_id: &str,
        telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
    ) -> Self {
        Self {
            working_dir: Arc::new(RwLock::new(working_dir)),
            semantic: Arc::new(Mutex::new(crate::semantic::SemanticSearcher::new())),
            file_history: Arc::new(Mutex::new(file_history::FileHistory::new(session_id))),
            ctx_budget_hint: Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
            read_budget_tokens: Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
            graph: Arc::new(RwLock::new(crate::graph::CodeGraph::new())),
            read_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
            first_error_signatures: Arc::new(RwLock::new(Vec::new())),
            telemetry,
        }
    }

    /// Create an isolated copy: same working directory value, independent Arc.
    /// Shares the same graph (read-only for tools) but independent working_dir.
    pub async fn isolate(&self) -> Self {
        let wd = self.working_dir.read().await.clone();
        let mut ctx = Self::new(wd);
        ctx.graph = self.graph.clone();
        ctx.telemetry = self.telemetry.clone();
        ctx
    }
}

/// Build a disabled (no-op) `Telemetry` handle — zero overhead, no I/O.
/// Used by `ToolContext::new` and in tests that don't care about telemetry.
fn disabled_telemetry() -> std::sync::Arc<atomcode_telemetry::Telemetry> {
    let cfg = atomcode_telemetry::ResolvedConfig {
        state: atomcode_telemetry::TelemetryState::Disabled("default"),
        endpoint: "http://localhost/v1/events".into(),
        atomcode_dir: std::path::PathBuf::from("/tmp"),
    };
    atomcode_telemetry::Telemetry::init(cfg, env!("CARGO_PKG_VERSION").into())
}

/// Extract up to 5 distinctive diagnostic lines from a failed bash/tool
/// output for use as a multi-signature "error anchor" (P0 #5).
/// Selection rule: longest lines first. Rationale — status noise
/// (`Checking v0.1.0 (/path)`, `Blocking waiting for file lock`) is almost
/// always shorter than real diagnostic content (`error[E0425]: cannot find
/// function \`foo\` in this scope`, full compiler traces). Sorting by length
/// pushes ambient status to the back of the queue without hardcoding tool
/// names.
///
/// Tech-neutral: no keyword matching on "error"/"failed"/"panic" etc. The
/// caller uses majority-absent semantics (≥3 of 5 disappear on success → fire
/// nudge) so lingering overlap on one or two status lines doesn't suppress
/// the detection.
pub fn extract_error_signatures(output: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Framework markers all start with `[` — elapsed, cwd, workspace
        // note, blocked messages. Skip them.
        if trimmed.starts_with('[') {
            continue;
        }
        if trimmed == "STDERR:" {
            continue;
        }
        if trimmed.len() < 15 {
            continue;
        }
        let s: String = trimmed.chars().take(120).collect();
        if !lines.contains(&s) {
            lines.push(s);
        }
    }
    // Sort by length desc — longer lines are more likely to be specific
    // diagnostic content (includes identifiers, paths, span markers).
    lines.sort_by_key(|s| std::cmp::Reverse(s.len()));
    lines.into_iter().take(5).collect()
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDef;
    fn approval(&self, args: &str) -> ApprovalRequirement;
    fn approval_with_context(&self, args: &str, _ctx: &ToolContext) -> ApprovalRequirement {
        self.approval(args)
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    // BTreeMap ensures stable iteration order (sorted by name),
    // which keeps tool definitions in a consistent order across turns.
    // This is important for OpenAI/DeepSeek auto prefix caching.
    // RwLock allows async registration from MCP connection events.
    tools: tokio::sync::RwLock<BTreeMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: tokio::sync::RwLock::new(BTreeMap::new()),
        }
    }

    /// Register a tool (async, acquires write lock).
    pub async fn register(&self, tool: Box<dyn Tool>) {
        let name = tool.definition().name.to_string();
        let mut tools = self.tools.write().await;
        tools.insert(name, Arc::from(tool));
    }

    /// Register a tool synchronously (for use during startup when we have exclusive access).
    /// This bypasses the RwLock by using `get_mut()` which requires `&mut self`.
    pub fn register_sync(&mut self, tool: Box<dyn Tool>) {
        let name = tool.definition().name.to_string();
        self.tools.get_mut().insert(name, Arc::from(tool));
    }

    /// Get all tool definitions (async, acquires read lock).
    pub async fn get_definitions(&self) -> Vec<ToolDef> {
        let tools = self.tools.read().await;
        tools.values().map(|t| t.definition()).collect()
    }

    /// Get a tool by name (async, acquires read lock).
    pub async fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let tools = self.tools.read().await;
        tools.get(name).cloned()
    }

    /// Iterate over all registered tools (async, acquires read lock).
    pub async fn iter(&self) -> impl Iterator<Item = (String, Arc<dyn Tool>)> {
        let tools = self.tools.read().await;
        tools.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>().into_iter()
    }

    /// Register a tool from an Arc (for building filtered registries from parent).
    pub async fn register_arc(&self, name: String, tool: Arc<dyn Tool>) {
        let mut tools = self.tools.write().await;
        tools.insert(name, tool);
    }

    /// Unregister all tools whose names start with `prefix`.
    ///
    /// Used by `/mcp reload` to drop all previously registered MCP tools
    /// (`mcp__{server}__{tool}`) before reconnecting/re-registering.
    pub async fn unregister_prefix(&self, prefix: &str) -> usize {
        let mut tools = self.tools.write().await;
        let to_remove: Vec<String> = tools
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        let n = to_remove.len();
        for k in to_remove {
            tools.remove(&k);
        }
        n
    }
}

/// Defensive unwrap for a known model-output quirk: deepseek-v4-flash and
/// some qwen variants occasionally wrap tool arguments in an extra
/// `{"arguments": {...}}` envelope, even though the OpenAI tool-call
/// protocol's `function.arguments` field is already supposed to carry the
/// flat schema-shaped object directly. When that happens, every tool
/// dispatch fails with `missing field 'X'` and the model loops on the same
/// bad payload until our identical-args guard blocks it.
///
/// Returns `Some(unwrapped_json_string)` when `raw` is a single-key object
/// `{"arguments": {object}}`, else `None`. No tool's legitimate schema uses
/// `arguments` as a top-level field name, so the heuristic is safe.
pub fn unwrap_doubly_nested_args(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let inner = map.get("arguments")?;
    if !inner.is_object() {
        return None;
    }
    serde_json::to_string(inner).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct DummyTool;

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "dummy",
                description: "A dummy tool".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
            }
        }

        fn approval(&self, _args: &str) -> ApprovalRequirement {
            ApprovalRequirement::AutoApprove
        }

        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                call_id: "test".to_string(),
                output: "ok".to_string(),
                success: true,
            })
        }
    }

    #[tokio::test]
    async fn test_registry_register_and_get() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool)).await;
        assert!(reg.get("dummy").await.is_some());
        assert!(reg.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_registry_definitions() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool)).await;
        let defs = reg.get_definitions().await;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "dummy");
    }

    #[test]
    fn sensitive_path_detects_relative_traversal_to_unix_root() {
        assert!(is_sensitive_input_path_with_context(
            "../../../etc/passwd",
            Some(Path::new("/home/alice/project")),
            Some(Path::new("/home/alice")),
        ));
    }

    #[test]
    fn sensitive_path_detects_windows_system_roots() {
        assert!(is_sensitive_input_path_with_context(
            r"C:\Windows\System32\drivers\etc\hosts",
            None,
            None,
        ));
        assert!(is_sensitive_input_path_with_context(
            r"D:\Windows\System32\drivers\etc\hosts",
            None,
            None,
        ));
        assert!(is_sensitive_input_path_with_context(
            r"C:\Program Files\AtomCode\config.toml",
            None,
            None,
        ));
        assert!(is_sensitive_input_path_with_context(
            r"C:\ProgramData\AtomCode\config.toml",
            None,
            None,
        ));
    }

    #[test]
    fn sensitive_path_uses_path_boundaries() {
        assert!(!is_sensitive_input_path_with_context(
            "/etc-old/passwd",
            None,
            None,
        ));
        assert!(!is_sensitive_input_path_with_context(
            r"C:\Windows.old\system.ini",
            None,
            None,
        ));
        assert!(!is_sensitive_input_path_with_context(
            r"D:\Windows.old\system.ini",
            None,
            None,
        ));
    }

    #[tokio::test]
    async fn test_tool_execute() {
        let tool = DummyTool;
        let ctx = ToolContext::new(std::env::current_dir().unwrap());
        let result = tool.execute("{}", &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "ok");
    }

    #[test]
    fn resolve_workspace_path_rejects_parent_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let path = format!("{}/secret.txt", outside.path().display());
        std::fs::write(outside.path().join("secret.txt"), "top-secret").unwrap();

        let err = resolve_workspace_path(&path, workspace.path()).unwrap_err();
        assert!(err.to_string().contains("outside working directory"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_workspace_path_rejects_symlink_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "top-secret").unwrap();
        let link = workspace.path().join("secret-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err =
            resolve_workspace_path(link.to_string_lossy().as_ref(), workspace.path()).unwrap_err();
        assert!(err.to_string().contains("outside working directory"));
    }

    #[test]
    fn inspect_path_access_marks_workspace_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "top-secret").unwrap();

        let access = inspect_path_access(&target.to_string_lossy(), workspace.path()).unwrap();
        assert!(!access.within_workspace);
        assert_eq!(access.path, target);
    }

    #[test]
    fn approval_for_non_sensitive_enumeration_outside_workspace_is_auto() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let approval = approval_for_path(
            &outside.path().to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Enumerate,
        )
        .unwrap();
        assert!(matches!(approval, ApprovalRequirement::AutoApprove));
    }

    #[test]
    fn approval_for_non_sensitive_read_outside_workspace_requires_confirmation() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("notes.txt");
        std::fs::write(&target, "hello").unwrap();

        let approval = approval_for_path(
            &target.to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Read,
        )
        .unwrap();
        assert!(matches!(approval, ApprovalRequirement::RequireApproval(_)));
    }

    #[test]
    fn approval_for_sensitive_read_outside_workspace_requires_always() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("id_rsa");
        std::fs::write(&target, "private-key").unwrap();

        let approval = approval_for_path(
            &target.to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Read,
        )
        .unwrap();
        assert!(matches!(
            approval,
            ApprovalRequirement::RequireApprovalAlways(_)
        ));
    }

    #[test]
    fn approval_for_system_protected_prefix_requires_always() {
        assert!(is_sensitive_path(Path::new(
            "/System/Library/CoreServices/boot.efi"
        )));
    }

    #[test]
    fn approval_for_usr_local_exception_is_not_sensitive() {
        assert!(!is_sensitive_path(Path::new("/usr/local/bin/tool")));
    }

    #[test]
    fn approval_for_private_var_prefix_requires_always() {
        assert!(is_sensitive_path(Path::new("/private/var/db/config")));
    }

    #[test]
    fn approval_for_private_var_folders_exception_is_not_sensitive() {
        assert!(!is_sensitive_path(Path::new(
            "/private/var/folders/xx/yy/T/file.txt"
        )));
    }

    #[test]
    fn approval_for_write_outside_workspace_requires_always() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("notes.txt");

        let approval = approval_for_path(
            &target.to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Write,
        )
        .unwrap();
        assert!(matches!(
            approval,
            ApprovalRequirement::RequireApprovalAlways(_)
        ));
    }

    #[tokio::test]
    async fn read_file_requests_approval_for_workspace_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "top-secret").unwrap();

        let tool = crate::tool::read::ReadFileTool;
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, target.display());

        assert!(matches!(
            tool.approval_with_context(&args, &ctx),
            ApprovalRequirement::RequireApproval(_)
        ));
    }

    #[tokio::test]
    async fn edit_file_requests_approval_for_workspace_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "top-secret").unwrap();

        let tool = crate::tool::edit::EditFileTool;
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = format!(
            r#"{{"file_path":"{}","old_string":"top-secret","new_string":"changed"}}"#,
            target.display()
        );

        assert!(matches!(
            tool.approval_with_context(&args, &ctx),
            ApprovalRequirement::RequireApprovalAlways(_)
        ));
    }

    // PermissionStore tests

    #[test]
    fn test_permission_store_auto_approve() {
        let store = PermissionStore::new();
        let decision = store.check("bash", &ApprovalRequirement::AutoApprove);
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    #[test]
    fn test_permission_store_require_approval() {
        let store = PermissionStore::new();
        let decision = store.check(
            "bash",
            &ApprovalRequirement::RequireApproval("Destructive".into()),
        );
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }

    #[test]
    fn test_permission_store_session_grant_bypasses_destructive() {
        // Session grant (user pressed [A]) DOES bypass RequireApproval.
        // The user explicitly chose "Always" — respect that. Bash still has
        // its own destructive-command detection as a separate safety layer.
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        let decision = store.check(
            "bash",
            &ApprovalRequirement::RequireApproval("Destructive".into()),
        );
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    #[test]
    fn test_permission_store_session_grant_does_not_bypass_require_approval_always() {
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        let decision = store.check(
            "bash",
            &ApprovalRequirement::RequireApprovalAlways("Sensitive".into()),
        );
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }

    #[test]
    fn test_permission_store_session_grant_allows_auto_approve() {
        // Session grant still works for non-destructive (AutoApprove) tools.
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        let decision = store.check("bash", &ApprovalRequirement::AutoApprove);
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    #[test]
    fn test_permission_store_always_deny_override() {
        let mut store = PermissionStore::new();
        store.set_override("bash", PermissionLevel::AlwaysDeny);
        // Even AutoApprove is blocked.
        let decision = store.check("bash", &ApprovalRequirement::AutoApprove);
        assert!(matches!(decision, PermissionDecision::Deny));
    }

    #[test]
    fn test_permission_store_always_allow_cannot_bypass_destructive() {
        // Even AlwaysAllow override must NOT bypass RequireApproval.
        let mut store = PermissionStore::new();
        store.set_override("bash", PermissionLevel::AlwaysAllow);
        let decision = store.check(
            "bash",
            &ApprovalRequirement::RequireApproval("Destructive".into()),
        );
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }

    #[tokio::test]
    async fn test_tool_context_isolate() {
        let ctx = ToolContext::new(PathBuf::from("/original"));
        let isolated = ctx.isolate().await;
        // Mutating isolated should not affect original
        *isolated.working_dir.write().await = PathBuf::from("/changed");
        let original_wd = ctx.working_dir.read().await.clone();
        assert_eq!(original_wd, PathBuf::from("/original"));
    }

    #[tokio::test]
    async fn test_registry_iter() {
        let reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool)).await;
        let items: Vec<_> = reg.iter().await.collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "dummy");
    }

    #[tokio::test]
    async fn test_registry_register_arc() {
        let reg1 = ToolRegistry::new();
        reg1.register(Box::new(DummyTool)).await;
        let reg2 = ToolRegistry::new();
        for (name, arc) in reg1.iter().await {
            reg2.register_arc(name, arc).await;
        }
        assert!(reg2.get("dummy").await.is_some());
    }

    #[test]
    fn test_permission_store_session_grant_only_affects_named_tool() {
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        // Other tools are unaffected.
        let decision = store.check(
            "create_file",
            &ApprovalRequirement::RequireApproval("write".into()),
        );
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }

    #[test]
    fn test_unwrap_doubly_nested_args_unwraps_wrapped_object() {
        let raw = r#"{"arguments":{"file_path":"/tmp/x.rs"}}"#;
        let unwrapped = unwrap_doubly_nested_args(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&unwrapped).unwrap();
        assert_eq!(parsed["file_path"], "/tmp/x.rs");
    }

    #[test]
    fn test_unwrap_doubly_nested_args_passes_flat_object_through() {
        // Already flat — must not unwrap.
        let raw = r#"{"file_path":"/tmp/x.rs"}"#;
        assert!(unwrap_doubly_nested_args(raw).is_none());
    }

    #[test]
    fn test_unwrap_doubly_nested_args_ignores_other_single_keys() {
        // A legitimate single-key object whose key happens to be something else.
        let raw = r#"{"command":"ls -la"}"#;
        assert!(unwrap_doubly_nested_args(raw).is_none());
    }

    #[test]
    fn test_unwrap_doubly_nested_args_ignores_multi_key_with_arguments() {
        // Multiple keys including 'arguments' — not the wrapper pattern,
        // could be a legitimate tool that happens to have an 'arguments' field.
        let raw = r#"{"arguments":{"x":1},"other":"y"}"#;
        assert!(unwrap_doubly_nested_args(raw).is_none());
    }

    #[test]
    fn test_unwrap_doubly_nested_args_ignores_string_arguments_value() {
        // Only object-valued 'arguments' is unwrapped; string would be
        // ambiguous (could be a legitimate field carrying free-form text).
        let raw = r#"{"arguments":"some string"}"#;
        assert!(unwrap_doubly_nested_args(raw).is_none());
    }

    #[test]
    fn test_unwrap_doubly_nested_args_ignores_malformed_json() {
        assert!(unwrap_doubly_nested_args("not json").is_none());
        assert!(unwrap_doubly_nested_args("").is_none());
    }
}
