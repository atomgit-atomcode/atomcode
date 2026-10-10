//! `SensitivePathGate` — require approval before a normally-Safe READ tool touches a
//! sensitive path (SSH keys, cloud creds, `.env`, …).
//!
//! Kernel approval is risk-based: `read_file` / `grep` / `glob` / `list_dir` are `Safe`, so
//! they NEVER prompt — meaning an agent can silently read `~/.ssh/id_rsa` or `.env` and the
//! contents ride a tool result straight to the LLM provider (secret exfiltration). This
//! gate preserves the existing per-path protection in a native middleware:
//! it acts ONLY on tools that would otherwise bypass approval (`Safe`) AND whose args name
//! a sensitive path, then runs the SAME approval round-trip as [`ApprovalMiddleware`]
//! (allow-once / allow-always / deny). `Risky` tools already go through approval, so this
//! never double-prompts; `-y` / auto-approve drivers answer it like any approval.
//!
//! [`ApprovalMiddleware`]: super::approval::ApprovalMiddleware

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall};

use super::approval::{
    ApprovalRequest, InMemoryPermissionStore, PermissionDecision, PermissionStore, APPROVAL_KIND,
};

/// Path fragments that mark a credential store. Matched case-insensitively as substrings of
/// the raw (JSON) tool arguments — the path rides there for every read tool. Deliberately
/// PATH-shaped (not bare words like "secret") so an ordinary `grep "secret"` over source
/// does not prompt. A false positive costs ONE approval prompt on an otherwise-Safe read,
/// so the list errs toward catching real secrets. `.env` is handled specially below.
const SENSITIVE_MARKERS: &[&str] = &[
    "/.ssh",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    "/.aws",
    "/.gnupg",
    "/.kube",
    "/.config/gcloud",
    ".netrc",
    ".git-credentials",
    "/.docker/config",
    ".npmrc",
    ".pypirc",
    ".pem",
    ".p12",
    ".pfx",
    ".keystore",
    "/secrets/",
    "/.terraform.d",
];

/// Placeholder-template `.env` variants committed to version control — they hold only
/// dummy values, so reading them is not a secret-exfiltration risk and must not prompt.
/// Matched as the keyword immediately after `.env.` (e.g. `.env.example`, `.env.sample`).
const ENV_TEMPLATE_SUFFIXES: &[&str] = &["example", "sample", "template", "dist", "defaults"];

/// What counts as a sensitive path: the fixed lists below, plus the credential
/// stores of the user tree this process was handed.
///
/// The tree is handed in, never looked up. It used to be `$ATOMCODE_HOME` read once
/// into a process-wide static, plus a literal `/.atomcode` spelling — so a build
/// that named its tree anything else guarded a path that did not exist, and a
/// program embedding these tools could not say where its own credentials were.
#[derive(Clone, Debug)]
pub struct SensitivePaths {
    user_dir: PathBuf,
    /// [`HOME_CREDENTIAL_STORES`] as lowercased `/`-separated substrings, twice
    /// over: under the tree as configured, and under `/<home dir name>` — the
    /// `~/<name>/auth.toml` spelling a model writes from habit, which is worth a
    /// prompt wherever the tree was moved to.
    markers: Arc<[String]>,
}

impl SensitivePaths {
    /// The guard for a product's dirs: its user tree, and its tree's default name.
    pub fn of(dirs: &crate::ProductDirs) -> Self {
        Self::new(dirs.user(), dirs.home_dir_name())
    }

    /// Guard the credential stores of the user tree `user_dir`. `home_dir_name`
    /// is the tree's default name under a home (the host's
    /// `distribution::HOME_DIR_NAME`), so its `~/<name>/…` spelling is caught too.
    pub fn new(user_dir: impl Into<PathBuf>, home_dir_name: &str) -> Self {
        let user_dir = user_dir.into();
        let mut markers = credential_markers_for(&user_dir);
        if !home_dir_name.is_empty() {
            markers.extend(credential_markers_for(&Path::new("/").join(home_dir_name)));
        }
        Self {
            user_dir,
            markers: markers.into(),
        }
    }

