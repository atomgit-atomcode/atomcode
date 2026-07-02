//! `SensitivePathGate` — require approval before a normally-Safe READ tool touches a
//! sensitive path (SSH keys, cloud creds, `.env`, …).
//!
//! v2's approval is risk-based: `read_file` / `grep` / `glob` / `list_dir` are `Safe`, so
//! they NEVER prompt — meaning an agent can silently read `~/.ssh/id_rsa` or `.env` and the
//! contents ride a tool result straight to the LLM provider (secret exfiltration). This
//! gate restores the per-path protection the legacy engine had, in v2's middleware idiom:
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

/// True if the raw args reference a sensitive path. `.env` is matched only as a FILENAME
/// (`.env"`, `.env'`, `.env.local…`) so `"environment"` / `.environment/` do not false-trip.
pub fn references_sensitive_path(args: &str) -> bool {
    let a = args.to_ascii_lowercase();
    if a.contains(".env\"") || a.contains(".env'") || a.contains(".env.") {
        return true;
    }
    SENSITIVE_MARKERS.iter().any(|m| a.contains(m))
}

/// The user's real home directory. Used to anchor `~/.ssh` / `~/.aws` / `~/.gnupg`
/// so a project-local `./.ssh/` (benign) is not treated like the real keys. Thin
/// alias over the crate-shared [`crate::pathutil::home_dir`] (single source of the
/// `HOME`/`USERPROFILE` logic).
fn home_dir() -> Option<PathBuf> {
    crate::pathutil::home_dir()
}

