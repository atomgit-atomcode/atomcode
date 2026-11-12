//! Explicit desktop MCP setup and read-only diagnostics. No live agent owner.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use atomcode_capabilities::mcp::{
    merge_stdio_mcp_server_into_text, set_mcp_server_disabled_in_text,
};
use atomcode_capabilities::setup::{fs_atomic::atomic_write, lock::SetupLock};
use atomcode_capabilities::ProductDirs;
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

const SCOPE_ENV: &str = "OPEN_COMPUTER_USE_ALLOWED_BUNDLE_IDS";
const SCOPE_CAPABILITY: &str = "atomcode/application-scope";

const SERVER: &str = "computer-use";
const OWNER: &str = "atomcode-computer-use-v1";
const SKILL: &str = include_str!("../assets/computer-use/SKILL.md");
/// Byte identity of the shipped d8be8b9bf skill; only this exact older body may
/// be upgraded. Local edits (including auto-load opt-out) stay protected.
const SHIPPED_V1_SKILL_SHA256: &str =
    "c5adc72d2559d6c52d7b880e152e48d0598d8d37abc92099015c8799e022be91";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: u64 = 16 * 1024;
const BLOCKED_OPT_INS: &[&str] = &[
    "OPEN_COMPUTER_USE_ALLOW_GLOBAL_POINTER_FALLBACKS",
    "OPEN_COMPUTER_USE_MACOS_ALLOW_FOREGROUND_INPUT",
    "OPEN_COMPUTER_USE_WINDOWS_ALLOW_FOREGROUND_INPUT",
    "OPEN_COMPUTER_USE_WINDOWS_ALLOW_APP_LAUNCH",
    "OPEN_COMPUTER_USE_WINDOWS_ALLOW_FOCUS_ACTIONS",
];

#[derive(Clone, Subcommand)]
pub enum ComputerCli {
    /// Enable an installed local desktop MCP after a non-interactive permission check.
    Enable {
        #[arg(long)]
        executable: Option<PathBuf>,
        #[command(flatten)]
        scope: ApplicationScopeArgs,
    },
    /// Show configuration without capturing the desktop or changing files.
    Status,
    /// Check backend permissions without requesting access or opening onboarding.
    Doctor {
        #[arg(long)]
        executable: Option<PathBuf>,
    },
    /// Disable the managed entry for new/reloaded sessions (not an emergency stop).
    Disable,
}

/// Explicit setup intent; omitted options preserve the saved policy.
#[derive(Clone, Debug, Default, Args)]
pub struct ApplicationScopeArgs {
    /// Restrict to exact bundle IDs (repeatable); requires the patched backend.
    #[arg(long = "allow-app", conflicts_with_all = ["deny_all_apps", "unrestricted_apps"])]
    pub allowed_apps: Vec<String>,
    /// Deny every application, including unknown applications.
    #[arg(long, conflicts_with = "unrestricted_apps")]
    pub deny_all_apps: bool,
    /// Explicitly remove the saved application restriction (not OS/backend denials).
    #[arg(long)]
    pub unrestricted_apps: bool,
}

impl ApplicationScopeArgs {
    fn apply(&self, saved: Option<Vec<String>>) -> Result<Option<Vec<String>>> {
        let modes = usize::from(!self.allowed_apps.is_empty())
            + usize::from(self.deny_all_apps)
            + usize::from(self.unrestricted_apps);
        if modes > 1 {
            bail!("application scope options are mutually exclusive");
        }
        if self.unrestricted_apps {
            return Ok(None);
        }
        if self.deny_all_apps {
            return Ok(Some(Vec::new()));
        }
        if !self.allowed_apps.is_empty() {
            return Ok(Some(normalize_bundle_ids(self.allowed_apps.clone())?));
        }
        Ok(saved)
    }
}

fn normalize_bundle_ids(mut ids: Vec<String>) -> Result<Vec<String>> {
    if ids.len() > 256 {
        bail!("application scope permits at most 256 bundle IDs");
    }
    for id in &mut ids {
        if id.len() > 255
            || !id.contains('.')
            || !id.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            })
        {
            bail!("application scope requires exact bundle IDs such as com.apple.TextEdit; no names, wildcards or paths");
        }
        id.make_ascii_lowercase();
    }
    ids.sort();
    ids.dedup();
    if serde_json::to_vec(&ids)?.len() > 65536 {
        bail!("application scope exceeds the backend JSON size limit");
    }
    Ok(ids)
}

fn saved_scope(entry: Option<&Value>) -> Result<Option<Vec<String>>> {
    let Some(raw) = entry
        .and_then(|entry| entry.get("env"))
        .and_then(|env| env.get(SCOPE_ENV))
    else {
        return Ok(None);
    };
    let raw = raw
        .as_str()
        .context("saved application scope must be a JSON-array string")?;
    if raw.len() > 65536 {
        bail!("saved application scope is too large");
    }
    Ok(Some(normalize_bundle_ids(
        serde_json::from_str(raw).context("invalid saved application scope")?,
    )?))
}

fn describe_scope(scope: Option<&[String]>) -> String {
    match scope {
        None => "Application scope: not restricted by managed configuration (host environment and backend denials still apply).".into(),
        Some([]) => "Application scope: deny all applications.".into(),
        Some(ids) => format!("Application scope: {}", ids.join(", ")),
    }
}