    /// True if the raw args reference a sensitive path. `.env` is matched only as a FILENAME
    /// (`.env"`, `.env'`, `.env.local…`) so `"environment"` / `.environment/` do not false-trip.
    /// Placeholder templates (`.env.example`, `.env.sample`, …) are excluded — they are
    /// committed to VCS and hold no real secrets, so prompting on them is pure friction.
    pub fn references(&self, args: &str) -> bool {
        let a = args.to_ascii_lowercase();
        // Bare `.env` filename (quoted in the JSON args).
        if a.contains(".env\"") || a.contains(".env'") {
            return true;
        }
        // `.env.<suffix>` is sensitive (`.env.local`, `.env.production`, …) UNLESS every
        // such occurrence is a known non-secret template.
        if env_dot_reference_is_sensitive(&a) {
            return true;
        }
        if self.matches_a_marker(&a) {
            return true;
        }
        // Raw JSON doubles Windows path separators. Decode string values, normalize their
        // separators, then apply the same path-shaped markers to the actual argument bytes.
        // This avoids maintaining a fragile second marker list for JSON escaping.
        serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .is_some_and(|value| self.decoded_json_references(&value))
    }

    /// Check file targets in the same world and directory the tool uses. Only
    /// path arguments are resolved: search patterns, URLs and query text are
    /// not filesystem paths. The raw check still covers shell commands.
    pub async fn references_in(
        &self,
        args: &str,
        working_dir: &Path,
        world: &dyn crate::world::FileSystem,
    ) -> bool {
        if self.references(args) {
            return true;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(args) else {
            return false;
        };
        let mut real_user: Option<Option<PathBuf>> = None;
        for key in ["file_path", "path"] {
            let Some(raw) = value.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            let path = super::resolve_path(raw, working_dir);
            let named = serde_json::json!({ "path": path }).to_string();
            if self.references(&named) {
                return true;
            }
            // A world that refuses resolution also refuses the tool's access.
            // Missing targets still received the lexical check above.
            if let Ok(real) = world.canonicalize(&path).await {
                if self.references(&serde_json::json!({ "path": real }).to_string()) {
                    return true;
                }
                let user = match &real_user {
                    Some(user) => user,
                    None => real_user.insert(world.canonicalize(&self.user_dir).await.ok()),
                };
                if user
                    .as_ref()
                    .is_some_and(|user| is_credential_path(&real, user))
                {
                    return true;
                }
            }
        }
        false
    }

    /// File grants follow the resolved target rather than a relative spelling
    /// that can name another secret after changing directory or a symlink.
    pub async fn file_target_scope(
        &self,
        args: &str,
        working_dir: &Path,
        world: &dyn crate::world::FileSystem,
    ) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(args).ok()?;
        let mut targets = Vec::new();
        for key in ["file_path", "path"] {
            if let Some(raw) = value.get(key).and_then(|v| v.as_str()) {
                let path = super::resolve_path(raw, working_dir);
                let real = world.canonicalize(&path).await.unwrap_or(path);
                targets.push(crate::pathnorm::to_display(&real));
            }
        }
        (!targets.is_empty()).then(|| targets.join("\u{1f}"))
    }

    /// [`SENSITIVE_MARKERS`] plus the credential stores of the user tree.
    fn matches_a_marker(&self, lowercased: &str) -> bool {
        SENSITIVE_MARKERS.iter().any(|m| lowercased.contains(m))
            || self.markers.iter().any(|m| lowercased.contains(m.as_str()))
    }