/// True iff a RESOLVED (absolute, cwd-joined) `path` is sensitive — a system-protected
/// location, a credential dir under the real home, or a secret file by name/extension. This is
/// the PATH-aware companion to [`references_sensitive_path`] (which substring-matches raw JSON
/// args): it correctly catches a RELATIVE `.ssh/authorized_keys` or a Windows `…\.ssh\…` once
/// resolved, which the substring form misses. Faithful port of the legacy (v1) `is_sensitive_path`
/// so write approval inherits the same protected set.
pub fn path_is_sensitive(path: &Path) -> bool {
    #[cfg(not(target_os = "windows"))]
    const SYSTEM_PROTECTED_PREFIXES: &[&str] = &[
        "/System", "/bin", "/sbin", "/usr", "/var", "/private/etc", "/private/var", "/etc",
        "/root", "/var/root", "/private/var/root",
    ];
    #[cfg(target_os = "windows")]
    const SYSTEM_PROTECTED_PREFIXES: &[&str] =
        &[r"C:\Windows", r"C:\Program Files", r"C:\Program Files (x86)", r"C:\ProgramData", r"C:\PerfLogs"];
    #[cfg(not(target_os = "windows"))]
    const SYSTEM_PROTECTED_EXCEPTIONS: &[&str] = &[
        "/usr/local", "/private/usr/local", "/Applications", "/Library", "/var/folders",
        "/private/var/folders", "/var/tmp", "/private/var/tmp",
    ];
    #[cfg(target_os = "windows")]
    const SYSTEM_PROTECTED_EXCEPTIONS: &[&str] = &[];
    const SECRET_HOME_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg"];
    const SECRET_FILE_NAMES: &[&str] = &[
        ".bashrc", ".bash_profile", ".zshrc", ".zprofile", ".zshenv", ".npmrc", ".pypirc", ".env",
        ".env.local", "credentials", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519",
    ];
    const SECRET_EXTS: &[&str] = &["pem", "key", "p12", "pfx", "der", "crt", "cer"];

    let has_protected_prefix =
        SYSTEM_PROTECTED_PREFIXES.iter().any(|p| path == Path::new(p) || path.starts_with(p));
    let has_exception_prefix =
        SYSTEM_PROTECTED_EXCEPTIONS.iter().any(|p| path == Path::new(p) || path.starts_with(p));
    if has_protected_prefix && !has_exception_prefix {
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

    if path.file_name().and_then(|n| n.to_str()).is_some_and(|name| SECRET_FILE_NAMES.contains(&name)) {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| SECRET_EXTS.iter().any(|c| ext.eq_ignore_ascii_case(c)))
}

/// Require approval before an otherwise-`Safe` tool reads a sensitive path.
pub struct SensitivePathGate {
    store: Arc<dyn PermissionStore>,
    kind: String,
}

impl Default for SensitivePathGate {
    fn default() -> Self {
        Self { store: Arc::new(InMemoryPermissionStore::new()), kind: APPROVAL_KIND.to_string() }
    }
}

impl SensitivePathGate {
    pub fn new() -> Self {
        Self::default()
    }
    /// Use a caller-supplied (e.g. shared / persisted) grant store.
    pub fn with_store(store: Arc<dyn PermissionStore>) -> Self {
        Self { store, kind: APPROVAL_KIND.to_string() }
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
        if !references_sensitive_path(&call.arguments) {
            return BeforeOutcome::Proceed;
        }
        // Distinct key namespace so a "sensitive-read always" grant never silently widens
        // an ordinary approval grant (and vice versa).
        let key = format!("sensitive::{}::{}", call.name, call.arguments);
        if self.store.is_granted(&key) {
            return BeforeOutcome::Proceed;
        }
        let payload = serde_json::to_value(ApprovalRequest {
            call_id: call.id.clone(),
            tool: tool.name().to_string(),
            args: call.arguments.clone(),
        })
        .unwrap_or(serde_json::Value::Null);
        match PermissionDecision::from_value(&rt.request(&self.kind, payload).await) {
            PermissionDecision::AllowOnce => BeforeOutcome::Proceed,
            PermissionDecision::AllowAlways => {
                self.store.grant(&key);
                BeforeOutcome::Proceed
            }
            PermissionDecision::Deny => BeforeOutcome::deny(format!(
                "reading a sensitive path needs approval and was denied: {} {}",
                tool.name(),
                call.arguments
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn detects_credential_paths_not_ordinary_content() {
        // Credential stores → flagged.
        assert!(references_sensitive_path(r#"{"file_path":"/home/u/.ssh/id_rsa"}"#));
        assert!(references_sensitive_path(r#"{"file_path":"/home/u/.ssh"}"#), "the .ssh dir too");
        assert!(references_sensitive_path(r#"{"file_path":"/proj/.env"}"#));
        assert!(references_sensitive_path(r#"{"file_path":"/proj/.env.local"}"#));
        assert!(references_sensitive_path(r#"{"path":"/home/u/.aws/credentials"}"#));
        assert!(references_sensitive_path(r#"{"file_path":"/etc/ssl/server.pem"}"#));
        assert!(references_sensitive_path(r#"{"file_path":"C:\\Users\\u\\.ssh\\id_ed25519"}"#), "windows key");
        // Ordinary reads / searches → NOT flagged.
        assert!(!references_sensitive_path(r#"{"file_path":"src/main.rs"}"#));
        assert!(!references_sensitive_path(r#"{"pattern":"secret","path":"src/"}"#), "grep word 'secret'");
        assert!(!references_sensitive_path(r#"{"path":"/proj/.environment/cfg"}"#), "no .env false-trip");
    }

    fn silent_rt() -> RequestCtx {
        // No driver drains the request → a bounded round-trip times out → Null → Deny.
        let (tx, _rx) = unbounded_channel();
        RequestCtx::new(tx, Some(Duration::from_millis(20)))
    }

    #[tokio::test]
    async fn safe_ordinary_read_passes_without_round_trip() {
        let gate = SensitivePathGate::new();
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool::default());
        let mut call =
            ToolCall { id: "1".into(), name: "read_file".into(), arguments: r#"{"file_path":"src/main.rs"}"#.into() };
        // Ordinary path → Proceed WITHOUT awaiting the (silent) driver.
        assert!(!gate.before(&mut call, &tool, &silent_rt()).await.is_deny());
    }

    #[tokio::test]
    async fn risky_tool_defers_to_approval_middleware() {
        // A Risky tool is ApprovalMiddleware's job; this gate must skip it (no double-prompt)
        // even if its args look sensitive.
        let gate = SensitivePathGate::new();
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::write::WriteFileTool);
        let mut call = ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: r#"{"file_path":"/home/u/.ssh/authorized_keys","content":"x"}"#.into(),
        };
        assert!(!gate.before(&mut call, &tool, &silent_rt()).await.is_deny());
    }

    #[tokio::test]
    async fn sensitive_read_fails_closed_when_driver_silent() {
        let gate = SensitivePathGate::new();
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool::default());
        let mut call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"file_path":"/home/u/.ssh/id_rsa"}"#.into(),
        };
        let res = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(res.is_deny(), "a sensitive read with no approval must fail closed");
        assert!(res.deny_reason().unwrap().contains("sensitive path"));
    }
}
