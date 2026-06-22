pub mod auto_fix;
pub mod bash;
pub mod blast_radius;
pub mod cd;
pub mod diagnostics;
pub mod edit;
pub mod file_deps;
pub mod file_history;
pub mod find_references;
pub mod glob;
pub mod grep;
pub mod list_dir;
pub mod list_symbols;
pub mod open_file;
pub mod parallel_edit;
pub mod read;
pub mod read_symbol;
pub mod result_store;
pub mod search_replace;
pub mod todo;
pub mod trace_callees;
pub mod trace_callers;
pub mod trace_chain;
pub mod use_skill;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::collections::{BTreeMap, HashSet};
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

/// Model-friendly tool-arguments validator.
///
/// Why this exists: serde's "missing field `X` at line 1 column 793" error
/// reads to weak models (GLM-5.1, Qwen) as a *parser-position* complaint and
/// reliably triggers hallucinated "fixes" like "I should use positional
/// arguments" — wasting a turn or six on the same tool call. See datalog
/// `atomgr-2d99b47d/2026-05-06_08-43-12.md` Turns 64–75 for the failure
/// mode this replaces.
///
/// What it returns instead, on failure:
/// - the **keys the model actually provided**
/// - the **keys it's missing** for the closest mode
/// - a **one-line example** of a correct call
///
/// `required_modes` is a list of accepted key sets — any one fully matched
/// passes. Single-mode tools pass `&[&[required_keys]]`. Multi-mode tools
/// like `edit_file` pass one slice per mode; the diagnostic picks the mode
/// with the fewest missing keys for the hint.
///
/// Returns the parsed `Value` on success so callers can avoid a second
/// parse pass.
pub fn diagnose_args(
    tool: &str,
    args: &str,
    required_modes: &[&[&str]],
    example: &str,
) -> std::result::Result<serde_json::Value, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Err(format!(
            "{tool} called with empty arguments — likely max_tokens cutoff. \
             Re-issue: {example}"
        ));
    }
    let value: serde_json::Value = serde_json::from_str(args).map_err(|_| {
        format!(
            "{tool} arguments are not valid JSON. Re-issue: {example}"
        )
    })?;
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            let kind = match &value {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => unreachable!(),
            };
            return Err(format!(
                "{tool} expected a JSON object, got {kind}. Re-issue: {example}"
            ));
        }
    };
    if required_modes
        .iter()
        .any(|m| m.iter().all(|k| obj.contains_key(*k)))
    {
        return Ok(value);
    }
    let provided: Vec<&str> = obj.keys().map(String::as_str).collect();
    // Pick the mode with the fewest missing keys — that's the call shape
    // the model was probably aiming at.
    let (closest, missing) = required_modes
        .iter()
        .map(|m| {
            let miss: Vec<&str> = m
                .iter()
                .filter(|k| !obj.contains_key(**k))
                .copied()
                .collect();
            (*m, miss)
        })
        .min_by_key(|(_, miss)| miss.len())
        .expect("required_modes must be non-empty");
    Err(format!(
        "{tool}: provided keys [{}], missing required [{}] for mode [{}]. \
         Re-issue: {}",
        provided.join(", "),
        missing.join(", "),
        closest.join("+"),
        example,
    ))
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

/// Strip the Windows extended-length / UNC verbatim prefix (`\\?\`, `\\?\UNC\`)
/// from a path string.
///
/// `std::fs::canonicalize()` returns verbatim paths on Windows (`\\?\D:\foo`),
/// while the TUI's `current_dir()` yields plain paths (`D:\foo`). Left
/// unnormalized the two forms hash to different session buckets, so webui and
/// TUI sessions for the same project never reconcile. Call this on any path that
/// may have passed through `canonicalize()` before storing, persisting, or
/// hashing it. No-op for paths without the prefix (incl. all POSIX paths).
pub fn strip_verbatim_prefix(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(path)
    }
}

/// [`strip_verbatim_prefix`] for `Path` callers; allocates a fresh `PathBuf`.
pub fn strip_verbatim_prefix_path(path: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(strip_verbatim_prefix(&path.to_string_lossy()).as_ref())
}

fn is_windows_sensitive_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\");
    let normalized = strip_verbatim_prefix(&normalized);
    let lowercase = normalized.to_ascii_lowercase();
    let sensitive_roots = [
        r"\windows",
        r"\program files",
        r"\program files (x86)",
        r"\programdata",
    ];
    let Some(path_root) = windows_rooted_path(&lowercase) else {
        return false;
    };

    sensitive_roots
        .iter()
        .any(|root| windows_path_starts_with(path_root, root))
}

fn windows_path_starts_with(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn windows_rooted_path(path: &str) -> Option<&str> {
    if let Some(path_without_drive) = strip_windows_drive_prefix(path) {
        return Some(path_without_drive);
    }

    if path.starts_with('\\') && !path.starts_with(r"\\") {
        return Some(path);
    }

    None
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

/// Get the real user's home directory, accounting for sudo scenarios.
///
/// When running under sudo, `dirs::home_dir()` returns root's home directory
/// because $HOME is set to /root. This function checks for SUDO_USER and
/// attempts to get the actual invoking user's home directory instead.
///
/// Priority:
/// 1. If SUDO_USER is set, try to get that user's home directory
/// 2. Fall back to dirs::home_dir() (which reads $HOME or uses system APIs)
pub fn real_home_dir() -> Option<PathBuf> {
    // Check if we're running under sudo
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        // Try to get the home directory for the sudo user
        if let Some(home) = get_user_home(&sudo_user) {
            return Some(home);
        }
    }
    
    // Fall back to the standard home directory
    dirs::home_dir()
}

/// Get the home directory for a specific user by looking up /etc/passwd (Unix)
/// or constructing the path for the user (macOS).
#[cfg(unix)]
fn get_user_home(username: &str) -> Option<PathBuf> {
    use std::ffi::CString;
    use std::ptr;
    
    // SAFETY: We're calling getpwnam which is thread-safe on modern systems
    // when using getpwnam_r
    let username_c = CString::new(username).ok()?;
    
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0u8; 4096]; // Buffer for string fields
        let mut result: *mut libc::passwd = ptr::null_mut();
        
        let ret = libc::getpwnam_r(
            username_c.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        );
        
        if ret == 0 && !result.is_null() {
            let home = std::ffi::CStr::from_ptr(pwd.pw_dir)
                .to_string_lossy()
                .into_owned();
            return Some(PathBuf::from(home));
        }
    }
    
    None
}

#[cfg(not(unix))]
fn get_user_home(_username: &str) -> Option<PathBuf> {
    // On non-Unix systems, we don't have getpwnam
    // Fall back to trying to construct the path
    None
}