    fn decoded_json_references(&self, value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(value) => {
                let normalized = value.to_ascii_lowercase().replace('\\', "/");
                self.matches_a_marker(&normalized)
            }
            serde_json::Value::Array(values) => {
                values.iter().any(|v| self.decoded_json_references(v))
            }
            serde_json::Value::Object(values) => {
                values.values().any(|v| self.decoded_json_references(v))
            }
            _ => false,
        }
    }

    /// True iff `path` is one of the [`HOME_CREDENTIAL_STORES`] under the user tree.
    pub(crate) fn is_credential_path(&self, path: &Path) -> bool {
        is_credential_path(path, &self.user_dir)
    }

    /// The user tree whose stores this guards.
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }
}

/// The part of a call this gate's grant is keyed on: the TARGET it names, not the raw
/// argument bytes. Reading a second WINDOW of the same file (`offset`/`limit`) is the same
/// decision the user already answered — keying on raw args made it re-prompt per window. It
/// stays PER TARGET, so a grant for one secret never covers a different one. A call naming no
/// target falls back to the raw arguments (fail-closed: nothing is widened).
fn grant_scope(args: &str) -> String {
    let targets = super::target_arg_values(args);
    if targets.is_empty() {
        return args.to_string();
    }
    targets.join("\u{1f}")
}

/// The AtomCode home's credential stores, named relative to it. A trailing `/`
/// marks a directory.
///
/// - `auth.toml`, `auth/`: the login store.
/// - `config.toml`: every `api_key` a person wrote in plain — `[provider_accounts.*]`,
///   `[providers.*]`, `[web_search]`. It used to be absent here and even pinned as an
///   ordinary file, so a `read_file` of it was `Safe` and asked nobody; on 2026-09-16 a
///   session read it to describe the settings, and a turn later repeated a provider key
///   to the person who asked for it — by then the key had already gone to the model
///   provider and into the session log.
/// - `mcp_auth.toml`: MCP OAuth tokens.
///
/// A file matches with anything after its name as well: `config.toml.bak` and
/// `config.toml.bak-before-…` are copies of the store and hold the same keys.
///
/// The uninstaller classifies the same files as credentials
/// (`atomcode-cli/src/uninstall/paths.rs`); `mcp.json` is on its list but not on
/// this one, because the agent reads it to help configure MCP servers.
const HOME_CREDENTIAL_STORES: &[&str] = &["auth.toml", "auth/", "config.toml", "mcp_auth.toml"];

/// [`HOME_CREDENTIAL_STORES`] under `config_dir`, as lowercased `/`-separated substrings.
fn credential_markers_for(config_dir: &Path) -> Vec<String> {
    let dir = config_dir
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('\\', "/");
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return Vec::new();
    }
    HOME_CREDENTIAL_STORES
        .iter()
        .map(|store| format!("{dir}/{store}"))
        .collect()
}

/// Scan every `.env.<suffix>` occurrence in the lowercased args; return true if any suffix
/// is NOT a recognized template keyword (i.e. a real secret variant like `local`/`production`).
fn env_dot_reference_is_sensitive(a: &str) -> bool {
    let mut rest = a;
    while let Some(pos) = rest.find(".env.") {
        let after = &rest[pos + ".env.".len()..];
        // Leading alphanumeric run is the variant keyword (stops at quote, dot, slash, …).
        let suffix: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !ENV_TEMPLATE_SUFFIXES.contains(&suffix.as_str()) {
            return true;
        }
        rest = after;
    }
    false
}

/// The user's real home directory. Used to anchor `~/.ssh` / `~/.aws` / `~/.gnupg`
/// so a project-local `./.ssh/` (benign) is not treated like the real keys. Thin
/// alias over the crate-shared [`crate::pathutil::home_dir`] (single source of the
/// `HOME`/`USERPROFILE` logic).
fn home_dir() -> Option<PathBuf> {
    crate::pathutil::home_dir()
}