#[derive(Debug, Deserialize)]
struct Permissions {
    accessibility: bool,
    screen_recording: bool,
}
/// Backend-reported display context, never an authorization decision.
#[derive(Debug, Deserialize)]
struct RuntimeIdentity {
    permission_app: Option<String>,
    bundle_identifier: Option<String>,
}
#[derive(Debug, Deserialize)]
struct Probe {
    schema_version: u32,
    kind: String,
    ui: bool,
    permissions: Permissions,
    runtime: Option<RuntimeIdentity>,
    application_scope: Option<Value>,
}
impl Probe {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let probe: Self = serde_json::from_slice(bytes)
            .context("backend did not return a supported non-interactive permission report")?;
        if probe.schema_version != 1 || probe.kind != "doctor" || probe.ui {
            bail!("unsupported backend diagnostic schema; use Open Computer Use 1.3.0 with doctor --no-ui");
        }
        if let Some(app) = probe
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.permission_app.as_deref())
        {
            if !Path::new(app).is_absolute() {
                bail!("backend permission App identity must be an absolute path");
            }
        }
        Ok(probe)
    }
    fn permission_identity(&self) -> String {
        match self.runtime.as_ref().and_then(|runtime| runtime.permission_app.as_deref()) {
            Some(app) => {
                let mut hint = format!("Permission App (reported by backend): {}", app.escape_debug());
                if let Some(identifier) = self.runtime.as_ref().and_then(|runtime| runtime.bundle_identifier.as_deref()) {
                    hint.push_str(&format!("\nBundle identifier (reported by backend): {}", identifier.escape_debug()));
                }
                hint
            }
            None => "Permission App: not identified by this backend. Run its local doctor to identify the App before changing OS permissions.".into(),
        }
    }
    fn describe(&self, backend: &Path) -> String {
        format!(
            "Backend: {}\n{}\nAccessibility: {}\nScreen Recording: {}",
            backend.to_string_lossy().escape_debug(),
            self.permission_identity(),
            self.permissions.accessibility,
            self.permissions.screen_recording
        )
    }
    fn require_scope(&self, scope: Option<&[String]>) -> Result<()> {
        if scope.is_some()
            && self
                .application_scope
                .as_ref()
                .and_then(|value| value.get("version"))
                .and_then(Value::as_u64)
                != Some(1)
        {
            bail!("application scope requires a backend reporting application_scope version 1; no configuration was changed");
        }
        Ok(())
    }
    fn require_ready(&self) -> Result<()> {
        if !self.permissions.accessibility || !self.permissions.screen_recording {
            bail!("Computer Use permissions incomplete: Accessibility={}, Screen Recording={}.\n{}\nGrant the identified backend App access in macOS settings, then retry; another App's grant does not apply. If this App is already enabled after a rebuild, remove its old permission entry and add the current App again: macOS may retain the previous signing requirement.", self.permissions.accessibility, self.permissions.screen_recording, self.permission_identity());
        }
        Ok(())
    }
}

pub async fn handle(command: ComputerCli) -> Result<()> {
    let dirs = atomcode_coding::config::product_dirs_from_env();
    println!("{}", execute(&dirs, command).await?);
    Ok(())
}

/// Shared local service: return display text, never print into a running TUI.
/// Mutations affect configuration only; the existing Reload command owns live state.
pub async fn execute(dirs: &ProductDirs, command: ComputerCli) -> Result<String> {
    match command {
        ComputerCli::Status => {
            let text = read_optional(&config_path(dirs))?;
            let entry = owned_entry(text.as_deref())?;
            let state = match entry.as_ref() {
                None => "not configured",
                Some(entry) if entry.get("disabled").and_then(Value::as_bool) == Some(true) => {
                    "disabled in configuration"
                }
                Some(_) => "enabled in configuration (live connection not queried)",
            };
            let scope = describe_scope(saved_scope(entry.as_ref())?.as_deref());
            Ok(format!("Computer Use: {state}\n{scope}\nBackend: Open Computer Use; supported integration: macOS 14+.\nInstall: npm install -g @opensymph/open-computer-use@1.3.0\nCheck: atomcode computer doctor; enable: atomcode computer enable\nUse /mcp to inspect the current session; project overrides and MCP pause still apply."))
        }
        ComputerCli::Doctor { executable } => {
            require_platform()?;
            let backend = backend_for(dirs, executable.as_deref())?;
            let report = probe(&backend, PROBE_TIMEOUT).await?;
            report.require_ready()?;
            Ok(report.describe(&backend))
        }
        ComputerCli::Enable { executable, scope } => {
            require_platform()?;
            let backend = backend_for(dirs, executable.as_deref())?;
            let policy =
                enable_after_probe_with_scope(dirs, &backend, PROBE_TIMEOUT, &scope).await?;
            Ok(format!("{}\nComputer Use configured with native MCP approvals. Restart AtomCode or run /computer reload, then describe a desktop task or use $computer-use explicitly. Screenshot tasks need a vision-capable model; automatic skill loading grants no tool permissions.", describe_scope(policy.as_deref())))
        }
        ComputerCli::Disable => {
            disable(dirs)?;
            Ok("Computer Use disabled in configuration. Run /computer reload in running sessions; already dispatched operations are not interrupted by this command.".into())
        }
    }
}