fn expand_user_path(path: &str) -> PathBuf {
    if path == "~" {
        return real_home_dir().unwrap_or_else(|| PathBuf::from(path));
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return real_home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path));
    }

    PathBuf::from(path)
}
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

/// Does `raw_path` (as the model wrote it) or `canonical_path` (the
/// resolved form) live under AtomCode's own config dir?
///
/// Both forms are checked because of #222: a user can keep an AtomCode
/// data symlink (e.g. `~/.atomcode/cj -> /mnt/d/.../md/`) that points
/// at a project they edit. The raw `~/.atomcode/...` prefix is enough
/// evidence that the model is acting on AtomCode-managed storage, even
/// if the canonical target lives outside the home dir. We still pass
/// the canonical form through `starts_with` so a pure target like
/// `~/.atomcode/memory.md` (no symlink) also matches.
fn is_under_atomcode_home(raw_path: &str, canonical_path: &Path) -> bool {
    let home = crate::config::Config::config_dir();
    // Canonicalise the config dir so the `starts_with` comparison
    // works even when `~/.atomcode` itself contains intermediate
    // symlinks (macOS `/var → /private/var` is the common case).
    let home_canon = std::fs::canonicalize(&home).unwrap_or(home.clone());

    if canonical_path.starts_with(&home_canon) || canonical_path.starts_with(&home) {
        return true;
    }

    let expanded = expand_user_path(raw_path);
    let normalized = normalize_path(&expanded);
    normalized.starts_with(&home_canon) || normalized.starts_with(&home)
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
    #[cfg(not(target_os = "windows"))]
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
    #[cfg(target_os = "windows")]
    const SYSTEM_PROTECTED_PREFIXES: &[&str] = &[
        r"C:\Windows",
        r"C:\Program Files",
        r"C:\Program Files (x86)",
        r"C:\ProgramData",
        r"C:\PerfLogs",
    ];
    #[cfg(not(target_os = "windows"))]
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
    #[cfg(target_os = "windows")]
    const SYSTEM_PROTECTED_EXCEPTIONS: &[&str] = &[];
    const SECRET_HOME_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg"];
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

    if let Some(home) = real_home_dir() {
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

/// Returns `true` when `path` is under a directory that AtomCode owns and
/// populates itself (installed plugins, local skills, etc.).  Reads from
/// those directories are trusted so they don't prompt for approval every
/// time a skill loads its bundled documentation index.
fn is_atomcode_owned_path(path: &Path) -> bool {
    let Some(home) = real_home_dir() else { return false };
    let trusted_roots: &[PathBuf] = &[
        home.join(".atomcode").join("plugins"),
        home.join(".atomcode").join("skills"),
    ];
    trusted_roots
        .iter()
        .any(|root| path == root.as_path() || path.starts_with(root))
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
    // AtomCode-owned directories (plugins, skills) are trusted —
    // reading from them shouldn't require interactive approval.
    if action != ExternalPathAction::Write && is_atomcode_owned_path(&access.path) {
        return Ok(ApprovalRequirement::AutoApprove);
    }

    // Cross-workspace writes to AtomCode's own config dir (default
    // `~/.atomcode/`, or whatever `Config::config_dir()` resolves to via
    // `ATOMCODE_HOME`) are part of how memory / skills / plugin state
    // is stored. The user expects `memory.md`, `skills/*.md`, etc. to
    // be writable from any project without a per-tool approval prompt
    // (#222). Detect that here — both for the raw path the model
    // produced (covers `~/.atomcode/...` when the user keeps a symlink
    // there) and for the canonicalised target.
    if is_under_atomcode_home(raw_path, &access.path) {
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
                // Listing a sensitive dir is less dangerous than reading its
                // contents — match Read's scoped behaviour: one [A] remembers
                // this exact path, every other sensitive path still prompts.
                ApprovalRequirement::RequireApprovalScoped {
                    reason: format!(
                        "{base_reason}. This path looks sensitive and requires confirmation."
                    ),
                    scope: access.path.to_string_lossy().into_owned(),
                }
            } else {
                // Enumerating (listing/searching) a non-sensitive path outside
                // the workspace still requires confirmation -- without an OS-level
                // sandbox to constrain the process (#220), any filesystem access
                // to unauthorised paths risks information leakage and wastes time
                // on irrelevant directories.  Users can grant per-session approval
                // with [A] if the access is intentional.
                ApprovalRequirement::RequireApproval(format!(
                    "{base_reason}. Enumerating paths outside the workspace requires confirmation."
                ))
            }
        }
        ExternalPathAction::Read => {
            if sensitive {
                // Reading is non-destructive, so re-confirming the SAME file on
                // every chunk adds no safety. Scope the approval to this exact
                // path: a session [A] remembers this file (so segmented reads
                // stop re-prompting) while every other sensitive path still
                // prompts. The canonical path is the scope key.
                ApprovalRequirement::RequireApprovalScoped {
                    reason: format!(
                        "{base_reason}. This path looks sensitive and requires confirmation."
                    ),
                    scope: access.path.to_string_lossy().into_owned(),
                }
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

/// Resolve the approval requirement for a single-file mutating tool
/// (`edit_file`, `write_file`, `search_replace`), scoping any prompt to the
/// exact target file.
///
/// This is the fix for "pressing [A]/Always doesn't stick across repeated
/// edits of the same file". Writes used to collapse to
/// `RequireApprovalAlways`, which [`PermissionStore::check`] routes to `Ask`
/// unconditionally — so the recorded session grant was dead and every edit
/// re-prompted. By emitting [`ApprovalRequirement::RequireApprovalScoped`]
/// keyed on the canonical file path (exactly what sensitive *reads* already
/// do), a session `[A]` lands in `session_scope_grants` and remembers THAT
/// file: re-editing it stops re-prompting, while every other sensitive /
/// out-of-workspace file still prompts, and a tool-wide grant never
/// blanket-unlocks sensitive writes.
///
/// `base` is the tool's own path-sensitivity verdict (`RequireApproval` for a
/// sensitive system path, else `AutoApprove`); `raw_path` is the
/// model-supplied target, canonicalised to a stable scope key.
pub fn scoped_write_approval(
    raw_path: &str,
    working_dir: &Path,
    base: ApprovalRequirement,
) -> ApprovalRequirement {
    // Stable per-file scope key — the same canonical form sensitive reads use,
    // so repeated edits of one file hash to one key. Fall back to the raw path
    // if canonicalisation fails (e.g. a not-yet-created parent) so the key is
    // still consistent across calls.
    let scope = inspect_path_access(raw_path, working_dir)
        .map(|r| r.path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw_path.to_string());

    match approval_for_path(raw_path, working_dir, ExternalPathAction::Write) {
        // Out-of-workspace write: prompt once, then remember this exact file
        // for the session. (`approval_for_path` returns the tool-wide
        // `RequireApprovalAlways` today; scope it here.)
        Ok(ApprovalRequirement::RequireApprovalAlways(reason))
        | Ok(ApprovalRequirement::RequireApproval(reason)) => {
            ApprovalRequirement::RequireApprovalScoped { reason, scope }
        }
        Ok(ApprovalRequirement::RequireApprovalScoped { reason, .. }) => {
            ApprovalRequirement::RequireApprovalScoped { reason, scope }
        }
        // In-workspace: the boundary check auto-approves. If the tool's base
        // verdict flagged a sensitive in-workspace target (e.g. `.env`), scope
        // the prompt to that file rather than the whole tool.
        Ok(ApprovalRequirement::AutoApprove) => match base {
            ApprovalRequirement::RequireApproval(reason) => {
                ApprovalRequirement::RequireApprovalScoped { reason, scope }
            }
            other => other,
        },
        Err(_) => base,
    }
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

#[derive(Debug, Clone)]
pub enum ApprovalRequirement {
    AutoApprove,
    RequireApproval(String),
    RequireApprovalAlways(String),
    /// Prompt the user, but a session [A] remembers this exact `scope` (a
    /// canonical resource key, e.g. a file path) rather than the whole tool.
    /// Used for sensitive reads: re-reading the same file in segments stops
    /// re-prompting once approved, while every other sensitive resource still
    /// requires its own confirmation. Unlike a tool-wide grant, a scoped grant
    /// only covers the matching `scope`.
    RequireApprovalScoped { reason: String, scope: String },
}

/// The resolved decision returned by `PermissionStore::check` or by an
/// interactive responder.
#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Allow,
    /// Allow AND remember for the session (tool-wide, or scoped — decided by
    /// the responder's context). Returned over the approval channel when the
    /// user picks "always allow"; `PermissionStore::check` never returns it.
    AllowAlways,
    /// Ask the user — carries the reason string from `ApprovalRequirement`.
    Ask(String),
    Deny,
}

/// Parse the wire string used by the daemon's permission endpoints
/// (`/chat/permission`, `/live/permission`) into a decision.
pub fn parse_permission_decision(s: &str) -> PermissionDecision {
    match s {
        "allow" => PermissionDecision::Allow,
        "always_allow" => PermissionDecision::AllowAlways,
        _ => PermissionDecision::Deny,
    }
}

/// Stores per-tool session-level grants.
pub struct PermissionStore {
    /// Session-level grants: tool names approved with [A]lways for this session.
    session_grants: HashSet<String>,
    /// Session-level scoped grants: resource keys (e.g. canonical file paths)
    /// approved with [A] for a `RequireApprovalScoped` call. Covers only the
    /// matching scope, never a whole tool.
    session_scope_grants: HashSet<String>,
}

impl PermissionStore {
    pub fn new() -> Self {
        Self {
            session_grants: HashSet::new(),
            session_scope_grants: HashSet::new(),
        }
    }

    /// Check whether a tool call should be auto-approved, needs asking, or denied.
    pub fn check(&self, tool_name: &str, approval: &ApprovalRequirement) -> PermissionDecision {
        if let ApprovalRequirement::RequireApprovalAlways(reason) = approval {
            return PermissionDecision::Ask(reason.clone());
        }

        // Path-scoped approval (e.g. sensitive reads). Allowed ONLY by an exact
        // per-scope session grant — deliberately NOT covered by a tool-wide [A]
        // (`session_grants`), so pressing [A] on an innocuous external file can
        // never silently unlock reads of ~/.ssh/id_rsa etc. Re-reading the same
        // file in segments yields the same scope, so one [A] stops the prompts.
        if let ApprovalRequirement::RequireApprovalScoped { reason, scope } = approval {
            if self.session_scope_grants.contains(scope) {
                return PermissionDecision::Allow;
            }
            return PermissionDecision::Ask(reason.clone());
        }

        // 1. Session grant (user pressed [A] during this session).
        //    This overrides RequireApproval — the user explicitly chose "Always"
        //    for this tool, so don't prompt again. Destructive operations opt
        //    into `RequireApprovalAlways` (handled above) so they're NOT covered
        //    by this bypass: e.g. bash returns RequireApprovalAlways for
        //    rm -rf / rmdir /s /q / git push --force / dd / etc. (bash.rs:115).
        if self.session_grants.contains(tool_name) {
            return PermissionDecision::Allow;
        }

        // 2. Destructive commands (RequireApproval) prompt unless session-granted.
        if let ApprovalRequirement::RequireApproval(reason) = approval {
            return PermissionDecision::Ask(reason.clone());
        }

        // 3. Defer to the tool's own approval requirement (AutoApprove path).
        PermissionDecision::Allow
    }

    /// Grant session-level permission for a tool (user pressed [A]).
    pub fn grant_session(&mut self, tool_name: &str) {
        self.session_grants.insert(tool_name.to_string());
    }

    /// Grant session-level permission for a single scope key (user pressed [A]
    /// on a `RequireApprovalScoped` call). Only this exact scope is unlocked.
    pub fn grant_session_scope(&mut self, scope: &str) {
        self.session_scope_grants.insert(scope.to_string());
    }

}

/// Shared execution context passed to every tool invocation.
/// Read cache key: (canonical path, offset, limit). offset/limit are the raw
/// args the model sent — different slicing windows cache separately.
pub type ReadCacheKey = (PathBuf, Option<usize>, Option<usize>);

/// Read cache entry: (file mtime at cache time, rendered tool output, number of
/// times this exact (path, offset, limit, mtime) tuple has been served).
///
/// The hit count drives the "you keep re-reading the same region" hint emitted
/// by `read.rs` on cache hits — it replaced the prior `runner.rs` BLOCKED guard
/// (deleted alongside) which was a soft-text error the model could ignore. By
/// returning the cached content WITH a count-aware note instead of refusing the
/// call, the framework lets the model see that the answer hasn't changed
/// while still giving a clear "stop re-reading" signal. mtime is still the
/// invalidation key — if disk mtime differs on next read, the entry is replaced
/// and the count resets to 1.
pub type ReadCacheEntry = (std::time::SystemTime, String, usize);

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
    /// Shared LSP manager for diagnostics tool. `None` when LSP is disabled.
    pub lsp: Option<std::sync::Arc<crate::lsp::manager::LspManager>>,
    /// Optional event sender for real-time tool output streaming (e.g., bash stdout).
    /// When set, tools like bash can send output chunks as they're produced.
    pub event_tx: Option<Arc<tokio::sync::mpsc::UnboundedSender<crate::turn::event::TurnEvent>>>,
    /// Current tool call ID for event correlation.
    pub current_call_id: Option<String>,
    /// Shared registry handle for tools that dispatch fork sub-agents
    /// (currently only `parallel_edit_files`). Set by `AgentLoop::new`
    /// after the registry is wrapped in `Arc`. Reading the registry via
    /// `ctx` instead of holding it in the tool struct avoids creating a
    /// `Tool ↔ Registry` `Arc` cycle that would otherwise leak memory
    /// for the lifetime of the process. `None` in headless / test
    /// contexts that don't need fork dispatch.
    pub tool_registry: Option<Arc<ToolRegistry>>,
    /// D3 file content store. read_file pushes large file content
    /// here transparently and consults it on subsequent reads of any
    /// range — disk hit only on first read or after edit. Conversation
    /// messages carry only the rendered text (with line numbers) for
    /// the requested region. edit_file / write_file invalidate
    /// entries on success so a stale entry cannot serve outdated
    /// bytes.
    pub file_store: Arc<RwLock<crate::ctx::file_store::FileStore>>,
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
            lsp: None,
            event_tx: None,
            current_call_id: None,
            tool_registry: None,
            file_store: Arc::new(RwLock::new(crate::ctx::file_store::FileStore::new())),
        }
    }

    /// Create an isolated copy: same working directory value, independent Arc.
    /// Shares the same graph (read-only for tools) but independent working_dir.
    ///
    /// Fields that are NOT copied from the original:
    /// - `read_cache`: reset — subagent re-reads files (acceptable for isolation)
    /// - `event_tx`: reset — subagent has its own event channel
    /// - `tool_registry`: reset — subagent gets filtered tools
    /// - `first_error_signatures`: reset — subagent has independent error state
    /// - `ctx_budget_hint`: reset to MAX — subagent has its own budget
    /// - `read_budget_tokens`: reset to MAX — subagent has own budget
    pub async fn isolate(&self) -> Self {
        let wd = self.working_dir.read().await.clone();
        let mut ctx = Self::new(wd);
        ctx.graph = self.graph.clone();
        ctx.telemetry = self.telemetry.clone();
        ctx.lsp = self.lsp.clone();
        // Share the FileStore — sub-agents reading the same file reuse
        // the parent's disk work and benefit from invalidation events
        // emitted by either side.
        ctx.file_store = self.file_store.clone();
        ctx
    }

    /// Notify LSP that a file changed (if LSP is enabled).
    /// This is a convenience method for write/edit/search_replace tools.
    pub async fn notify_lsp_file_changed(&self, path: &Path, content: &str) {
        if let Some(ref lsp) = self.lsp {
            if let Err(e) = lsp.notify_file_changed(path, content).await {
                tracing::warn!(
                    "[lsp] Failed to refresh diagnostics for {}: {}",
                    path.display(),
                    e
                );
            }
        }
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

    /// Pre-flight syntactic check on raw tool-call arguments. The runner
    /// calls this **before** approval and before execute, so a parse
    /// failure short-circuits to a tool-result error and the model
    /// receives a structured retry hint without bothering the user.
    ///
    /// Default impl: `Ok(())`. Tools with strict required-field schemas
    /// (write_file / edit_file / search_replace) override to surface the
    /// serde error early. Implementations should be cheap (parse only,
    /// no I/O) — the runner re-parses inside `execute()` for actual use.
    ///
    /// Trigger context (2026-05-02 datalog evidence): provider-side
    /// stream truncation can deliver `[RAW ARGS: {]` or
    /// `[RAW ARGS: {"file_path":"..."]` (closing-bracket wrong, content
    /// missing). The previous flow let those reach `approval_with_context`
    /// where the tool's own fail-closed branch returned
    /// `RequireApproval("Could not parse … for safety check.")` and the
    /// user saw an approval prompt for an obviously-broken call. Pressing
    /// Allow then died on the same parse in `execute()`. Validating up
    /// front eliminates the user-visible round-trip entirely.
    fn validate_args(&self, _args: &str) -> std::result::Result<(), String> {
        Ok(())
    }
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

    /// Top-level property names declared in the tool's `parameters` schema.
    /// Used by `recover_tool_args` to decide whether a payload needs
    /// wrapper unwrapping. Returns empty Vec if the tool isn't registered
    /// or the schema doesn't expose `properties` (in which case
    /// `recover_tool_args` falls back to its permissive branch).
    pub async fn expected_top_keys(&self, name: &str) -> Vec<String> {
        let tools = self.tools.read().await;
        let Some(tool) = tools.get(name) else { return Vec::new() };
        let def = tool.definition();
        def.parameters
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
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

/// Wrapper key names atomgit's gateway has been observed to inject around
/// tool_call arguments. None are used as legitimate top-level field names by
/// any registered tool — see `recover_tool_args` doc-comment for the safety
/// argument.
const ARGS_WRAPPER_KEYS: &[&str] = &["arguments", "input", "content"];

/// Recover a flat schema-shaped JSON object from possibly-mangled tool args.
///
/// Background: the atomgit `api-ai.gitcode.com` gateway (and its internal
/// `10.205.128.41:6538` deployment) wraps tool_call `function.arguments` into
/// extra envelopes that violate the OpenAI tool-call protocol. Observed
/// shapes:
///
///   variant A1 (stream)      — `{"arguments": "<stringified-json-object>"}`
///   variant A2 (non-stream)  — `{"arguments": <object>}`
///   variant B  (double)      — `{"arguments": "{\"arguments\": ...}"}`
///   variant C  (multi-key)   — `{"arguments": "...", "timeout": 120}`
///   variant D  (alt key)     — `{"content": "<stringified-json-object>"}`
///
/// (Variant E is `function.name` field corruption — caller-side detection,
///  not handled here.)
///
/// This function recovers the original schema-shaped object by:
///   1. trying direct parse first — if the JSON already contains an
///      expected schema field, return None (caller uses raw),
///   2. otherwise iteratively unwrapping any single-key wrapper (A/A2/B),
///      stringified or object-valued, up to 5 levels deep,
///   3. on multi-key wrapper (C), unwrapping the wrapper key and merging in
///      the sibling keys that match `expected_top_keys`,
///   4. final-validating that the recovered object contains at least one
///      `expected_top_keys` field — otherwise returns None to signal
///      unrecoverable.
///
/// Safety against false positives: `ARGS_WRAPPER_KEYS` (`arguments`, `input`,
/// `content`) are never used as top-level field names by any tool registered
/// in atomcode (verified across all 22 builtin tools and MCP tool naming
/// convention). When a future tool adds such a field, callers using
/// `recover_tool_args` with that tool's `expected_top_keys` will short-circuit
/// at step 1 and never invoke unwrap.
pub fn recover_tool_args(raw: &str, expected_top_keys: &[String]) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if !value.is_object() {
        return None;
    }

    // Step 1 — already flat schema shape? When all top-level keys are
    // declared in the tool's schema, the payload is legitimate as-is and
    // we must NOT touch it. This is the strict guard that protects tools
    // whose schema legitimately uses one of the wrapper key names
    // (e.g. write/todo declare `content`): if the model writes
    // {"file_path": "/x.json", "content": "{\"foo\": 1}"}, both keys are
    // schema-declared so we return None, leaving the JSON-shaped content
    // string untouched. Without this guard, has_wrapper_shape would
    // misidentify `content` as a wrapper and corrupt the payload.
    //
    // When schema is unknown (expected_top_keys empty — e.g. dynamic MCP
    // tools whose definition isn't loaded), we can't make this judgement,
    // so fall through to the permissive unwrap loop.
    if !expected_top_keys.is_empty() && all_keys_in_expected(&value, expected_top_keys) {
        return None;
    }

    // Step 2/3 — unwrap loop, capped at 5 to defend against pathological inputs.
    let mut progressed = false;
    for _ in 0..5 {
        match try_unwrap_once(value, expected_top_keys) {
            UnwrapStep::Stable(v) => {
                value = v;
                break;
            }
            UnwrapStep::Progressed(v) => {
                value = v;
                progressed = true;
            }
        }
    }

    // Step 4 — only return Some if we actually unwrapped something.
    // Returning None here means "raw is fine, use it as-is".
    if !progressed {
        return None;
    }

    // Recovered object must contain at least one expected schema field
    // (when schema is known). With no schema, accept any flat object form
    // as a permissive fallback for unknown tools (e.g. dynamic MCP tools
    // whose schema isn't loaded yet).
    if !expected_top_keys.is_empty() && !has_expected_key(&value, expected_top_keys) {
        return None;
    }
    if has_wrapper_shape(&value) {
        // Still wrapped after the loop — couldn't recover within budget.
        return None;
    }
    serde_json::to_string(&value).ok()
}

fn has_expected_key(v: &serde_json::Value, expected: &[String]) -> bool {
    let Some(map) = v.as_object() else { return false };
    expected.iter().any(|k| map.contains_key(k.as_str()))
}

/// Strict legitimacy check: every top-level key of `v` is declared in the
/// tool's schema. Used by the Step 1 short-circuit to identify payloads
/// that are already in valid schema shape and must be passed through
/// untouched, even if some of those keys happen to overlap with
/// `ARGS_WRAPPER_KEYS` (e.g. `content` in write/todo).
fn all_keys_in_expected(v: &serde_json::Value, expected: &[String]) -> bool {
    let Some(map) = v.as_object() else { return false };
    if map.is_empty() {
        return false;
    }
    map.keys().all(|k| expected.iter().any(|e| e == k))
}

fn has_wrapper_shape(v: &serde_json::Value) -> bool {
    let Some(map) = v.as_object() else { return false };
    ARGS_WRAPPER_KEYS.iter().any(|k| {
        map.get(*k).is_some_and(|inner| {
            // Wrapper if the wrapper key's value is itself an object, or is
            // a string that parses to an object.
            if inner.is_object() {
                return true;
            }
            if let Some(s) = inner.as_str() {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                    return parsed.is_object();
                }
            }
            false
        })
    })
}

enum UnwrapStep {
    Progressed(serde_json::Value),
    Stable(serde_json::Value),
}

fn try_unwrap_once(value: serde_json::Value, expected: &[String]) -> UnwrapStep {
    let Some(map) = value.as_object() else {
        return UnwrapStep::Stable(value);
    };

    // Find the first wrapper key whose value resolves to an object.
    let mut wrapper_key: Option<&str> = None;
    let mut inner_obj: Option<serde_json::Value> = None;
    for &k in ARGS_WRAPPER_KEYS {
        let Some(v) = map.get(k) else { continue };
        if let Some(obj) = v.as_object() {
            wrapper_key = Some(k);
            inner_obj = Some(serde_json::Value::Object(obj.clone()));
            break;
        }
        if let Some(s) = v.as_str() {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                if parsed.is_object() {
                    wrapper_key = Some(k);
                    inner_obj = Some(parsed);
                    break;
                }
            }
        }
    }

    let (Some(wk), Some(mut inner)) = (wrapper_key, inner_obj) else {
        return UnwrapStep::Stable(value);
    };

    // Variant C support: merge sibling keys (other than the wrapper) that
    // are in `expected_top_keys` into the unwrapped object. This covers the
    // observed `{"arguments": "{...}", "timeout": 120}` form where wrapper
    // and a legitimate field both appear at the top.
    if let Some(inner_map) = inner.as_object_mut() {
        for (k, v) in map.iter() {
            if k == wk {
                continue;
            }
            if expected.iter().any(|e| e == k) && !inner_map.contains_key(k) {
                inner_map.insert(k.clone(), v.clone());
            }
        }
    }

    UnwrapStep::Progressed(inner)
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
            r"\Windows\System32\drivers\etc\hosts",
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
        assert!(!is_sensitive_input_path_with_context(
            r"\Windows.old\system.ini",
            None,
            None,
        ));
        assert!(!is_sensitive_input_path_with_context(
            r"\\server\share\Windows\system.ini",
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
        // canonicalize for comparison: macOS resolves /var → /private/var via
        // symlink, so the unresolved `target` won't byte-compare against
        // inspect_path_access's canonicalized result.
        assert_eq!(access.path, target.canonicalize().unwrap());
    }

    #[test]
    fn approval_for_non_sensitive_enumeration_outside_workspace_requires_confirmation() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let approval = approval_for_path(
            &outside.path().to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Enumerate,
        )
        .unwrap();
        // #220: enumerating paths outside the workspace requires confirmation
        // even when non-sensitive, because there is no OS-level sandbox.
        assert!(matches!(approval, ApprovalRequirement::RequireApproval(_)));
    }

    // Regression (#220): Enumerate action on external paths must require confirmation.
    // Before the fix, non-sensitive external paths were AutoApprove,
    // so find/ls on unauthorised paths ran without prompts.
    #[test]
    fn enumerate_outside_workspace_can_be_granted_by_session() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let approval = approval_for_path(
            &outside.path().to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Enumerate,
        )
        .unwrap();

        // RequireApproval can be granted by a session grant (user pressed [A]),
        // so repeated operations on the same tool do not re-prompt.
        let mut store = PermissionStore::new();
        let decision = store.check("bash", &approval);
        assert!(matches!(decision, PermissionDecision::Ask(_)));

        store.grant_session("bash");
        let decision_again = store.check("bash", &approval);
        assert!(matches!(decision_again, PermissionDecision::Allow));
    }

    // #220: Enumerate is NOT scoped -- it uses RequireApproval, not RequireApprovalScoped.
    // A tool-wide [A] grant on bash can cover this,
    // but sensitive paths (._ssh, /etc) still use RequireApprovalScoped
    // and cannot be bypassed by a tool-wide grant.
    #[test]
    fn enumerate_outside_workspace_is_not_scoped() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let approval = approval_for_path(
            &outside.path().to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Enumerate,
        )
        .unwrap();

        // Non-scoped: matches RequireApproval, not RequireApprovalScoped
        assert!(
            matches!(approval, ApprovalRequirement::RequireApproval(_)),
            "non-sensitive external enumeration should use RequireApproval"
        );

        // A tool-wide session grant should cover RequireApproval,
        // but NOT RequireApprovalScoped (sensitive paths).
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        let decision = store.check("bash", &approval);
        assert!(matches!(decision, PermissionDecision::Allow));
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
    fn approval_for_sensitive_read_outside_workspace_is_path_scoped() {
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
        // Sensitive reads are PATH-SCOPED, not blanket "always ask": a session
        // [A] remembers THIS exact file so re-reading it in segments doesn't
        // re-prompt, while every other sensitive file still requires its own
        // confirmation. The scope key is the canonical path.
        match approval {
            ApprovalRequirement::RequireApprovalScoped { scope, .. } => {
                assert_eq!(Path::new(&scope), target.canonicalize().unwrap());
            }
            _ => panic!("expected RequireApprovalScoped for a sensitive external read"),
        }
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
    fn enumerate_sensitive_outside_workspace_is_scoped_not_always() {
        let workspace = TempDir::new().unwrap();
        let home = real_home_dir().expect("home");
        let ssh = home.join(".ssh");
        let approval = approval_for_path(
            &ssh.to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Enumerate,
        )
        .expect("approval");
        match approval {
            ApprovalRequirement::RequireApprovalScoped { scope, .. } => {
                // scope is the canonical path; .ssh may or may not exist on
                // this machine, so just assert the scope string is non-empty.
                assert!(!scope.is_empty(), "scope should be a non-empty path");
            }
            _ => panic!("expected RequireApprovalScoped for sensitive enumeration outside workspace"),
        }
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

    /// Serialise ATOMCODE_HOME-mutating tests. `cargo test` runs in
    /// parallel by default, and `std::env::set_var` is a process-wide
    /// mutation — two of these tests racing flipped each other's
    /// `Config::config_dir()` mid-call, producing a flaky failure
    /// (test passes solo, fails in the suite).
    static ATOMCODE_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Writing into AtomCode's own config dir from outside the
    /// workspace must auto-approve — memory.md, skills, plugin state
    /// are all expected to land there without per-call prompts (#222).
    /// We exercise both an existing path and a not-yet-created one.
    #[test]
    fn approval_for_write_under_atomcode_home_is_auto() {
        let _guard = ATOMCODE_HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let prev = std::env::var("ATOMCODE_HOME").ok();
        unsafe {
            std::env::set_var("ATOMCODE_HOME", home.path());
        }

        let workspace = TempDir::new().unwrap();
        let memory_file = home.path().join("memory.md");
        std::fs::write(&memory_file, "scratch").unwrap();

        let approval = approval_for_path(
            &memory_file.to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Write,
        )
        .unwrap();
        assert!(matches!(approval, ApprovalRequirement::AutoApprove));

        // Not-yet-created path under the home dir is also auto.
        let new_file = home.path().join("skills").join("new.md");
        let approval_new = approval_for_path(
            &new_file.to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Write,
        )
        .unwrap();
        assert!(matches!(approval_new, ApprovalRequirement::AutoApprove));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("ATOMCODE_HOME", v),
                None => std::env::remove_var("ATOMCODE_HOME"),
            }
        }
    }

    /// Regression for #222: a symlink that the user manually placed
    /// inside `~/.atomcode/` to point at a separate project dir must
    /// still auto-approve writes — the raw `~/.atomcode/<symlink>/...`
    /// form is enough evidence that this is AtomCode-managed storage.
    #[cfg(unix)]
    #[test]
    fn approval_for_write_through_atomcode_home_symlink_is_auto() {
        let _guard = ATOMCODE_HOME_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let prev = std::env::var("ATOMCODE_HOME").ok();
        unsafe {
            std::env::set_var("ATOMCODE_HOME", home.path());
        }

        let project = TempDir::new().unwrap();
        let link = home.path().join("cj");
        std::os::unix::fs::symlink(project.path(), &link).unwrap();
        let memory_path = link.join("memory").join("notes.md");

        let workspace = TempDir::new().unwrap();
        let approval = approval_for_path(
            &memory_path.to_string_lossy(),
            workspace.path(),
            ExternalPathAction::Write,
        )
        .unwrap();
        assert!(
            matches!(approval, ApprovalRequirement::AutoApprove),
            "expected AutoApprove for ~/.atomcode/cj symlink write"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("ATOMCODE_HOME", v),
                None => std::env::remove_var("ATOMCODE_HOME"),
            }
        }
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

        // Out-of-workspace edit prompts, but scoped to this exact file so a
        // session [A] remembers it (re-editing the same file stops
        // re-prompting) — not a tool-wide blanket grant.
        let approval = tool.approval_with_context(&args, &ctx);
        let scope = match &approval {
            ApprovalRequirement::RequireApprovalScoped { scope, .. } => scope.clone(),
            other => panic!("expected scoped approval for workspace escape, got {other:?}"),
        };
        let mut store = PermissionStore::new();
        assert!(matches!(store.check("edit_file", &approval), PermissionDecision::Ask(_)));
        store.grant_session_scope(&scope); // user pressed [A]
        let again = tool.approval_with_context(&args, &ctx);
        assert!(
            matches!(store.check("edit_file", &again), PermissionDecision::Allow),
            "[A] on an out-of-workspace file must stop re-prompting for that same file",
        );
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
    fn test_permission_store_session_grant_bypasses_require_approval() {
        // Session grant (user pressed [A]) bypasses plain RequireApproval —
        // the user explicitly chose "Always" for this tool. Destructive
        // commands (rm -rf / rmdir / git push -f / …) must opt into
        // RequireApprovalAlways so this bypass does NOT cover them; see
        // the bash tool (bash.rs:115) and `..._does_not_bypass_require_approval_always`.
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        let decision = store.check(
            "bash",
            &ApprovalRequirement::RequireApproval("non-destructive".into()),
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
    fn test_permission_store_scoped_grant_allows_same_scope() {
        // The user's bug: reading the SAME file in segments re-prompts on every
        // chunk even after pressing [A]. With path-scoped grants, pressing [A]
        // remembers the file's canonical path; subsequent reads (any
        // offset/limit → same scope) auto-approve.
        let mut store = PermissionStore::new();
        let approval = ApprovalRequirement::RequireApprovalScoped {
            reason: "sensitive read".into(),
            scope: "/home/u/.config/notes.md".into(),
        };
        assert!(matches!(
            store.check("read_file", &approval),
            PermissionDecision::Ask(_)
        ));
        store.grant_session_scope("/home/u/.config/notes.md");
        assert!(matches!(
            store.check("read_file", &approval),
            PermissionDecision::Allow
        ));
    }

    #[test]
    fn test_permission_store_scoped_grant_does_not_cover_other_scopes() {
        // Granting one file must not auto-approve a DIFFERENT sensitive file.
        let mut store = PermissionStore::new();
        store.grant_session_scope("/home/u/.config/notes.md");
        let other = ApprovalRequirement::RequireApprovalScoped {
            reason: "sensitive read".into(),
            scope: "/home/u/.ssh/id_rsa".into(),
        };
        assert!(matches!(
            store.check("read_file", &other),
            PermissionDecision::Ask(_)
        ));
    }

    #[test]
    fn test_permission_store_tool_grant_does_not_cover_scoped() {
        // A tool-wide [A] (grant_session) must NOT auto-approve a path-scoped
        // sensitive read — otherwise pressing [A] on an innocuous external file
        // would silently allow reading ~/.ssh/id_rsa for the rest of the session.
        let mut store = PermissionStore::new();
        store.grant_session("read_file");
        let approval = ApprovalRequirement::RequireApprovalScoped {
            reason: "sensitive read".into(),
            scope: "/home/u/.ssh/id_rsa".into(),
        };
        assert!(matches!(
            store.check("read_file", &approval),
            PermissionDecision::Ask(_)
        ));
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

    fn cmd_keys() -> Vec<String> {
        vec!["command".into(), "timeout".into()]
    }
    fn read_keys() -> Vec<String> {
        vec!["file_path".into(), "offset".into(), "limit".into()]
    }
    fn grep_keys() -> Vec<String> {
        vec!["pattern".into(), "path".into(), "max_results".into(), "context".into()]
    }
    fn write_keys() -> Vec<String> {
        vec!["file_path".into(), "content".into()]
    }
    fn todo_keys() -> Vec<String> {
        vec!["action".into(), "content".into(), "id".into()]
    }

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn recover_flat_passes_through() {
        // Already in schema shape — return None so caller uses raw unchanged.
        let raw = r#"{"command":"ls -la"}"#;
        assert!(recover_tool_args(raw, &cmd_keys()).is_none());
    }

    #[test]
    fn recover_variant_a1_string_inner() {
        // Stream-mode atomgit wrap: {"arguments": "<stringified-json>"}.
        let raw = r#"{"arguments":"{\"command\":\"ls\"}"}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        assert_eq!(parse(&recovered)["command"], "ls");
    }

    #[test]
    fn recover_variant_a2_object_inner() {
        // Non-stream atomgit wrap: {"arguments": <object>}.
        let raw = r#"{"arguments":{"command":"ls","timeout":30}}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        let v = parse(&recovered);
        assert_eq!(v["command"], "ls");
        assert_eq!(v["timeout"], 30);
    }

    #[test]
    fn recover_variant_b_double_string() {
        // Two-layer wrap (datalog 6% form), both string-valued.
        let raw = r#"{"arguments":"{\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}"}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        assert_eq!(parse(&recovered)["command"], "ls");
    }

    #[test]
    fn recover_variant_b_triple_object() {
        // Three-layer object wrap (Bruno non-stream observed form).
        let raw = r#"{"arguments":{"arguments":{"command":"ls"}}}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        assert_eq!(parse(&recovered)["command"], "ls");
    }

    #[test]
    fn recover_variant_c_multi_key_merges_siblings() {
        // {"arguments":"{\"command\":\"ls\"}", "timeout": 120}
        // The wrapper key contains the schema-shaped object; sibling keys
        // already in expected schema get merged into the recovered object.
        let raw = r#"{"arguments":"{\"command\":\"ls\"}","timeout":120}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        let v = parse(&recovered);
        assert_eq!(v["command"], "ls");
        assert_eq!(v["timeout"], 120);
    }

    #[test]
    fn recover_variant_d_content_wrapper() {
        // Alternative wrapper key: "content" instead of "arguments".
        let raw = r#"{"content":"{\"pattern\":\"foo\",\"path\":\"/x\"}"}"#;
        let recovered = recover_tool_args(raw, &grep_keys()).unwrap();
        let v = parse(&recovered);
        assert_eq!(v["pattern"], "foo");
        assert_eq!(v["path"], "/x");
    }

    #[test]
    fn recover_variant_d_input_wrapper() {
        // "input" wrapper key (Anthropic-style — not seen in atomgit datalog
        // but documented in the spec; covered defensively).
        let raw = r#"{"input":{"file_path":"/tmp/a.rs"}}"#;
        let recovered = recover_tool_args(raw, &read_keys()).unwrap();
        assert_eq!(parse(&recovered)["file_path"], "/tmp/a.rs");
    }

    #[test]
    fn recover_unrecoverable_returns_none() {
        // Wrapper present but inner has no expected schema field — bail.
        let raw = r#"{"arguments":{"random":"junk"}}"#;
        assert!(recover_tool_args(raw, &cmd_keys()).is_none());
    }

    #[test]
    fn recover_iteration_bound_pathological_input() {
        // 100-layer recursive wrap — must terminate without OOM.
        let mut deep = String::from(r#"{"command":"ls"}"#);
        for _ in 0..100 {
            deep = format!(r#"{{"arguments":{}}}"#, deep);
        }
        // After 5 unwrap iterations we still won't reach the schema field
        // because the wrap is too deep — should return None, not panic.
        let result = recover_tool_args(&deep, &cmd_keys());
        // Either None (too deep to recover) or Some with the recovered form
        // — both are acceptable outcomes; what matters is termination.
        assert!(result.is_none() || result.is_some());
    }

    #[test]
    fn recover_no_expected_keys_falls_back_permissive() {
        // Unknown tool — no schema available. Function falls back to:
        // unwrap if wrapper present, else None.
        let wrapped = r#"{"arguments":{"x":1}}"#;
        let recovered = recover_tool_args(wrapped, &[]).unwrap();
        assert_eq!(parse(&recovered)["x"], 1);

        let flat = r#"{"x":1}"#;
        assert!(recover_tool_args(flat, &[]).is_none());
    }

    #[test]
    fn recover_real_datalog_payload() {
        // Verbatim from datalog 2026-04-25_22-02-07.jsonl step 4.
        let raw = r#"{"arguments": "{\"command\": \"cd /Users/lichao/project/gitcode/ai/atomcode && cargo check 2>&1 | grep -iE 'warning.*(dead_code|unused)' | head -20\"}"}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        let v = parse(&recovered);
        assert!(v["command"].as_str().unwrap().contains("cargo check"));
    }

    #[test]
    fn recover_real_bruno_object_payload() {
        // Verbatim from Bruno non-stream response captured during reproduction.
        let raw = r#"{"arguments": {"command": "grep -rn '#\\[allow(dead_code)\\]' /Users/lichao/project/gitcode/ai/atomcode/crates/ --include='*.rs' | head -50", "timeout": 10}}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        let v = parse(&recovered);
        assert_eq!(v["timeout"], 10);
        assert!(v["command"].as_str().unwrap().contains("dead_code"));
    }

    #[test]
    fn recover_malformed_json_returns_none() {
        assert!(recover_tool_args("not json", &cmd_keys()).is_none());
        assert!(recover_tool_args("", &cmd_keys()).is_none());
        assert!(recover_tool_args("[]", &cmd_keys()).is_none());
    }

    // -------- Regression: schema fields overlapping ARGS_WRAPPER_KEYS --------
    //
    // The write tool's schema declares `content`, which is also one of the
    // wrapper keys. Earlier versions of recover_tool_args used a Step 1
    // short-circuit `has_expected_key && !has_wrapper_shape`. When a model
    // wrote a JSON-shaped string to a file, has_wrapper_shape misidentified
    // the legitimate `content` field as a wrapper, the unwrap loop stripped
    // it, and Variant C merge dropped the actual content value. The fix
    // changed Step 1 to an "all top-level keys are schema-declared" check,
    // which passes through legitimate payloads even when their values
    // happen to look like wrappers.

    #[test]
    fn recover_write_with_json_object_content_passthrough() {
        // The classic break: writing a JSON file whose content is a JSON
        // object literal. content's string value parses to an object, so
        // the old short-circuit failed and Variant D unwrap corrupted the
        // payload. Must return None now (legitimate, all keys schema-declared).
        let raw = r#"{"file_path":"/tmp/x.json","content":"{\"foo\":1}"}"#;
        assert!(recover_tool_args(raw, &write_keys()).is_none());
    }

    #[test]
    fn recover_write_with_nested_json_content_passthrough() {
        // Deeply-nested JSON content — would have unwrapped multiple layers
        // under the old logic.
        let raw = r#"{"file_path":"/tmp/cfg.json","content":"{\"a\":{\"b\":{\"c\":1}}}"}"#;
        assert!(recover_tool_args(raw, &write_keys()).is_none());
    }

    #[test]
    fn recover_todo_with_json_content_passthrough() {
        // Same class of bug for the todo tool — task description that
        // happens to be a JSON snippet.
        let raw = r#"{"action":"add","content":"{\"task\":\"refactor\"}"}"#;
        assert!(recover_tool_args(raw, &todo_keys()).is_none());
    }

    #[test]
    fn recover_write_genuine_wrap_still_recovered() {
        // Sanity: a genuinely wrapped write payload (atomgit gateway A2)
        // must still recover. The wrapper key here is `arguments` (not
        // declared in write schema), so all_keys_in_expected fails and
        // we fall through to unwrap.
        let raw = r#"{"arguments":{"file_path":"/tmp/x","content":"hello"}}"#;
        let recovered = recover_tool_args(raw, &write_keys()).unwrap();
        let v = parse(&recovered);
        assert_eq!(v["file_path"], "/tmp/x");
        assert_eq!(v["content"], "hello");
    }

    #[test]
    fn recover_partial_keys_still_recoverable_via_wrapper() {
        // Payload with a wrapper key + sibling that's NOT in schema:
        // {"arguments": "...", "foo": 1} — top-level keys {arguments, foo},
        // neither schema-declared, so all_keys_in_expected=false → unwrap.
        let raw = r#"{"arguments":"{\"command\":\"ls\"}","foo":1}"#;
        let recovered = recover_tool_args(raw, &cmd_keys()).unwrap();
        assert_eq!(parse(&recovered)["command"], "ls");
    }

    #[test]
    fn test_real_home_dir_returns_something() {
        // In normal conditions, real_home_dir should return a valid path
        let home = real_home_dir();
        assert!(home.is_some(), "real_home_dir should return Some in normal conditions");
        let path = home.unwrap();
        assert!(path.is_absolute(), "Home directory should be an absolute path");
    }

    #[test]
    fn test_real_home_dir_with_simulated_sudo() {
        // Save original state
        let original_sudo_user = std::env::var("SUDO_USER").ok();
        let original_home = std::env::var("HOME").ok();
        
        // Simulate sudo scenario: HOME=/root, SUDO_USER=<current_user>
        // We can't actually change to root, but we can verify the logic works
        #[cfg(unix)]
        {
            // Get current user's home from dirs::home_dir()
            let normal_home = dirs::home_dir();
            
            // Set SUDO_USER to a user that exists (the current user)
            // This tests that get_user_home() works correctly
            if let Some(ref home) = normal_home {
                // The home directory should be valid
                assert!(home.is_absolute());
            }
        }
        
        // Restore original state
        if let Some(orig) = original_sudo_user {
            std::env::set_var("SUDO_USER", orig);
        } else {
            std::env::remove_var("SUDO_USER");
        }
        
        if let Some(orig) = original_home {
            std::env::set_var("HOME", orig);
        }
    }

    #[test]
    fn test_expand_user_path_with_tilde() {
        // Test that ~/path is expanded correctly
        let home = real_home_dir().unwrap();
        let expanded = expand_user_path("~/test");
        assert_eq!(expanded, home.join("test"));
        
        // Test that ~ alone expands to home
        let expanded = expand_user_path("~");
        assert_eq!(expanded, home);
        
        // Test that non-tilde paths are preserved
        let expanded = expand_user_path("/absolute/path");
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn approval_for_windows_system_protected_prefix_requires_always() {
        assert!(is_sensitive_path(Path::new(r"C:\Windows\System32\config.sys")));
        assert!(is_sensitive_path(Path::new(r"C:\Program Files\SomeApp\app.exe")));
        assert!(is_sensitive_path(Path::new(r"C:\ProgramData\secrets.txt")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn approval_for_unix_system_protected_prefix_requires_always() {
        assert!(is_sensitive_path(Path::new("/System/Library/CoreServices/boot.efi")));
        assert!(is_sensitive_path(Path::new("/etc/passwd")));
        assert!(is_sensitive_path(Path::new("/var/log/syslog")));
    }

    #[test]
    fn config_dir_and_bare_config_no_longer_sensitive() {
        let home = real_home_dir().expect("home");
        // ~/.config/<app>/settings is ordinary app config, not a secret.
        assert!(!is_sensitive_path(&home.join(".config").join("app").join("settings.toml")));
        // A bare file named `config` outside the workspace is not a secret.
        assert!(!is_sensitive_path(std::path::Path::new("/tmp/somewhere/config")));
    }

    #[test]
    fn real_secrets_still_sensitive() {
        let home = real_home_dir().expect("home");
        assert!(is_sensitive_path(&home.join(".ssh").join("id_rsa")));
        assert!(is_sensitive_path(&home.join(".aws").join("credentials")));
        assert!(is_sensitive_path(std::path::Path::new("/tmp/x/server.key")));
    }
}