/// True iff `path` is one of the [`HOME_CREDENTIAL_STORES`] under `config_dir`.
///
/// Anchored on the tree handed in rather than a literal name: with the tree
/// relocated, a literal guarded a path that does not exist while the real
/// `auth.toml` stayed unguarded.
pub(crate) fn is_credential_path(path: &Path, config_dir: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(config_dir) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    HOME_CREDENTIAL_STORES
        .iter()
        .any(|store| match store.strip_suffix('/') {
            Some(dir) => relative == dir || relative.starts_with(store),
            // A file, or a copy of it (`config.toml.bak`) — but not a nested
            // file of the same name, which `starts_with` on the relative path
            // already rules out.
            None => relative.starts_with(store),
        })
}

impl SensitivePaths {
    /// True iff a RESOLVED (absolute, cwd-joined) `path` is sensitive — a system-protected
    /// location, a credential store of the user tree, a credential dir under the real home, or a
    /// secret file by name/extension. This is the PATH-aware companion to [`Self::references`]
    /// (which substring-matches raw JSON args): it correctly catches a RELATIVE
    /// `.ssh/authorized_keys` or a Windows `…\.ssh\…` once resolved, which the substring form
    /// misses. Faithful port of the legacy (v1) `is_sensitive_path` so write approval inherits the
    /// same protected set.
    pub fn path_is_sensitive(&self, path: &Path) -> bool {
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
            .any(|p| path == Path::new(p) || path.starts_with(p));
        let has_exception_prefix = SYSTEM_PROTECTED_EXCEPTIONS
            .iter()
            .any(|p| path == Path::new(p) || path.starts_with(p));
        if has_protected_prefix && !has_exception_prefix {
            return true;
        }

        if self.is_credential_path(path) {
            return true;
        }

        if let Some(home) = home_dir() {
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
            .and_then(|e| e.to_str())
            .is_some_and(|ext| SECRET_EXTS.iter().any(|c| ext.eq_ignore_ascii_case(c)))
    }
}

/// The guard this crate's unit tests use: a temp user tree, with this product's
/// default name for the `~/<name>/…` spelling (existing expectations name it).
#[cfg(test)]
pub(crate) fn test_guard() -> SensitivePaths {
    SensitivePaths::new(crate::product_dirs::test_dirs().user(), ".atomcode")
}

/// Require approval before an otherwise-`Safe` tool reads a sensitive path.
pub struct SensitivePathGate {
    store: Arc<dyn PermissionStore>,
    kind: String,
    sensitive: SensitivePaths,
}

impl SensitivePathGate {
    pub fn new(sensitive: SensitivePaths) -> Self {
        Self::with_store(Arc::new(InMemoryPermissionStore::new()), sensitive)
    }
    /// Use a caller-supplied (e.g. shared / persisted) grant store.
    pub fn with_store(store: Arc<dyn PermissionStore>, sensitive: SensitivePaths) -> Self {
        Self {
            store,
            kind: APPROVAL_KIND.to_string(),
            sensitive,
        }
    }
}

#[async_trait]
impl ToolMiddleware for SensitivePathGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        // Only tools that would otherwise SKIP approval need this — a Risky tool already
        // round-trips through ApprovalMiddleware, so gating it here would double-prompt.
        if tool.risk(&call.arguments) != RiskLevel::Safe {
            return BeforeOutcome::Proceed;
        }
        if !self.sensitive.references(&call.arguments) {
            return BeforeOutcome::Proceed;
        }
        // Distinct key namespace so a "sensitive-read always" grant never silently widens
        // an ordinary approval grant (and vice versa).
        //
        // Keyed on the TARGET the call names, not the raw argument bytes: reading a second
        // WINDOW of the same file (`offset`/`limit`) is the same decision the user already
        // answered, and keying on raw args made it re-prompt per window. It stays per-target,
        // so a grant for one secret never covers a different one. No target ⇒ raw args.
        let key = format!("sensitive::{}::{}", call.name, grant_scope(&call.arguments));
        if self.store.is_granted(&key) {
            return BeforeOutcome::Proceed;
        }
        let payload = serde_json::to_value(ApprovalRequest {
            call_id: call.id.clone(),
            tool: tool.name().to_string(),
            args: call.arguments.clone(),
            reason: None,
            allow_all_bash: false,
        })
        .unwrap_or(serde_json::Value::Null);
        match PermissionDecision::from_value(&rt.request(&self.kind, payload).await) {
            PermissionDecision::AllowOnce => BeforeOutcome::Proceed,
            PermissionDecision::AllowAlways | PermissionDecision::AllowAlwaysAll => {
                self.store.grant(&key);
                BeforeOutcome::Proceed
            }
            PermissionDecision::Deny => BeforeOutcome::deny(format!(
                "reading a sensitive path needs approval and was denied: {}",
                tool.name()
            )),
        }
    }
}