#[cfg(target_os = "macos")]
fn require_platform() -> Result<()> {
    let output = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .stdin(Stdio::null())
        .output()?;
    let major = std::str::from_utf8(&output.stdout)?
        .trim()
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if !output.status.success() || major < 14 {
        bail!("this integration requires macOS 14 or later");
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn require_platform() -> Result<()> {
    bail!("this Computer Use integration currently supports macOS 14 or later only")
}
fn config_path(dirs: &ProductDirs) -> PathBuf {
    dirs.user().join("mcp.json")
}
fn skill_path(dirs: &ProductDirs) -> PathBuf {
    dirs.user().join("skills/computer-use/SKILL.md")
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            bail!("refusing non-regular file: {}", path.display())
        }
        Ok(_) => Ok(Some(std::fs::read_to_string(path)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn owned_entry(text: Option<&str>) -> Result<Option<Value>> {
    let root: Value = match text {
        Some(text) => serde_json::from_str(&atomcode_capabilities::jsonc::strip_comments(text))?,
        None => json!({}),
    };
    if !root.is_object() {
        bail!("MCP config must be an object");
    }
    let mut found = None;
    for key in ["servers", "mcpServers"] {
        if let Some(map) = root.get(key) {
            let map = map
                .as_object()
                .context("MCP server map must be an object")?;
            if let Some(entry) = map.get(SERVER) {
                if entry.get("managedBy").and_then(Value::as_str) != Some(OWNER) {
                    bail!("MCP server 'computer-use' already exists outside this integration; inspect /mcp instead of overwriting it");
                }
                if found.is_some() {
                    bail!(
                        "duplicate managed computer-use entries; keep a single entry before setup"
                    );
                }
                found = Some(entry.clone());
            }
        }
    }
    Ok(found)
}
fn resolve_backend(path: &Path) -> Result<PathBuf> {
    let found = which::which(path).context("Open Computer Use is not installed or executable; install @opensymph/open-computer-use@1.3.0 or pass --executable")?;
    Ok(std::fs::canonicalize(found)?)
}
fn backend_for(dirs: &ProductDirs, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return resolve_backend(path);
    }
    let entry = owned_entry(read_optional(&config_path(dirs))?.as_deref())?;
    let saved = entry
        .as_ref()
        .and_then(|entry| entry.get("command"))
        .and_then(Value::as_str);
    resolve_backend(Path::new(saved.unwrap_or("open-computer-use")))
}
pub(crate) fn write_checked(path: &Path, before: &Option<String>, text: &str) -> Result<()> {
    if &read_optional(path)? != before {
        bail!("{} changed; retry without overwriting it", path.display());
    }
    atomic_write(path, text.as_bytes(), 0o600)
}

/// Managed desktop/browser setup edits the same MCP document. Serialize both
/// integrations; optimistic text checks still detect non-cooperating edits.
pub(crate) fn lock_mcp_setup(dirs: &ProductDirs) -> Result<SetupLock> {
    Ok(SetupLock::acquire(
        &dirs.user().join("integrations/mcp-setup"),
        false,
    )?)
}
#[cfg(test)]
async fn enable_after_probe(dirs: &ProductDirs, backend: &Path, deadline: Duration) -> Result<()> {
    enable_after_probe_with_scope(dirs, backend, deadline, &ApplicationScopeArgs::default())
        .await?;
    Ok(())
}

async fn enable_after_probe_with_scope(
    dirs: &ProductDirs,
    backend: &Path,
    deadline: Duration,
    change: &ApplicationScopeArgs,
) -> Result<Option<Vec<String>>> {
    let before = read_optional(&config_path(dirs))?;
    let prior = owned_entry(before.as_deref())?;
    // Explicit replacement can repair a malformed saved scope. Preservation must
    // understand it and cannot quietly discard it.
    let saved =
        if change.unrestricted_apps || change.deny_all_apps || !change.allowed_apps.is_empty() {
            None
        } else {
            saved_scope(prior.as_ref())?
        };
    let scope = change.apply(saved)?;
    let report = probe(backend, deadline).await?;
    report.require_ready()?;
    report.require_scope(scope.as_deref())?;
    enable_with_scope(
        dirs,
        backend,
        scope.as_deref(),
        change.unrestricted_apps,
        Some(&before),
    )?;
    Ok(scope)
}

fn is_bundled_skill(text: &str) -> bool {
    text == SKILL || format!("{:x}", Sha256::digest(text.as_bytes())) == SHIPPED_V1_SKILL_SHA256
}

#[cfg(test)]
fn enable(dirs: &ProductDirs, backend: &Path) -> Result<()> {
    let entry = owned_entry(read_optional(&config_path(dirs))?.as_deref())?;
    enable_with_scope(
        dirs,
        backend,
        saved_scope(entry.as_ref())?.as_deref(),
        false,
        None,
    )
}

fn enable_with_scope(
    dirs: &ProductDirs,
    backend: &Path,
    scope: Option<&[String]>,
    clear_scope_requirement: bool,
    expected: Option<&Option<String>>,
) -> Result<()> {
    let _lock = lock_mcp_setup(dirs)?;
    let config = config_path(dirs);
    let before = read_optional(&config)?;
    if expected.is_some_and(|expected| expected != &before) {
        bail!("MCP configuration changed during permission probe; retry without overwriting it");
    }
    let prior = owned_entry(before.as_deref())?;
    let skill = skill_path(dirs);
    let prior_skill = read_optional(&skill)?;
    if prior_skill
        .as_deref()
        .is_some_and(|text| !is_bundled_skill(text))
    {
        bail!("computer-use skill has local changes; preserve or rename it before enabling");
    }
    let merged = merge_stdio_mcp_server_into_text(
        before.as_deref(),
        SERVER,
        backend.to_str().context("backend path must be UTF-8")?,
        &["mcp".into()],
    )?;
    let mut root: Value = serde_json::from_str(&merged)?;
    let entry = &mut root["mcpServers"][SERVER];
    if let Some(prior) = prior {
        *entry = prior;
    }
    entry["command"] = json!(backend.to_str().context("backend path must be UTF-8")?);
    entry["args"] = json!(["mcp"]);
    entry["managedBy"] = json!(OWNER);
    entry["disabled"] = json!(false);
    entry["trust"] = json!(false);
    entry["autoApprove"] = json!([]);
    entry["timeout_ms"] = json!(30_000);
    let mut env = match entry.get("env") {
        None => serde_json::Map::new(),
        Some(value) => value
            .as_object()
            .context("managed MCP env must be an object")?
            .clone(),
    };
    if env.values().any(|value| !value.is_string()) {
        bail!("managed MCP env values must be strings");
    }
    for flag in BLOCKED_OPT_INS {
        env.insert((*flag).into(), json!("0"));
    }
    if let Some(ids) = scope {
        env.insert(SCOPE_ENV.into(), json!(serde_json::to_string(ids)?));
    } else {
        env.remove(SCOPE_ENV);
    }
    entry["env"] = Value::Object(env);
    if entry.get("requiredExperimentalCapabilities").is_some()
        && entry.get("required_experimental_capabilities").is_some()
    {
        bail!("duplicate required capabilities fields in managed MCP configuration");
    }
    let mut required: std::collections::BTreeMap<String, u64> = match entry
        .get("requiredExperimentalCapabilities")
        .or_else(|| entry.get("required_experimental_capabilities"))
    {
        Some(value) => {
            serde_json::from_value(value.clone()).context("invalid saved required capabilities")?
        }
        None => Default::default(),
    };
    if scope.is_some() {
        if required
            .get(SCOPE_CAPABILITY)
            .is_some_and(|version| *version != 1)
        {
            bail!("saved application scope capability version conflicts with supported version 1; inspect configuration before replacing it");
        }
        required.insert(SCOPE_CAPABILITY.into(), 1);
    } else if clear_scope_requirement {
        required.remove(SCOPE_CAPABILITY);
    }
    entry
        .as_object_mut()
        .context("managed MCP entry must be an object")?
        .remove("required_experimental_capabilities");
    if !required.is_empty() {
        entry["requiredExperimentalCapabilities"] = json!(required);
    } else {
        entry
            .as_object_mut()
            .context("managed MCP entry must be an object")?
            .remove("requiredExperimentalCapabilities");
    }
    // Validate only the edited entry, not unrelated servers. This also enforces
    // generic requirement bounds before any product file is written.
    atomcode_capabilities::mcp::parse_mcp_servers(
        &json!({"mcpServers":{SERVER:entry.clone()}}).to_string(),
        atomcode_capabilities::mcp::McpConfigSource::User,
    )?;
    write_checked(&skill, &prior_skill, SKILL)?;
    // Retain the identical skill if the config commit is uncertain. Deleting
    // it could undo part of a commit that actually reached disk.
    write_checked(
        &config,
        &before,
        &format!("{}\n", serde_json::to_string_pretty(&root)?),
    )
}
fn disable(dirs: &ProductDirs) -> Result<()> {
    let path = config_path(dirs);
    let before = read_optional(&path)?;
    if owned_entry(before.as_deref())?.is_none() {
        return Ok(());
    }
    let _lock = lock_mcp_setup(dirs)?;
    let after = set_mcp_server_disabled_in_text(
        before
            .as_deref()
            .context("managed MCP configuration disappeared")?,
        SERVER,
        true,
    )?;
    write_checked(&path, &before, &after)?;
    let skill = skill_path(dirs);
    if read_optional(&skill)?
        .as_deref()
        .is_some_and(is_bundled_skill)
    {
        std::fs::remove_file(skill)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) struct ProbeGroup(pub(crate) Option<i32>);
#[cfg(unix)]
impl Drop for ProbeGroup {
    fn drop(&mut self) {
        if let Some(group) = self.0.take() {
            // The probe starts its own process group; kill wrappers and their
            // children on deadline/cancellation, never another runtime's group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}
async fn read_probe_output<R: tokio::io::AsyncRead + Unpin>(reader: R) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        bail!("backend diagnostic output exceeded 16 KiB");
    }
    Ok(bytes)
}
async fn probe(backend: &Path, deadline: Duration) -> Result<Probe> {
    let mut command = tokio::process::Command::new(backend);
    command
        .args(["doctor", "--no-ui"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for flag in BLOCKED_OPT_INS {
        command.env(flag, "0");
    }
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .context("could not start the Computer Use permission probe")?;
    #[cfg(unix)]
    let mut group = ProbeGroup(child.id().and_then(|id| i32::try_from(id).ok()));
    let stdout = child.stdout.take().context("missing probe stdout")?;
    let stderr = child.stderr.take().context("missing probe stderr")?;
    let operation = async {
        let (stdout, _) = tokio::try_join!(read_probe_output(stdout), read_probe_output(stderr))?;
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>((status, stdout))
    };
    let result = tokio::time::timeout(deadline, operation).await;
    match result {
        Ok(Ok((status, stdout))) => {
            #[cfg(unix)]
            {
                group.0 = None;
            }
            if !status.success() {
                bail!("backend permission probe failed; no configuration was changed");
            }
            Probe::parse(&stdout)
        }
        other => {
            #[cfg(unix)]
            drop(group);
            let _ = child.kill().await;
            match other {
                Ok(Err(error)) => Err(error),
                _ => anyhow::bail!(
                    "backend permission probe timed out; no configuration was changed"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READY: &str = r#"{"schema_version":1,"kind":"doctor","ui":false,"permissions":{"accessibility":true,"screen_recording":true}}"#;

    fn tree() -> (tempfile::TempDir, ProductDirs) {
        let temp = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(temp.path().join("user"), ".test-product");
        (temp, dirs)
    }
    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn config(dirs: &ProductDirs) -> Value {
        serde_json::from_str(&read_optional(&config_path(dirs)).unwrap().unwrap()).unwrap()
    }
    fn configured(dirs: &ProductDirs) {
        enable(dirs, Path::new("/test/desktop backend")).unwrap();
    }

    #[test]
    fn scope_ids_are_exact_bounded_normalized_and_modes_are_exclusive() {
        let ids = normalize_bundle_ids(vec![
            "com.apple.TextEdit".into(),
            "COM.APPLE.TEXTEDIT".into(),
        ])
        .unwrap();
        assert_eq!(ids, vec!["com.apple.textedit"]);
        for id in [
            "TextEdit",
            "*",
            "com.apple.*",
            "com..app",
            ".com.app",
            "com.app.",
            "com.app/name",
            "com.app\n",
            "com.你好",
        ] {
            assert!(normalize_bundle_ids(vec![id.into()]).is_err(), "{id}");
        }
        assert!(normalize_bundle_ids(vec!["com.app".into(); 257]).is_err());
        assert!(normalize_bundle_ids(
            (0..256)
                .map(|i| format!("com.{i:03}{}", "a".repeat(248)))
                .collect()
        )
        .is_err());
        assert!(normalize_bundle_ids(vec![format!("com.{}", "a".repeat(252))]).is_err());
        let args = ApplicationScopeArgs {
            allowed_apps: vec!["com.app".into()],
            deny_all_apps: true,
            unrestricted_apps: false,
        };
        assert!(args.apply(None).is_err());
        assert_eq!(
            ApplicationScopeArgs::default()
                .apply(Some(ids.clone()))
                .unwrap(),
            Some(ids)
        );
    }

    #[test]
    fn scoped_enable_preserves_policy_and_only_explicit_unrestriction_removes_it() {
        let (_temp, dirs) = tree();
        configured(&dirs);
        let mut root = config(&dirs);
        let entry = &mut root["mcpServers"][SERVER];
        entry["env"][SCOPE_ENV] = json!("[\"com.apple.TextEdit\"]");
        entry["env"]["CUSTOM_SETTING"] = json!("keep-private");
        entry["required_experimental_capabilities"] = json!({"vendor/another":2});
        entry["allowedTools"] = json!(["observe"]);
        put(&config_path(&dirs), &root.to_string());
        configured(&dirs);
        let root = config(&dirs);
        let entry = &root["mcpServers"][SERVER];
        assert_eq!(entry["env"][SCOPE_ENV], "[\"com.apple.textedit\"]");
        assert_eq!(entry["env"]["CUSTOM_SETTING"], "keep-private");
        assert_eq!(entry["allowedTools"], json!(["observe"]));
        assert_eq!(
            entry["requiredExperimentalCapabilities"],
            json!({SCOPE_CAPABILITY:1,"vendor/another":2})
        );
        assert!(entry.get("required_experimental_capabilities").is_none());
        disable(&dirs).unwrap();
        configured(&dirs);
        assert_eq!(
            config(&dirs)["mcpServers"][SERVER]["env"][SCOPE_ENV],
            "[\"com.apple.textedit\"]"
        );
        enable_with_scope(&dirs, Path::new("/test/backend"), Some(&[]), false, None).unwrap();
        assert_eq!(config(&dirs)["mcpServers"][SERVER]["env"][SCOPE_ENV], "[]");
        assert!(describe_scope(Some(&[])).contains("deny all"));
        enable_with_scope(&dirs, Path::new("/test/backend"), None, true, None).unwrap();
        let root = config(&dirs);
        let entry = &root["mcpServers"][SERVER];
        assert!(entry["env"].get(SCOPE_ENV).is_none());
        assert_eq!(
            entry["requiredExperimentalCapabilities"],
            json!({"vendor/another":2})
        );
        assert_eq!(entry["env"]["CUSTOM_SETTING"], "keep-private");
    }

    #[test]
    fn plain_enable_retains_requirements_even_without_an_environment_scope() {
        let (_temp, dirs) = tree();
        configured(&dirs);
        let mut root = config(&dirs);
        root["mcpServers"][SERVER]["requiredExperimentalCapabilities"] =
            json!({SCOPE_CAPABILITY:1,"vendor/other":3});
        put(&config_path(&dirs), &root.to_string());
        configured(&dirs);
        assert_eq!(
            config(&dirs)["mcpServers"][SERVER]["requiredExperimentalCapabilities"],
            json!({SCOPE_CAPABILITY:1,"vendor/other":3})
        );
    }

    #[test]
    fn conflicting_or_ambiguous_saved_policies_are_not_silently_replaced() {
        for required in [
            json!({SCOPE_CAPABILITY:2}),
            json!({"vendor/bad":0}),
            json!(null),
        ] {
            let (_temp, dirs) = tree();
            configured(&dirs);
            let mut root = config(&dirs);
            root["mcpServers"][SERVER]["requiredExperimentalCapabilities"] = required;
            let before = root.to_string();
            put(&config_path(&dirs), &before);
            assert!(
                enable_with_scope(&dirs, Path::new("/replacement"), Some(&[]), false, None)
                    .is_err()
            );
            assert_eq!(read_optional(&config_path(&dirs)).unwrap().unwrap(), before);
        }
        let duplicate = json!({"servers":{SERVER:{"managedBy":OWNER}},"mcpServers":{SERVER:{"managedBy":OWNER}}});
        assert!(owned_entry(Some(&duplicate.to_string())).is_err());
    }

    #[test]
    fn configuration_changed_during_probe_is_not_overwritten() {
        let (_temp, dirs) = tree();
        configured(&dirs);
        let before = read_optional(&config_path(&dirs)).unwrap();
        let changed = before.as_ref().unwrap().replace("30000", "40000");
        put(&config_path(&dirs), &changed);
        assert!(enable_with_scope(
            &dirs,
            Path::new("/replacement"),
            Some(&[]),
            false,
            Some(&before)
        )
        .is_err());
        assert_eq!(
            read_optional(&config_path(&dirs)).unwrap().unwrap(),
            changed
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn scoped_probe_requires_typed_version_before_writing_and_checks_preserved_policy() {
        tokio::time::resume(); // Real subprocess I/O, as with the existing probe tests.
        let (temp, dirs) = tree();
        let args = ApplicationScopeArgs {
            allowed_apps: vec!["com.apple.TextEdit".into()],
            ..Default::default()
        };
        for declaration in [
            json!(null),
            json!({}),
            json!({"version":true}),
            json!({"version":"1"}),
            json!({"version":1.0}),
            json!({"version":2}),
        ] {
            let mut report: Value = serde_json::from_str(READY).unwrap();
            report["application_scope"] = declaration;
            let backend = fake_backend(temp.path(), &format!("printf '%s' '{report}'"));
            assert!(
                enable_after_probe_with_scope(&dirs, &backend, PROBE_TIMEOUT, &args)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("application_scope version 1")
            );
            assert!(!dirs.user().exists());
        }
        let mut report: Value = serde_json::from_str(READY).unwrap();
        report["application_scope"] = json!({"version":1});
        let backend = fake_backend(temp.path(), &format!("printf '%s' '{report}'"));
        enable_after_probe_with_scope(&dirs, &backend, PROBE_TIMEOUT, &args)
            .await
            .unwrap();
        let before = read_optional(&config_path(&dirs)).unwrap();
        let legacy = fake_backend(temp.path(), &format!("printf '%s' '{READY}'"));
        assert!(enable_after_probe(&dirs, &legacy, PROBE_TIMEOUT)
            .await
            .is_err());
        assert_eq!(read_optional(&config_path(&dirs)).unwrap(), before);
        let clear = ApplicationScopeArgs {
            unrestricted_apps: true,
            ..Default::default()
        };
        enable_after_probe_with_scope(&dirs, &legacy, PROBE_TIMEOUT, &clear)
            .await
            .unwrap();
        assert!(config(&dirs)["mcpServers"][SERVER]["env"]
            .get(SCOPE_ENV)
            .is_none());
    }

    #[test]
    fn permission_report_is_strict_and_requires_both_permissions() {
        Probe::parse(READY.as_bytes())
            .unwrap()
            .require_ready()
            .unwrap();
        for invalid in [
            READY.replace("\"ui\":false", "\"ui\":true"),
            READY.replace("\"schema_version\":1", "\"schema_version\":2"),
            READY.replace("\"doctor\"", "\"capture\""),
            READY.replace("\"accessibility\":true", "\"accessibility\":\"true\""),
            "{}".into(),
        ] {
            assert!(Probe::parse(invalid.as_bytes()).is_err());
        }
        for permission in ["accessibility", "screen_recording"] {
            let missing = READY.replace(
                &format!("\"{permission}\":true"),
                &format!("\"{permission}\":false"),
            );
            assert!(Probe::parse(missing.as_bytes())
                .unwrap()
                .require_ready()
                .is_err());
        }
    }

    #[test]
    fn reported_permission_app_is_display_context_and_never_grants_permissions() {
        let app = std::env::temp_dir().join("Open Computer Use (AtomCode Lease).app");
        let mut report: Value = serde_json::from_str(READY).unwrap();
        report["runtime"] = json!({
            "permission_app": app, "bundle_identifier": "com.ifuryst.opencomputeruse.dev",
            "unrelated_secret": "do-not-print-this",
        });
        let ready = Probe::parse(&serde_json::to_vec(&report).unwrap()).unwrap();
        let displayed = ready.describe(Path::new("backend"));
        assert!(displayed.contains(&app.to_string_lossy().escape_debug().to_string()));
        assert!(displayed.contains("com.ifuryst.opencomputeruse.dev"));
        assert!(!displayed.contains("do-not-print-this"));
        report["permissions"]["accessibility"] = json!(false);
        let error = Probe::parse(&serde_json::to_vec(&report).unwrap())
            .unwrap()
            .require_ready()
            .unwrap_err()
            .to_string();
        assert!(error.contains("permissions incomplete") && error.contains("Accessibility=false"));
        assert!(error.contains("previous signing requirement"));
        assert!(error.contains(&app.to_string_lossy().escape_debug().to_string()));
        assert!(!error.contains("do-not-print-this"));
        assert!(Probe::parse(READY.as_bytes())
            .unwrap()
            .describe(Path::new("legacy"))
            .contains("not identified"));
    }

    #[test]
    fn permission_identity_is_typed_absolute_and_safe_for_terminal_text() {
        let mut report: Value = serde_json::from_str(READY).unwrap();
        for invalid in [
            json!(42),
            json!({"permission_app": 42}),
            json!({"permission_app": "relative.app"}),
            json!({"permission_app": ""}),
        ] {
            report["runtime"] = invalid;
            assert!(Probe::parse(&serde_json::to_vec(&report).unwrap()).is_err());
        }
        let app = std::env::temp_dir().join("unexpected\n\u{1b}[31m.app");
        report["runtime"] = json!({"permission_app": app, "bundle_identifier": "id\n\u{1b}[31m"});
        let displayed = Probe::parse(&serde_json::to_vec(&report).unwrap())
            .unwrap()
            .permission_identity();
        assert!(!displayed.contains('\u{1b}'));
        assert_eq!(displayed.lines().count(), 2);
        assert!(displayed.contains("\\n"));
    }

    #[test]
    fn enable_preserves_other_servers_and_is_idempotent() {
        let (_temp, dirs) = tree();
        put(
            &config_path(&dirs),
            r#"{"otherSetting":42,"servers":{"legacy":{"command":"keep-me"}},"mcpServers":{"other":{"command":"also-keep","env":{"TOKEN":"private"}}}}"#,
        );
        configured(&dirs);
        let first = config(&dirs);
        let servers = &first["mcpServers"];
        assert_eq!(servers["legacy"]["command"], "keep-me");
        assert_eq!(servers["other"]["env"]["TOKEN"], "private");
        assert_eq!(first["otherSetting"], 42);
        let entry = &servers[SERVER];
        assert_eq!(entry["managedBy"], OWNER);
        assert_eq!(entry["command"], "/test/desktop backend");
        assert_eq!(entry["args"], json!(["mcp"]));
        assert_eq!(entry["trust"], false);
        assert_eq!(entry["autoApprove"], json!([]));
        assert_eq!(entry["disabled"], false);
        for flag in BLOCKED_OPT_INS {
            assert_eq!(entry["env"][flag], "0");
        }
        assert_eq!(
            read_optional(&skill_path(&dirs)).unwrap().as_deref(),
            Some(SKILL)
        );
        configured(&dirs);
        assert_eq!(config(&dirs), first);
    }

    #[test]
    fn unmanaged_server_in_either_map_is_never_overwritten() {
        for root in [
            json!({"mcpServers":{SERVER:{"command":"custom"}}}),
            json!({"servers":{SERVER:{"command":"custom"}},"mcpServers":{SERVER:{"managedBy":OWNER}}}),
        ] {
            let (_temp, dirs) = tree();
            let before = root.to_string();
            put(&config_path(&dirs), &before);
            assert!(enable(&dirs, Path::new("/test/backend")).is_err());
            assert!(disable(&dirs).is_err());
            assert_eq!(read_optional(&config_path(&dirs)).unwrap().unwrap(), before);
            assert!(!skill_path(&dirs).exists());
        }
    }

    #[test]
    fn shipped_skill_can_upgrade_without_overwriting_local_edits() {
        const OLD: &str = include_str!("../tests/fixtures/computer-use-v1.md");
        assert!(is_bundled_skill(OLD));
        assert_ne!(OLD, SKILL);
        let (_temp, dirs) = tree();
        configured(&dirs);
        put(&skill_path(&dirs), OLD);
        configured(&dirs);
        assert_eq!(
            read_optional(&skill_path(&dirs)).unwrap().as_deref(),
            Some(SKILL)
        );
        for changed in [
            format!("{OLD}\nLocal instructions"),
            OLD.replace("auto-load: computer-use", "auto-load: off"),
        ] {
            put(&skill_path(&dirs), &changed);
            let before = read_optional(&config_path(&dirs)).unwrap();
            assert!(enable(&dirs, Path::new("/replacement/backend")).is_err());
            assert_eq!(
                read_optional(&skill_path(&dirs)).unwrap().as_deref(),
                Some(changed.as_str())
            );
            assert_eq!(read_optional(&config_path(&dirs)).unwrap(), before);
        }
    }

    #[test]
    fn disable_removes_an_unmodified_shipped_old_skill() {
        let (_temp, dirs) = tree();
        configured(&dirs);
        put(
            &skill_path(&dirs),
            include_str!("../tests/fixtures/computer-use-v1.md"),
        );
        disable(&dirs).unwrap();
        assert!(!skill_path(&dirs).exists());
        assert_eq!(config(&dirs)["mcpServers"][SERVER]["disabled"], true);
    }

    #[test]
    fn custom_skill_and_comments_are_preserved() {
        let (_temp, dirs) = tree();
        put(&skill_path(&dirs), "my custom skill");
        assert!(enable(&dirs, Path::new("/test/backend")).is_err());
        assert!(!config_path(&dirs).exists());
        assert_eq!(
            read_optional(&skill_path(&dirs)).unwrap().unwrap(),
            "my custom skill"
        );
        std::fs::remove_file(skill_path(&dirs)).unwrap();
        let commented = "{\n// keep this explanation\n\"mcpServers\":{}}";
        put(&config_path(&dirs), commented);
        assert!(enable(&dirs, Path::new("/test/backend")).is_err());
        assert_eq!(
            read_optional(&config_path(&dirs)).unwrap().unwrap(),
            commented
        );
        assert!(!skill_path(&dirs).exists());
    }

    #[test]
    fn disable_keeps_other_servers_and_customized_skill() {
        let (_temp, dirs) = tree();
        put(
            &config_path(&dirs),
            r#"{"mcpServers":{"other":{"command":"keep"}}}"#,
        );
        configured(&dirs);
        disable(&dirs).unwrap();
        assert_eq!(config(&dirs)["mcpServers"][SERVER]["disabled"], true);
        assert_eq!(config(&dirs)["mcpServers"]["other"]["command"], "keep");
        assert!(!skill_path(&dirs).exists());
        configured(&dirs);
        put(&skill_path(&dirs), "user's new instructions");
        disable(&dirs).unwrap();
        assert_eq!(
            read_optional(&skill_path(&dirs)).unwrap().unwrap(),
            "user's new instructions"
        );
    }

    #[test]
    fn empty_status_reads_and_disable_do_not_create_a_product_tree() {
        let (_temp, dirs) = tree();
        assert!(
            owned_entry(read_optional(&config_path(&dirs)).unwrap().as_deref())
                .unwrap()
                .is_none()
        );
        disable(&dirs).unwrap();
        assert!(!dirs.user().exists());
    }

    #[test]
    fn changed_config_is_not_overwritten() {
        let (_temp, dirs) = tree();
        put(&config_path(&dirs), "new contents");
        assert!(write_checked(
            &config_path(&dirs),
            &Some("old contents".into()),
            "replacement"
        )
        .is_err());
        assert_eq!(
            read_optional(&config_path(&dirs)).unwrap().unwrap(),
            "new contents"
        );
    }

    #[cfg(unix)]
    #[test]
    fn nonregular_config_and_skill_are_refused() {
        use std::os::unix::fs::symlink;
        let (temp, dirs) = tree();
        put(&temp.path().join("external"), "do not touch");
        std::fs::create_dir_all(dirs.user()).unwrap();
        symlink(temp.path().join("external"), config_path(&dirs)).unwrap();
        assert!(enable(&dirs, Path::new("/test/backend")).is_err());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("external")).unwrap(),
            "do not touch"
        );
        std::fs::remove_file(config_path(&dirs)).unwrap();
        std::fs::create_dir_all(skill_path(&dirs)).unwrap();
        assert!(enable(&dirs, Path::new("/test/backend")).is_err());
        assert!(!config_path(&dirs).exists());
    }

    #[cfg(unix)]
    fn fake_backend(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("backend with spaces");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    // These probes depend on real subprocess I/O and scheduling. A paused
    // Tokio clock could expire before the OS gets to run the child process.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_uses_only_noninteractive_diagnosis_and_disables_opt_ins() {
        let (temp, dirs) = tree();
        let mut script = String::from(
            "[ \"$#\" = 2 ] && [ \"$1\" = doctor ] && [ \"$2\" = --no-ui ] || exit 2\n",
        );
        for flag in BLOCKED_OPT_INS {
            script.push_str(&format!("[ \"${flag}\" = 0 ] || exit 3\n"));
        }
        script.push_str(&format!("printf '%s' '{READY}'"));
        let backend = fake_backend(temp.path(), &script);
        assert_eq!(
            resolve_backend(&backend).unwrap(),
            std::fs::canonicalize(&backend).unwrap()
        );
        probe(&backend, PROBE_TIMEOUT)
            .await
            .unwrap()
            .require_ready()
            .unwrap();
        assert!(!dirs.user().exists());
        enable_after_probe(&dirs, &backend, PROBE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            config(&dirs)["mcpServers"][SERVER]["command"],
            backend.to_str().unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_probe_never_writes_configuration_or_skill() {
        let (temp, dirs) = tree();
        for script in [
            format!(
                "printf '%s' '{}'",
                READY.replace("\"accessibility\":true", "\"accessibility\":false")
            ),
            "printf 'invalid report'".into(),
            "printf 'private diagnostic details' >&2; exit 1".into(),
            "head -c 17000 /dev/zero".into(),
            "head -c 17000 /dev/zero >&2".into(),
        ] {
            let backend = fake_backend(temp.path(), &script);
            let error = enable_after_probe(&dirs, &backend, PROBE_TIMEOUT)
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("private diagnostic details"));
            assert!(!dirs.user().exists());
        }
        assert!(resolve_backend(&temp.path().join("missing backend")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_probe_kills_its_child_group_and_does_not_commit() {
        let (temp, dirs) = tree();
        let backend = fake_backend(
            temp.path(),
            "sleep 1; printf 'should never happen' > \"$0.effect\"",
        );
        let error = enable_after_probe(&dirs, &backend, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!temp.path().join("backend with spaces.effect").exists());
        assert!(!dirs.user().exists());
    }
}