#[cfg(test)]
mod grant_scope_tests {
    use super::grant_scope;
    use serde_json::json;

    /// A second window over the SAME secret is the same decision — it must not re-prompt.
    /// (Before this, the key was the raw argument bytes, so every `offset` change asked again;
    /// a driver-side tool-wide cache used to hide that, and hid far too much else besides.)
    #[test]
    fn same_target_different_window_shares_a_grant() {
        let a = json!({ "file_path": "~/.ssh/config", "offset": 1, "limit": 50 }).to_string();
        let b = json!({ "file_path": "~/.ssh/config", "offset": 51, "limit": 50 }).to_string();
        assert_eq!(grant_scope(&a), grant_scope(&b));
    }

    /// …but a DIFFERENT secret is a different decision and must ask again.
    #[test]
    fn a_different_target_does_not_share_a_grant() {
        let a = json!({ "file_path": "~/.ssh/id_rsa" }).to_string();
        let b = json!({ "file_path": "~/.aws/credentials" }).to_string();
        assert_ne!(grant_scope(&a), grant_scope(&b));
    }

    /// No target field ⇒ fall back to the raw arguments; nothing is widened.
    #[test]
    fn no_target_falls_back_to_raw_arguments() {
        let raw = json!({ "whatever": "x" }).to_string();
        assert_eq!(grant_scope(&raw), raw);
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn relative_file_targets_use_the_execution_directory() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(user.join("config.toml"), "FAKE_KEY").unwrap();
        std::fs::write(project.join("config.toml"), "ordinary").unwrap();
        let guard = super::SensitivePaths::new(&user, ".ours");
        let world = crate::world::LocalFs::unfenced();
        let args = r#"{"file_path":"config.toml"}"#;
        assert!(guard.references_in(args, &user, &world).await);
        assert!(
            guard
                .references_in(r#"{"path":"../user/config.toml"}"#, &project, &world)
                .await
        );
        assert!(!guard.references_in(args, &project, &world).await);
        assert!(
            !guard
                .references_in(r#"{"pattern":"config.toml"}"#, &user, &world)
                .await
        );
        assert!(
            !guard
                .references_in(r#"{"file_path":".env.example"}"#, &project, &world)
                .await
        );
        assert_ne!(
            guard.file_target_scope(args, &user, &world).await,
            guard.file_target_scope(args, &project, &world).await
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_targets_and_a_symlinked_user_tree_are_guarded() {
        let temp = tempfile::tempdir().unwrap();
        let real_user = temp.path().join("user");
        std::fs::create_dir_all(&real_user).unwrap();
        std::fs::write(real_user.join("config.toml"), "FAKE_KEY").unwrap();
        let user = temp.path().join("user-alias");
        std::os::unix::fs::symlink(&real_user, &user).unwrap();
        std::os::unix::fs::symlink(
            real_user.join("config.toml"),
            temp.path().join("settings.txt"),
        )
        .unwrap();
        std::fs::write(temp.path().join(".env.production"), "FAKE_ENV").unwrap();
        std::os::unix::fs::symlink(
            temp.path().join(".env.production"),
            temp.path().join(".env.example"),
        )
        .unwrap();
        let guard = super::SensitivePaths::new(&user, ".ours");
        // This is the world's production mode: reads unfenced, writes fenced.
        let world = crate::world::LocalFs::writes_fenced(temp.path());
        assert!(
            guard
                .references_in(r#"{"file_path":"settings.txt"}"#, temp.path(), &world)
                .await
        );
        assert!(
            guard
                .references_in(r#"{"file_path":".env.example"}"#, temp.path(), &world)
                .await
        );
        let args = serde_json::json!({"file_path": real_user.join("config.toml")}).to_string();
        assert!(guard.references_in(&args, temp.path(), &world).await);
        assert_eq!(
            guard
                .file_target_scope(r#"{"file_path":"settings.txt"}"#, temp.path(), &world)
                .await,
            guard.file_target_scope(&args, temp.path(), &world).await
        );
    }

    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc::unbounded_channel;

    /// The guard as this product's host builds it for a person whose tree sits
    /// at the default place.
    fn guard() -> SensitivePaths {
        SensitivePaths::new("/home/u/.atomcode", ".atomcode")
    }

    fn references_sensitive_path(args: &str) -> bool {
        guard().references(args)
    }

    #[test]
    fn detects_credential_paths_not_ordinary_content() {
        // Credential stores → flagged.
        assert!(references_sensitive_path(
            r#"{"file_path":"/home/u/.ssh/id_rsa"}"#
        ));
        assert!(
            references_sensitive_path(r#"{"file_path":"/home/u/.ssh"}"#),
            "the .ssh dir too"
        );
        assert!(references_sensitive_path(r#"{"file_path":"/proj/.env"}"#));
        assert!(references_sensitive_path(
            r#"{"file_path":"/proj/.env.local"}"#
        ));
        assert!(
            references_sensitive_path(r#"{"file_path":"/proj/.env.production"}"#),
            "real secret variant"
        );
        assert!(references_sensitive_path(
            r#"{"file_path":"/home/u/.atomcode/auth.toml"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"command":"cat ~/.atomcode/auth.toml"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"file_path":"C:\\Users\\u\\.atomcode\\auth.toml"}"#
        ));
        // The config file holds every plain `api_key`, and a copy of it holds the same.
        assert!(references_sensitive_path(
            r#"{"file_path":"/Users/u/.atomcode/config.toml"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"file_path":"~/.atomcode/config.toml.bak-before-permissions-cleanup"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"pattern":"api_key","path":"/Users/u/.atomcode/config.toml"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"file_path":"C:\\Users\\u\\.atomcode\\config.toml"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"file_path":"/Users/u/.atomcode/mcp_auth.toml"}"#
        ));
        // …but the rest of the home is not a credential store, and neither is a
        // project's own `config.toml`.
        assert!(!references_sensitive_path(
            r#"{"file_path":"/Users/u/.atomcode/memory.md"}"#
        ));
        assert!(!references_sensitive_path(
            r#"{"file_path":"/Users/u/.atomcode/plugins/x/config.toml"}"#
        ));
        assert!(!references_sensitive_path(
            r#"{"file_path":"/proj/.cargo/config.toml"}"#
        ));
        // Placeholder templates (committed to VCS, no real secrets) → NOT flagged.
        assert!(
            !references_sensitive_path(r#"{"file_path":"/proj/.env.example"}"#),
            ".env.example is a template"
        );
        assert!(!references_sensitive_path(
            r#"{"file_path":"/proj/.env.sample"}"#
        ));
        assert!(!references_sensitive_path(
            r#"{"file_path":"/proj/.env.template"}"#
        ));
        assert!(!references_sensitive_path(
            r#"{"file_path":"/proj/.env.dist"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"path":"/home/u/.aws/credentials"}"#
        ));
        assert!(references_sensitive_path(
            r#"{"file_path":"/etc/ssl/server.pem"}"#
        ));
        assert!(
            references_sensitive_path(r#"{"file_path":"C:\\Users\\u\\.ssh\\id_ed25519"}"#),
            "windows key"
        );
        // Ordinary reads / searches → NOT flagged.
        assert!(!references_sensitive_path(r#"{"file_path":"src/main.rs"}"#));
        assert!(
            !references_sensitive_path(r#"{"pattern":"secret","path":"src/"}"#),
            "grep word 'secret'"
        );
        assert!(
            !references_sensitive_path(r#"{"path":"/proj/.environment/cfg"}"#),
            "no .env false-trip"
        );
    }

    fn silent_rt() -> RequestCtx {
        // No driver drains the request → a bounded round-trip times out → Null → Deny.
        let (tx, _rx) = unbounded_channel();
        RequestCtx::new(tx, Some(Duration::from_millis(20)))
    }

    #[tokio::test]
    async fn safe_ordinary_read_passes_without_round_trip() {
        let gate = SensitivePathGate::new(guard());
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool::default());
        let mut call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"file_path":"src/main.rs"}"#.into(),
        };
        // Ordinary path → Proceed WITHOUT awaiting the (silent) driver.
        assert!(!gate.before(&mut call, &tool, &silent_rt()).await.is_deny());
    }

    #[tokio::test]
    async fn risky_tool_defers_to_approval_middleware() {
        // A Risky tool is ApprovalMiddleware's job; this gate must skip it (no double-prompt)
        // even if its args look sensitive.
        let gate = SensitivePathGate::new(guard());
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::write::WriteFileTool::default());
        let mut call = ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: r#"{"file_path":"/home/u/.ssh/authorized_keys","content":"x"}"#.into(),
        };
        assert!(!gate.before(&mut call, &tool, &silent_rt()).await.is_deny());
    }

    #[tokio::test]
    async fn sensitive_read_fails_closed_when_driver_silent() {
        let gate = SensitivePathGate::new(guard());
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool::default());
        let mut call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"file_path":"/home/u/.ssh/id_rsa"}"#.into(),
        };
        let res = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            res.is_deny(),
            "a sensitive read with no approval must fail closed"
        );
        assert!(res.deny_reason().unwrap().contains("sensitive path"));
    }

    /// The credential guard follows `$ATOMCODE_HOME`. Driven through the pure
    /// cores so no test has to mutate the process-global env (libtest runs these
    /// in parallel threads, and the crate's `#[ctor]` already owns that var).
    #[test]
    fn the_credential_guard_follows_a_relocated_config_dir() {
        let moved = Path::new("/opt/ac");
        assert!(is_credential_path(Path::new("/opt/ac/auth.toml"), moved));
        assert!(is_credential_path(
            Path::new("/opt/ac/auth/token.json"),
            moved
        ));
        // The default location is NOT special-cased: with the tree moved, that
        // path is an ordinary file. `SENSITIVE_MARKERS` still covers the raw-arg
        // spelling — see `the_default_credential_markers_survive_relocation`.
        assert!(!is_credential_path(
            Path::new("/home/u/.atomcode/auth.toml"),
            moved
        ));
        // A copy of a store is a store: `auth.toml.bak` holds the same tokens.
        // A name that merely starts like the credential DIR is not it.
        assert!(is_credential_path(
            Path::new("/opt/ac/auth.toml.bak"),
            moved
        ));
        assert!(!is_credential_path(Path::new("/opt/ac/authors"), moved));

        // The config file carries plain `api_key`s; its hand-made copies carry them too.
        assert!(is_credential_path(Path::new("/opt/ac/config.toml"), moved));
        assert!(is_credential_path(
            Path::new("/opt/ac/config.toml.bak-before-permissions-cleanup"),
            moved
        ));
        assert!(is_credential_path(
            Path::new("/opt/ac/mcp_auth.toml"),
            moved
        ));
        // Only at the top of the home: a plugin's own `config.toml` is not the store.
        assert!(!is_credential_path(
            Path::new("/opt/ac/plugins/x/config.toml"),
            moved
        ));
        assert!(!is_credential_path(Path::new("/opt/ac/memory.md"), moved));
        assert!(!is_credential_path(
            Path::new("/elsewhere/config.toml"),
            moved
        ));

        let default = Path::new("/home/u/.atomcode");
        assert!(is_credential_path(
            Path::new("/home/u/.atomcode/auth.toml"),
            default
        ));
    }

    #[test]
    fn markers_are_derived_from_the_configured_dir() {
        assert_eq!(
            credential_markers_for(Path::new("/opt/AC")),
            vec![
                "/opt/ac/auth.toml".to_string(),
                "/opt/ac/auth/".to_string(),
                "/opt/ac/config.toml".to_string(),
                "/opt/ac/mcp_auth.toml".to_string(),
            ],
            "lowercased so it matches the lowercased args"
        );
        // Windows dirs reach the matcher `/`-normalized, like every other marker.
        assert_eq!(
            credential_markers_for(Path::new(r"C:\ac"))[0],
            "c:/ac/auth.toml"
        );
        // A trailing separator must not double up.
        assert_eq!(
            credential_markers_for(Path::new("/opt/ac/"))[0],
            "/opt/ac/auth.toml"
        );
        assert!(credential_markers_for(Path::new("")).is_empty());
    }

    /// End-to-end through the guard's own entry point, for a tree moved off the
    /// default: only the markers derived from the tree handed in can match it.
    #[test]
    fn a_relocated_credential_path_is_flagged_in_raw_args() {
        let dir = Path::new("/opt/relocated-tree");
        let guard = SensitivePaths::new(dir, ".atomcode");

        let auth = dir.join("auth.toml");
        let args = serde_json::json!({ "file_path": auth.to_string_lossy() }).to_string();
        assert!(
            guard.references(&args),
            "credentials at the configured location must gate a Safe read: {args}"
        );

        let token = dir.join("auth").join("token.json");
        let args = serde_json::json!({ "command": format!("cat {}", token.display()) }).to_string();
        assert!(guard.references(&args), "{args}");

        let config = dir.join("config.toml");
        let args = serde_json::json!({ "file_path": config.to_string_lossy() }).to_string();
        assert!(
            guard.references(&args),
            "the relocated config file holds the api keys too: {args}"
        );

        // Same tree, ordinary file → still no prompt. Pins that the markers are
        // path-shaped and did not widen into "anything under the config dir".
        let ordinary = dir.join("memory.md");
        let args = serde_json::json!({ "file_path": ordinary.to_string_lossy() }).to_string();
        assert!(!guard.references(&args), "{args}");
    }

    /// A distribution's own name is what is guarded — its stores are caught in
    /// both spellings — and the upstream name confers nothing.
    #[test]
    fn a_renamed_distribution_guards_its_own_name_and_not_upstreams() {
        let guard = SensitivePaths::new("/home/u/.longcode", ".longcode");
        assert!(guard.references(r#"{"file_path":"/home/u/.longcode/auth.toml"}"#));
        assert!(guard.references(r#"{"command":"cat ~/.longcode/config.toml"}"#));
        assert!(guard.path_is_sensitive(Path::new("/home/u/.longcode/mcp_auth.toml")));
        assert!(!guard.references(r#"{"file_path":"/home/u/.atomcode/auth.toml"}"#));
    }

    /// Relocating the tree must not stop flagging the default spelling: a model
    /// writes `~/.atomcode/auth.toml` from habit, and that string is still worth
    /// a prompt wherever the tree was moved.
    #[test]
    fn the_default_credential_markers_survive_relocation() {
        let guard = SensitivePaths::new("/opt/relocated-tree", ".atomcode");
        assert!(guard.references(r#"{"file_path":"/home/u/.atomcode/auth.toml"}"#));
        assert!(guard.references(r#"{"command":"cat ~/.atomcode/auth/token.json"}"#));
    }
}
