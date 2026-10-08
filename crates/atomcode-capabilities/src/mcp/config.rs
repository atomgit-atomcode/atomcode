//! MCP configuration loading.
//!
//! Servers are configured in two places: the user-level `<user tree>/mcp.json`
//! (every project) and the project's `.mcp.json` (that project; it overrides the
//! user-level entry of the same name).
//!
//! The free functions read and write those files. Every edit is also offered on a
//! config's text alone (`*_in_text`, [`parse_mcp_servers`]), for a host that keeps
//! the user-level config itself — it holds credentials, and the host may keep it
//! encrypted — and reads and stores it its own way. [`McpStorage`] is the two
//! together: the files, or the host's documents for the user-level config and the
//! OAuth tokens. See `docs/mcp.md` §3.1.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

/// MCP server transport configuration.
#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        auth: Option<McpHttpAuthConfig>,
        timeout_ms: Option<u64>,
    },
}

/// How a server is reached, without how to reach it.
///
/// [`McpTransportConfig`]'s payload carries headers and OAuth material, so a
/// listener that only wants to say "this one is a stdio server" must not be
/// handed the config to find out. This is the discriminant on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransportKind {
    Stdio,
    Http,
}

impl McpTransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            McpTransportKind::Stdio => "stdio",
            McpTransportKind::Http => "http",
        }
    }
}

impl McpTransportConfig {
    /// Which transport this is, with nothing that could authenticate as anyone.
    pub fn kind(&self) -> McpTransportKind {
        match self {
            McpTransportConfig::Stdio { .. } => McpTransportKind::Stdio,
            McpTransportConfig::Http { .. } => McpTransportKind::Http,
        }
    }
}

/// Authentication configuration for HTTP MCP servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpHttpAuthConfig {
    OAuth(McpOAuthConfig),
}

/// OAuth configuration for HTTP MCP servers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpOAuthConfig {
    /// Human-readable/provider compatibility name. Older configs only set this.
    pub provider: Option<String>,
    /// Optional authorization server issuer URL.
    pub issuer: Option<String>,
    /// Optional protected resource metadata URL or fixed resource identifier.
    pub resource: Option<String>,
    /// Optional pre-registered OAuth client id.
    pub client_id: Option<String>,
    /// Optional environment variable containing a confidential client secret.
    pub client_secret_env: Option<String>,
    /// Optional requested scopes.
    pub scopes: Vec<String>,
}

/// MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub disabled: bool,
    pub config: McpTransportConfig,
    /// Where this server config was loaded from (user-level or project-level).
    pub source: McpConfigSource,
    /// `trust: true` in the config ⇒ every tool from this server is auto-approved
    /// (the approval prompt is skipped). MCP servers are external code, so this is
    /// OPT-IN per server.
    pub trust: bool,
    /// Per-tool auto-approve allowlist (the `autoApprove` array): bare tool names
    /// (e.g. `["query", "search"]`) whose calls skip the approval prompt.
    pub auto_approve: Vec<String>,
}

/// Configuration source for a server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum McpConfigSource {
    Project,
    User,
    /// Supplied at runtime by a driver (e.g. an ACP client injecting
    /// `mcpServers` in `session/new`). The client is the trust boundary for
    /// these servers, so [`crate::mcp::trust::partition_by_trust`] never
    /// withholds them — only `Project`-source servers are gated on project
    /// trust.
    Driver,
}

impl McpConfigSource {
    /// Returns the string representation for telemetry JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            McpConfigSource::Project => "project",
            McpConfigSource::User => "global",
            McpConfigSource::Driver => "driver",
        }
    }
}

/// Raw MCP config file format (for deserialization).
#[derive(Debug, Deserialize)]
struct McpConfigFile {
    /// JSON key `mcpServers`（与 Cursor 等工具一致）；`servers` 仍可作为别名读取旧配置。
    #[serde(default, rename = "mcpServers", alias = "servers")]
    mcp_servers: BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, Deserialize)]
struct McpServerEntry {
    /// Ignored for transport selection (stdio vs HTTP is inferred from `command` vs `url`).
    /// Accepted so configs copied from Claude / Cursor validate.
    #[serde(default, rename = "type")]
    _transport_hint: Option<String>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    auth: Option<McpAuthEntry>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// `trust: true` ⇒ auto-approve every tool from this server (skip the prompt).
    #[serde(default)]
    trust: bool,
    /// `autoApprove: ["query", ...]` ⇒ per-tool auto-approve allowlist.
    #[serde(default, rename = "autoApprove", alias = "auto_approve")]
    auto_approve: Vec<String>,
}

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct McpAuthEntry {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret_env: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    bearer: Option<String>,
    #[serde(default)]
    header: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedHttpAuth {
    oauth: Option<McpHttpAuthConfig>,
    headers: BTreeMap<String, String>,
}

/// Merge the user-level and project-level configs, disabled entries included.
///
/// Project overrides user for a server of the same name. This is the whole read; whether
/// disabled servers are withheld is the caller's decision — see [`load_mcp_config`] and
/// [`load_mcp_config_including_disabled`].
fn merge_configs(project_dir: &Path, user_dir: &Path) -> Result<Vec<McpServerConfig>> {
    let user_config = load_config_file(&user_dir.join("mcp.json"), McpConfigSource::User)?;

    let project_config =
        load_config_file(&project_dir.join(".mcp.json"), McpConfigSource::Project)?;

    Ok(project_over_user(user_config, project_config))
}

/// Project overrides user for a server of the same name.
fn project_over_user(
    user_config: Vec<McpServerConfig>,
    project_config: Vec<McpServerConfig>,
) -> Vec<McpServerConfig> {
    let mut merged: BTreeMap<String, McpServerConfig> = BTreeMap::new();

    for config in user_config {
        merged.insert(config.name.clone(), config);
    }

    for config in project_config {
        merged.insert(config.name.clone(), config);
    }

    merged.into_values().collect()
}

// ---- where one project's MCP servers are kept -------------------------------------

/// Where one project's MCP servers are configured, and their OAuth tokens kept.
///
/// [`McpStorage::new`] is the files every front end of this workspace uses: the
/// project's `.mcp.json`, and the user tree's `mcp.json` and `mcp_auth.toml` — the
/// same reads and writes as the free functions in this module, byte for byte.
///
/// A host that keeps the two user-level documents itself — they hold credentials,
/// and it may keep them encrypted — hands them in as
/// [`atomcode_config::DocumentStore`]s ([`with_user_config`](Self::with_user_config),
/// [`with_tokens`](Self::with_tokens)). Every read goes through `read` and every
/// edit through `update`, so the library never touches those documents' storage and
/// never writes them as plain text. The project's `.mcp.json` is written by hand and
/// usually committed; it stays a plain file in every case.
///
/// A document the host cannot read is an error naming it, never an empty config:
/// an edit reads first and stops there, so an unreadable document is not
/// overwritten with an empty one.
#[derive(Clone, Debug)]
pub struct McpStorage {
    project_dir: PathBuf,
    user_dir: PathBuf,
    user_config: Option<Arc<dyn atomcode_config::DocumentStore>>,
    tokens: Option<Arc<dyn atomcode_config::DocumentStore>>,
}

/// How a host-kept user-level config is named in an error.
const HOSTED_USER_CONFIG: &str = "the user-level MCP config the host keeps";

impl McpStorage {
    /// The project's `.mcp.json` and the user tree's files.
    pub fn new(project_dir: impl Into<PathBuf>, user_dir: impl Into<PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
            user_dir: user_dir.into(),
            user_config: None,
            tokens: None,
        }
    }

    /// The user-level config (what `mcp.json` holds) is kept by the host.
    pub fn with_user_config(mut self, store: Arc<dyn atomcode_config::DocumentStore>) -> Self {
        self.user_config = Some(store);
        self
    }

    /// The OAuth tokens (what `mcp_auth.toml` holds, in its format) are kept by the host.
    pub fn with_tokens(mut self, store: Arc<dyn atomcode_config::DocumentStore>) -> Self {
        self.tokens = Some(store);
        self
    }

    /// The directory whose `.mcp.json` is the project config.
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    /// The user tree: still where the project trust store is, whoever keeps the rest.
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }

    /// Where the OAuth tokens are kept.
    pub fn tokens(&self) -> super::oauth::McpTokenStore {
        match &self.tokens {
            Some(store) => super::oauth::McpTokenStore::hosted(store.clone()),
            None => super::oauth::McpTokenStore::in_tree(&self.user_dir),
        }
    }

    /// [`load_mcp_config`] over this storage.
    pub fn load(&self) -> Result<Vec<McpServerConfig>> {
        Ok(self
            .load_including_disabled()?
            .into_iter()
            .filter(|c| !c.disabled)
            .collect())
    }

    /// [`load_mcp_config_including_disabled`] over this storage.
    pub fn load_including_disabled(&self) -> Result<Vec<McpServerConfig>> {
        let Some(store) = &self.user_config else {
            return load_mcp_config_including_disabled(&self.project_dir, &self.user_dir);
        };
        let user_config = match store
            .read()
            .with_context(|| format!("Failed to read {HOSTED_USER_CONFIG}"))?
        {
            Some(text) => servers_in(&text, McpConfigSource::User, &HOSTED_USER_CONFIG)?,
            None => Vec::new(),
        };
        let project_config = load_config_file(
            &self.project_dir.join(".mcp.json"),
            McpConfigSource::Project,
        )?;
        Ok(project_over_user(user_config, project_config))
    }

    /// The file a server of this source is read from: [`config_path_for_source`], and
    /// `None` for a user-level config the host keeps — there is no file of it to show
    /// or open.
    pub fn path_for(&self, source: McpConfigSource) -> Option<PathBuf> {
        match source {
            McpConfigSource::User if self.user_config.is_some() => None,
            _ => config_path_for_source(&self.project_dir, &self.user_dir, source),
        }
    }

    /// [`set_mcp_server_disabled_in_json_file`] on the config `source` is read from.
    pub fn set_server_disabled(
        &self,
        source: McpConfigSource,
        server_key: &str,
        disabled: bool,
    ) -> Result<()> {
        match (source, &self.user_config) {
            (McpConfigSource::User, Some(store)) => {
                if server_key.is_empty() {
                    bail!("MCP server name must not be empty");
                }
                store.update(&mut |text| {
                    let text = text.ok_or_else(|| {
                        anyhow::anyhow!(
                            "MCP server '{server_key}' is not defined in {HOSTED_USER_CONFIG}"
                        )
                    })?;
                    with_server_disabled(text, server_key, disabled, &HOSTED_USER_CONFIG).map(Some)
                })
            }
            _ => {
                set_mcp_server_disabled_in_json_file(&self.file_for(source)?, server_key, disabled)
            }
        }
    }

    /// [`merge_stdio_mcp_server_into_json_file`] on the config `source` is read from.
    pub fn merge_stdio_server(
        &self,
        source: McpConfigSource,
        server_key: &str,
        program: &str,
        args: &[String],
    ) -> Result<()> {
        match (source, &self.user_config) {
            (McpConfigSource::User, Some(store)) => {
                check_stdio_entry(server_key, program)?;
                store.update(&mut |text| {
                    with_stdio_server(text, server_key, program, args, &HOSTED_USER_CONFIG)
                        .map(Some)
                })
            }
            _ => merge_stdio_mcp_server_into_json_file(
                &self.file_for(source)?,
                server_key,
                program,
                args,
            ),
        }
    }

    /// [`merge_http_oauth_mcp_server_into_json_file`] on the config `source` is read from.
    pub fn merge_http_oauth_server(
        &self,
        source: McpConfigSource,
        server_key: &str,
        url: &str,
        provider: &str,
    ) -> Result<()> {
        match (source, &self.user_config) {
            (McpConfigSource::User, Some(store)) => {
                check_http_oauth_entry(server_key, url, provider)?;
                store.update(&mut |text| {
                    with_http_oauth_server(text, server_key, url, provider, &HOSTED_USER_CONFIG)
                        .map(Some)
                })
            }
            _ => merge_http_oauth_mcp_server_into_json_file(
                &self.file_for(source)?,
                server_key,
                url,
                provider,
            ),
        }
    }

    /// [`add_auto_approved_tool`] over this storage: the project file when it defines
    /// `server`, else the user-level config.
    pub fn add_auto_approved_tool(&self, server: &str, tool: &str) -> Result<()> {
        let Some(store) = &self.user_config else {
            return add_auto_approved_tool(&self.project_dir, &self.user_dir, server, tool);
        };
        let project_path = self.project_dir.join(".mcp.json");
        if file_defines_server(&project_path, server) {
            return write_auto_approved_tool(&project_path, server, tool);
        }
        store.update(&mut |text| {
            with_auto_approved_tool(text, server, tool, &HOSTED_USER_CONFIG).map(Some)
        })
    }

    fn file_for(&self, source: McpConfigSource) -> Result<PathBuf> {
        config_path_for_source(&self.project_dir, &self.user_dir, source).ok_or_else(|| {
            anyhow::anyhow!("a driver-supplied MCP server has no config file to edit")
        })
    }
}

/// Load and merge MCP configurations from project and user levels.
///
/// Project config (`.mcp.json` in project root) overrides user config
/// (`<user tree>/mcp.json`) for servers with the same name.
///
/// Servers configured with `disabled: true` are withheld: they are not a tool source for a
/// running session. A surface that manages them wants the other entry point, below.
pub fn load_mcp_config(project_dir: &Path, user_dir: &Path) -> Result<Vec<McpServerConfig>> {
    Ok(merge_configs(project_dir, user_dir)?
        .into_iter()
        .filter(|c| !c.disabled)
        .collect())
}

/// The same merge as [`load_mcp_config`], but keeping `disabled` servers.
///
/// A management surface has to show a server it is offering to re-enable; hiding it would
/// make the switch one-way. Nothing that builds a tool catalog may use this.
pub fn load_mcp_config_including_disabled(
    project_dir: &Path,
    user_dir: &Path,
) -> Result<Vec<McpServerConfig>> {
    merge_configs(project_dir, user_dir)
}

/// The file a server of this source is read from and written back to.
///
/// `None` for [`McpConfigSource::Driver`]: those servers arrive over the wire (an ACP client
/// injecting `mcpServers` in `session/new`) and have no file to edit. A caller that is about
/// to report "disabled" must treat `None` as "not applicable", not as "not found".
pub fn config_path_for_source(
    project_dir: &Path,
    user_dir: &Path,
    source: McpConfigSource,
) -> Option<std::path::PathBuf> {
    match source {
        McpConfigSource::User => Some(user_dir.join("mcp.json")),
        McpConfigSource::Project => Some(project_dir.join(".mcp.json")),
        McpConfigSource::Driver => None,
    }
}

/// Turn one configured server off or back on, in the file that defines it.
///
/// Turning off writes `disabled: true`. Turning on **removes** the key rather than writing
/// `false`, so an enabled entry reads exactly like one that never carried it
/// (`McpServerEntry::disabled` is `#[serde(default)]`).
///
/// Bails, leaving the file byte-identical, when the file carries JSONC comments (see
/// `json_for_rewrite`) or when it does not define `server_key`. This edits an existing
/// entry; it never adds a server.
pub fn set_mcp_server_disabled_in_json_file(
    path: &Path,
    server_key: &str,
    disabled: bool,
) -> Result<()> {
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }
    let text = read_config_text(path)?;
    let out = with_server_disabled(&text, server_key, disabled, &path.display())?;
    std::fs::write(path, out)
        .with_context(|| format!("Failed to write MCP config to {}", path.display()))?;

    Ok(())
}

/// [`set_mcp_server_disabled_in_json_file`] on a config's text rather than its file:
/// the text it should be stored as afterwards. Refuses the same things.
pub fn set_mcp_server_disabled_in_text(
    text: &str,
    server_key: &str,
    disabled: bool,
) -> Result<String> {
    with_server_disabled(text, server_key, disabled, &GIVEN_TEXT)
}

fn with_server_disabled(
    text: &str,
    server_key: &str,
    disabled: bool,
    origin: &dyn std::fmt::Display,
) -> Result<String> {
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }

    let mut root: Value = json_for_rewrite(text, origin)?;

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("MCP config root must be a JSON object"))?;

    // Merge the legacy `servers` key into `mcpServers` first, so the edit lands on the entry
    // a reader would resolve — and is written back in one place.
    let mut servers = collect_merged_mcp_server_maps(root_obj);
    let entry = servers
        .get_mut(server_key)
        .ok_or_else(|| anyhow::anyhow!("MCP server '{server_key}' is not defined in {origin}"))?;
    let entry_obj = entry
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("MCP server '{server_key}' entry is not an object"))?;

    if disabled {
        entry_obj.insert("disabled".to_string(), Value::Bool(true));
    } else {
        entry_obj.remove("disabled");
    }

    root_obj.insert("mcpServers".to_string(), Value::Object(servers));
    root_obj.remove("servers");

    let text = serde_json::to_string_pretty(&root).context("Failed to serialize MCP config")?;
    Ok(format!("{text}\n"))
}

/// What the text functions name as the config's origin in an error, where the
/// file functions name the file.
const GIVEN_TEXT: &str = "the given text";

fn read_config_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read MCP config from {}", path.display()))
}

/// True when `text` carries JSONC comments, i.e. rewriting it as plain JSON would
/// silently discard something the user wrote.
fn has_json_comments(text: &str) -> bool {
    crate::jsonc::strip_comments(text) != text
}

/// Read a config file that is about to be REWRITTEN by a machine edit.
///
/// A rewrite serialises the parsed `Value` back out, which would silently delete every
/// comment in the file. Refuse instead: the read path tolerates comments, so the config
/// still works — it just cannot be edited for you. Losing someone's annotations without
/// telling them is worse than making them edit by hand.
fn json_for_rewrite(text: &str, origin: &dyn std::fmt::Display) -> Result<Value> {
    if has_json_comments(text) {
        bail!(
            "{origin} contains comments, and rewriting it would delete them. \
             Edit the file by hand, or remove the comments and retry."
        );
    }
    serde_json::from_str(text)
        .with_context(|| format!("Failed to parse MCP config JSON from {origin}"))
}

fn load_config_file(path: &Path, source: McpConfigSource) -> Result<Vec<McpServerConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = read_config_text(path)?;
    servers_in(&content, source, &path.display())
}

/// The servers a config's text configures, read as a file of `source` would be:
/// comments tolerated, the legacy `servers` key accepted, `disabled` entries kept
/// (filter them the way [`load_mcp_config`] does when building a tool catalog).
///
/// For a host that keeps the config somewhere other than a plain file — encrypted,
/// say — and reads it itself.
pub fn parse_mcp_servers(text: &str, source: McpConfigSource) -> Result<Vec<McpServerConfig>> {
    servers_in(text, source, &GIVEN_TEXT)
}

fn servers_in(
    content: &str,
    source: McpConfigSource,
    origin: &dyn std::fmt::Display,
) -> Result<Vec<McpServerConfig>> {
    let raw: McpConfigFile = serde_json::from_str(&crate::jsonc::strip_comments(content))
        .with_context(|| format!("Failed to parse MCP config from {origin}"))?;

    let mut configs = Vec::new();

    for (name, entry) in raw.mcp_servers {
        let mut config = server_entry_to_config(&name, entry)?;
        config.source = source;
        configs.push(config);
    }

    Ok(configs)
}

fn server_entry_to_config(name: &str, entry: McpServerEntry) -> Result<McpServerConfig> {
    let trust = entry.trust;
    let disabled = entry.disabled;
    let transport = if let Some(command) = entry.command {
        McpTransportConfig::Stdio {
            command: expand_tilde(&expand_env_vars(&command)),
            args: entry
                .args
                .unwrap_or_default()
                .into_iter()
                .map(|a| expand_tilde(&expand_env_vars(&a)))
                .collect(),
            env: entry
                .env
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, expand_env_vars(&v)))
                .collect(),
            timeout_ms: entry.timeout_ms,
        }
    } else if let Some(url) = entry.url {
        let parsed_auth = parse_http_auth(name, entry.auth)?;
        let mut headers: BTreeMap<String, String> = entry
            .headers
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, expand_env_vars(&v)))
            .collect();
        for (k, v) in parsed_auth.headers {
            headers.entry(k).or_insert(v);
        }
        McpTransportConfig::Http {
            url: expand_tilde(&expand_env_vars(&url)),
            headers,
            auth: parsed_auth.oauth,
            timeout_ms: entry.timeout_ms,
        }
    } else {
        bail!(
            "MCP server '{}' must have either 'command' (stdio) or 'url' (http)",
            name
        );
    };

    Ok(McpServerConfig {
        name: name.to_string(),
        disabled,
        config: transport,
        source: McpConfigSource::Project, // default; overwritten by load_config_file
        trust,
        auto_approve: entry.auto_approve,
    })
}

fn parse_http_auth(name: &str, auth: Option<McpAuthEntry>) -> Result<ParsedHttpAuth> {
    let mut parsed = ParsedHttpAuth {
        oauth: None,
        headers: BTreeMap::new(),
    };
    let Some(auth) = auth else {
        return Ok(parsed);
    };

    if let (Some(header), Some(bearer)) = (auth.header, auth.bearer) {
        parsed.headers.insert(header, expand_env_vars(&bearer));
    }

    match auth.kind.as_deref() {
        Some("oauth") => {
            parsed.oauth = Some(McpHttpAuthConfig::OAuth(McpOAuthConfig {
                provider: Some(auth.provider.unwrap_or_else(|| name.to_string())),
                issuer: auth.issuer.map(|v| expand_env_vars(&v)),
                resource: auth.resource.map(|v| expand_env_vars(&v)),
                client_id: auth.client_id.map(|v| expand_env_vars(&v)),
                client_secret_env: auth.client_secret_env,
                scopes: auth
                    .scopes
                    .into_iter()
                    .map(|s| expand_env_vars(&s))
                    .collect(),
            }));
            Ok(parsed)
        }
        Some(other) => bail!(
            "MCP server '{}' has unsupported auth.type '{}'",
            name,
            other
        ),
        None => Ok(parsed),
    }
}

fn collect_merged_mcp_server_maps(root: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(Value::Object(m)) = root.get("servers") {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Object(m)) = root.get("mcpServers") {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Add or replace a **stdio** MCP server entry in a JSON config file (`.mcp.json` or `$ATOMCODE_HOME/mcp.json`).
///
/// Merges existing `servers` and `mcpServers` maps, then writes a single `mcpServers` object (drops the legacy
/// `servers` key). Other top-level JSON keys are preserved.
pub fn merge_stdio_mcp_server_into_json_file(
    path: &Path,
    server_key: &str,
    program: &str,
    args: &[String],
) -> Result<()> {
    check_stdio_entry(server_key, program)?;
    let existing = if path.exists() {
        Some(read_config_text(path)?)
    } else {
        None
    };
    let out = with_stdio_server(
        existing.as_deref(),
        server_key,
        program,
        args,
        &path.display(),
    )?;
    create_parent(path)?;
    std::fs::write(path, out)
        .with_context(|| format!("Failed to write MCP config to {}", path.display()))?;

    Ok(())
}

/// [`merge_stdio_mcp_server_into_json_file`] on a config's text — `None` for a config
/// that does not exist yet: the text it should be stored as afterwards.
pub fn merge_stdio_mcp_server_into_text(
    text: Option<&str>,
    server_key: &str,
    program: &str,
    args: &[String],
) -> Result<String> {
    with_stdio_server(text, server_key, program, args, &GIVEN_TEXT)
}

fn check_stdio_entry(server_key: &str, program: &str) -> Result<()> {
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }
    if program.is_empty() {
        bail!("command must not be empty");
    }
    Ok(())
}

fn with_stdio_server(
    existing: Option<&str>,
    server_key: &str,
    program: &str,
    args: &[String],
    origin: &dyn std::fmt::Display,
) -> Result<String> {
    check_stdio_entry(server_key, program)?;

    let mut root: Value = match existing {
        Some(text) => json_for_rewrite(text, origin)?,
        None => json!({}),
    };

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("MCP config root must be a JSON object"))?;

    let mut servers = collect_merged_mcp_server_maps(root_obj);
    let entry = json!({
        "command": program,
        "args": args,
    });
    servers.insert(server_key.to_string(), entry);
    root_obj.insert("mcpServers".to_string(), Value::Object(servers));
    root_obj.remove("servers");

    let text = serde_json::to_string_pretty(&root).context("Failed to serialize MCP config")?;
    Ok(format!("{text}\n"))
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory for {}", path.display())
            })?;
        }
    }
    Ok(())
}

/// Add or replace an **HTTP OAuth** MCP server entry in a JSON config file.
pub fn merge_http_oauth_mcp_server_into_json_file(
    path: &Path,
    server_key: &str,
    url: &str,
    provider: &str,
) -> Result<()> {
    check_http_oauth_entry(server_key, url, provider)?;
    let existing = if path.exists() {
        Some(read_config_text(path)?)
    } else {
        None
    };
    let out = with_http_oauth_server(
        existing.as_deref(),
        server_key,
        url,
        provider,
        &path.display(),
    )?;
    create_parent(path)?;
    std::fs::write(path, out)
        .with_context(|| format!("Failed to write MCP config to {}", path.display()))?;
    Ok(())
}

/// [`merge_http_oauth_mcp_server_into_json_file`] on a config's text — `None` for a
/// config that does not exist yet: the text it should be stored as afterwards.
pub fn merge_http_oauth_mcp_server_into_text(
    text: Option<&str>,
    server_key: &str,
    url: &str,
    provider: &str,
) -> Result<String> {
    with_http_oauth_server(text, server_key, url, provider, &GIVEN_TEXT)
}

fn check_http_oauth_entry(server_key: &str, url: &str, provider: &str) -> Result<()> {
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }
    if url.is_empty() {
        bail!("url must not be empty");
    }
    if provider.is_empty() {
        bail!("provider must not be empty");
    }
    Ok(())
}

fn with_http_oauth_server(
    existing: Option<&str>,
    server_key: &str,
    url: &str,
    provider: &str,
    origin: &dyn std::fmt::Display,
) -> Result<String> {
    check_http_oauth_entry(server_key, url, provider)?;

    let mut root: Value = match existing {
        Some(text) => json_for_rewrite(text, origin)?,
        None => json!({}),
    };

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("MCP config root must be a JSON object"))?;

    let mut servers = collect_merged_mcp_server_maps(root_obj);
    let entry = json!({
        "url": url,
        "auth": {
            "type": "oauth",
            "provider": provider,
        },
    });
    servers.insert(server_key.to_string(), entry);
    root_obj.insert("mcpServers".to_string(), Value::Object(servers));
    root_obj.remove("servers");

    let pretty = serde_json::to_string_pretty(&root).context("Failed to serialize MCP config")?;
    Ok(format!("{}\n", pretty))
}

/// Expand environment variables in a string.
///
/// Supports `${VAR}` and `${VAR:-default}` syntax.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            i += 2; // skip ${

            let mut var_name = String::new();
            let mut default = String::new();
            let mut has_default = false;

            while i < bytes.len() && bytes[i] != b'}' {
                if bytes[i] == b':' && !has_default && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                    i += 2; // skip :-
                    has_default = true;
                    continue;
                }
                if has_default {
                    default.push(bytes[i] as char);
                } else {
                    var_name.push(bytes[i] as char);
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // skip }
            }

            let value = std::env::var(&var_name).unwrap_or_else(|_| {
                if has_default {
                    default
                } else {
                    String::new()
                }
            });
            result.push_str(&value);
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

/// Expand a leading `~` (home) in a string.
///
/// - `~/path` → `$HOME/path`
/// - `~` → `$HOME`
/// - Other forms (e.g. `~user/...`) are left unchanged.
fn expand_tilde(s: &str) -> String {
    if s == "~" {
        return crate::mcp::util::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|| s.to_string());
    }
    let Some(rest) = s.strip_prefix("~/") else {
        return s.to_string();
    };
    let Some(home) = crate::mcp::util::home_dir() else {
        return s.to_string();
    };
    home.join(rest).to_string_lossy().to_string()
}

/// Persist a per-tool auto-approve grant: append `tool` to the `autoApprove`
/// array of `server` in whichever config file defines it (project `.mcp.json`
/// first, else user `mcp.json`). Creates the user file if neither defines it.
/// Idempotent; preserves existing JSON content.
pub fn add_auto_approved_tool(
    project_dir: &Path,
    user_dir: &Path,
    server: &str,
    tool: &str,
) -> Result<()> {
    let project_path = project_dir.join(".mcp.json");
    let user_path = user_dir.join("mcp.json");

    let target = if file_defines_server(&project_path, server) {
        project_path
    } else {
        user_path
    };

    write_auto_approved_tool(&target, server, tool)
}

/// Whether the config file at `path` defines `server` — anything unreadable or
/// unparseable does not.
fn file_defines_server(path: &Path, server: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&crate::jsonc::strip_comments(&s)).ok())
        .and_then(|v| {
            let obj = v.get("mcpServers").or_else(|| v.get("servers")).cloned()?;
            obj.as_object().map(|m| m.contains_key(server))
        })
        .unwrap_or(false)
}

fn write_auto_approved_tool(target: &Path, server: &str, tool: &str) -> Result<()> {
    let existing = if target.exists() {
        Some(read_config_text(target)?)
    } else {
        None
    };
    let out = with_auto_approved_tool(existing.as_deref(), server, tool, &target.display())?;

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(target, out)?;
    Ok(())
}

/// The edit [`add_auto_approved_tool`] makes to whichever file it picks, on a config's
/// text — `None` for a config that does not exist yet: the text it should be stored as
/// afterwards. Choosing the file (the project's when it defines `server`, else the
/// user-level one) stays with the caller.
pub fn add_auto_approved_tool_to_text(
    text: Option<&str>,
    server: &str,
    tool: &str,
) -> Result<String> {
    with_auto_approved_tool(text, server, tool, &GIVEN_TEXT)
}

fn with_auto_approved_tool(
    existing: Option<&str>,
    server: &str,
    tool: &str,
    origin: &dyn std::fmt::Display,
) -> Result<String> {
    let mut root: Value = match existing {
        Some(text) => json_for_rewrite(text, origin)?,
        None => serde_json::json!({ "mcpServers": {} }),
    };

    let key = if root.get("servers").is_some() && root.get("mcpServers").is_none() {
        "servers"
    } else {
        "mcpServers"
    };
    if !root.get(key).map(|v| v.is_object()).unwrap_or(false) {
        root[key] = serde_json::json!({});
    }
    let servers = root[key]
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{key} is not an object"))?;

    let entry = servers
        .entry(server.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let entry_obj = entry
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("server '{server}' entry is not an object"))?;
    let list = entry_obj
        .entry("autoApprove".to_string())
        .or_insert_with(|| serde_json::json!([]));
    let arr = list
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("autoApprove is not an array"))?;
    if !arr.iter().any(|v| v.as_str() == Some(tool)) {
        arr.push(Value::String(tool.to_string()));
    }

    Ok(serde_json::to_string_pretty(&root)?)
}

#[cfg(test)]
mod jsonc_tests {
    use super::*;

    #[test]
    fn commented_config_file_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{
  // stdio 示例
  "mcpServers": {
    "srv": {
      "command": "x" /* 行内说明 */
    }
  }
}"#,
        )
        .unwrap();
        let configs = load_config_file(&dir.path().join(".mcp.json"), McpConfigSource::Project)
            .expect("a commented .mcp.json must load");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "srv");
    }

    #[test]
    fn trailing_commas_are_still_rejected() {
        // Comment tolerance is not JSON5; keep the failure mode honest.
        assert!(
            serde_json::from_str::<Value>(&crate::jsonc::strip_comments("{\"a\":1,}")).is_err()
        );
    }

    #[test]
    fn rewriting_a_commented_file_is_refused_instead_of_dropping_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        let original = "{\n  // 别删我\n  \"mcpServers\": {}\n}";
        std::fs::write(&path, original).unwrap();

        let error = merge_stdio_mcp_server_into_json_file(&path, "srv", "npx", &[]).unwrap_err();
        assert!(
            error.to_string().contains("contains comments"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a refused rewrite must leave the file byte-identical"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn add_auto_approved_tool_writes_into_project_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{"srv":{"command":"x"}}}"#,
        )
        .unwrap();

        add_auto_approved_tool(dir.path(), &dir.path().join("tree"), "srv", "query")
            .expect("write ok");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        let arr = written["mcpServers"]["srv"]["autoApprove"]
            .as_array()
            .expect("autoApprove array");
        assert!(arr.iter().any(|v| v == "query"));

        // Idempotent: second call must not duplicate.
        add_auto_approved_tool(dir.path(), &dir.path().join("tree"), "srv", "query").unwrap();
        let again: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            again["mcpServers"]["srv"]["autoApprove"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_expand_env_vars_simple() {
        std::env::set_var("TEST_VAR", "test_value");
        let result = expand_env_vars("${TEST_VAR}");
        assert_eq!(result, "test_value");
    }

    #[test]
    fn test_expand_env_vars_with_default() {
        std::env::remove_var("NONEXISTENT_VAR");
        let result = expand_env_vars("${NONEXISTENT_VAR:-default_value}");
        assert_eq!(result, "default_value");
    }

    #[test]
    fn test_expand_env_vars_existing_with_default() {
        std::env::set_var("EXISTING_VAR", "actual");
        let result = expand_env_vars("${EXISTING_VAR:-unused}");
        assert_eq!(result, "actual");
    }

    #[test]
    fn test_expand_env_vars_no_var() {
        std::env::remove_var("MISSING_VAR");
        let result = expand_env_vars("${MISSING_VAR}");
        assert_eq!(result, "");
    }

    #[test]
    fn test_expand_env_vars_mixed() {
        std::env::set_var("VAR1", "a");
        std::env::set_var("VAR2", "b");
        let result = expand_env_vars("prefix_${VAR1}_middle_${VAR2}_suffix");
        assert_eq!(result, "prefix_a_middle_b_suffix");
    }

    #[test]
    fn test_expand_tilde_home_only() {
        let Some(home) = crate::mcp::util::home_dir() else {
            return;
        };
        assert_eq!(expand_tilde("~"), home.to_string_lossy());
    }

    #[test]
    fn test_expand_tilde_home_prefix() {
        let Some(home) = crate::mcp::util::home_dir() else {
            return;
        };
        assert_eq!(
            expand_tilde("~/x/y"),
            home.join("x/y").to_string_lossy().to_string()
        );
    }

    #[test]
    fn test_expand_tilde_does_not_expand_other_forms() {
        assert_eq!(expand_tilde("~someone/x"), "~someone/x");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }

    #[test]
    fn mcp_config_file_accepts_mcp_servers_key() {
        let raw: McpConfigFile =
            serde_json::from_str(r#"{"mcpServers":{"a":{"command":"echo","args":[]}}}"#).unwrap();
        assert!(raw.mcp_servers.contains_key("a"));
    }

    #[test]
    fn parses_trust_and_auto_approve_from_mcp_json() {
        // The exact shapes from the bug report — both must be honored, not dropped.
        let raw: McpConfigFile = serde_json::from_str(
            r#"{"mcpServers":{"my-docs":{"command":"my-docs-server","trust":true,"autoApprove":["query","search"]}}}"#,
        )
        .unwrap();
        let (name, entry) = raw.mcp_servers.into_iter().next().unwrap();
        let cfg = server_entry_to_config(&name, entry).unwrap();
        assert!(cfg.trust, "trust:true must be parsed");
        assert_eq!(
            cfg.auto_approve,
            vec!["query".to_string(), "search".to_string()]
        );
    }

    #[test]
    fn trust_and_auto_approve_default_off_when_absent() {
        let raw: McpConfigFile =
            serde_json::from_str(r#"{"mcpServers":{"s":{"url":"http://127.0.0.1:8080/mcp"}}}"#)
                .unwrap();
        let (name, entry) = raw.mcp_servers.into_iter().next().unwrap();
        let cfg = server_entry_to_config(&name, entry).unwrap();
        assert!(!cfg.trust);
        assert!(cfg.auto_approve.is_empty());
    }

    #[test]
    fn mcp_config_file_accepts_servers_alias() {
        let raw: McpConfigFile =
            serde_json::from_str(r#"{"servers":{"b":{"command":"echo","args":[]}}}"#).unwrap();
        assert!(raw.mcp_servers.contains_key("b"));
    }

    #[test]
    fn load_mcp_config_reports_malformed_project_file() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".mcp.json"), "{not-json").unwrap();

        let error = load_mcp_config(project.path(), &project.path().join("tree")).unwrap_err();

        assert!(error.to_string().contains("Failed to parse MCP config"));
    }

    #[test]
    fn merge_stdio_creates_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        merge_stdio_mcp_server_into_json_file(&path, "p", "npx", &["@x/y".to_string()]).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let p = v["mcpServers"]["p"].as_object().unwrap();
        assert_eq!(p["command"].as_str(), Some("npx"));
        assert_eq!(p["args"].as_array().unwrap()[0].as_str(), Some("@x/y"));
    }

    #[test]
    fn merge_stdio_preserves_other_top_level_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"note":"keep","mcpServers":{"old":{"command":"true","args":[]}}}"#,
        )
        .unwrap();
        merge_stdio_mcp_server_into_json_file(&path, "new", "uv", &[]).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v.get("note").and_then(|x| x.as_str()), Some("keep"));
        let m = v.get("mcpServers").unwrap().as_object().unwrap();
        assert!(m.contains_key("old"));
        assert!(m.contains_key("new"));
    }

    #[test]
    fn http_config_accepts_oauth_auth() {
        let cfg = server_entry_to_config(
            "github",
            serde_json::from_str(
                r#"{
                    "url":"https://api.githubcopilot.com/mcp/",
                    "auth":{"type":"oauth","provider":"github"}
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        match cfg.config {
            McpTransportConfig::Http { auth, .. } => {
                assert_eq!(
                    auth,
                    Some(McpHttpAuthConfig::OAuth(McpOAuthConfig {
                        provider: Some("github".to_string()),
                        ..McpOAuthConfig::default()
                    }))
                );
            }
            _ => panic!("expected http config"),
        }
    }

    #[test]
    fn http_config_accepts_generic_oauth_auth() {
        let cfg = server_entry_to_config(
            "notion",
            serde_json::from_str(
                r#"{
                    "url":"https://mcp.notion.com/mcp",
                    "auth":{
                        "type":"oauth",
                        "issuer":"https://mcp.notion.com",
                        "resource":"https://mcp.notion.com/mcp",
                        "client_id":"client",
                        "client_secret_env":"NOTION_SECRET",
                        "scopes":["read","write"]
                    }
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        match cfg.config {
            McpTransportConfig::Http { auth, .. } => {
                assert_eq!(
                    auth,
                    Some(McpHttpAuthConfig::OAuth(McpOAuthConfig {
                        provider: Some("notion".to_string()),
                        issuer: Some("https://mcp.notion.com".to_string()),
                        resource: Some("https://mcp.notion.com/mcp".to_string()),
                        client_id: Some("client".to_string()),
                        client_secret_env: Some("NOTION_SECRET".to_string()),
                        scopes: vec!["read".to_string(), "write".to_string()],
                    }))
                );
            }
            _ => panic!("expected http config"),
        }
    }

    #[test]
    fn http_config_accepts_bearer_header_auth_without_type() {
        let cfg = server_entry_to_config(
            "figma",
            serde_json::from_str(
                r#"{
                    "url":"https://mcp.figma.com/mcp",
                    "auth":{"bearer":"figd_token","header":"X-Figma-Token"}
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        match cfg.config {
            McpTransportConfig::Http { headers, auth, .. } => {
                assert_eq!(
                    headers.get("X-Figma-Token").map(String::as_str),
                    Some("figd_token")
                );
                assert_eq!(auth, None);
            }
            _ => panic!("expected http config"),
        }
    }

    #[test]
    fn merge_http_oauth_creates_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        merge_http_oauth_mcp_server_into_json_file(
            &path,
            "github",
            "https://api.githubcopilot.com/mcp/",
            "github",
        )
        .unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let p = v["mcpServers"]["github"].as_object().unwrap();
        assert_eq!(
            p["url"].as_str(),
            Some("https://api.githubcopilot.com/mcp/")
        );
        assert_eq!(p["auth"]["type"].as_str(), Some("oauth"));
        assert_eq!(p["auth"]["provider"].as_str(), Some("github"));
    }

    #[test]
    fn listing_shows_disabled_servers_that_loading_still_hides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{
                "panel-test-on":  {"command":"npx","args":["-y","a"]},
                "panel-test-off": {"command":"npx","args":["-y","b"],"disabled":true}
            }}"#,
        )
        .unwrap();

        let listed =
            load_mcp_config_including_disabled(dir.path(), &dir.path().join("tree")).unwrap();
        let names: Vec<&str> = listed.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"panel-test-off"),
            "the management list shows disabled servers: {names:?}"
        );
        assert!(
            listed
                .iter()
                .find(|c| c.name == "panel-test-off")
                .unwrap()
                .disabled
        );

        let loaded = load_mcp_config(dir.path(), &dir.path().join("tree")).unwrap();
        let names: Vec<&str> = loaded.iter().map(|c| c.name.as_str()).collect();
        assert!(
            !names.contains(&"panel-test-off"),
            "the runtime load still withholds it: {names:?}"
        );
        assert!(names.contains(&"panel-test-on"));
    }

    #[test]
    fn a_driver_server_has_no_config_file_to_write() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            config_path_for_source(
                dir.path(),
                &dir.path().join("tree"),
                McpConfigSource::Project
            ),
            Some(dir.path().join(".mcp.json")),
            "a project server lives in the project root"
        );
        assert_eq!(
            config_path_for_source(dir.path(), &dir.path().join("tree"), McpConfigSource::User),
            Some(dir.path().join("tree").join("mcp.json")),
            "a user server lives in the user tree handed in"
        );
        assert_eq!(
            config_path_for_source(
                dir.path(),
                &dir.path().join("tree"),
                McpConfigSource::Driver
            ),
            None,
            "a driver-supplied server was never read from a file, so there is none to edit"
        );
    }

    #[test]
    fn a_disabled_server_is_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        std::fs::write(
            &target,
            r#"{"mcpServers":{"srv":{"command":"npx","args":["-y","a"]}}}"#,
        )
        .unwrap();

        set_mcp_server_disabled_in_json_file(&target, "srv", true).unwrap();

        let configs = load_config_file(&target, McpConfigSource::Project).unwrap();
        assert_eq!(configs.len(), 1);
        assert!(configs[0].disabled, "the flag must survive a reload");

        set_mcp_server_disabled_in_json_file(&target, "srv", false).unwrap();

        let configs = load_config_file(&target, McpConfigSource::Project).unwrap();
        assert!(!configs[0].disabled);
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            !text.contains("disabled"),
            "enabling removes the key instead of writing false: {text}"
        );
    }

    #[test]
    fn disabling_a_server_the_file_does_not_define_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        let original = r#"{"mcpServers":{"srv":{"command":"npx"}}}"#;
        std::fs::write(&target, original).unwrap();

        let error = set_mcp_server_disabled_in_json_file(&target, "nope", true).unwrap_err();
        assert!(
            error.to_string().contains("not defined"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            original,
            "a refused write must leave the file byte-identical"
        );
    }

    #[test]
    fn a_server_under_the_legacy_servers_key_can_still_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        std::fs::write(&target, r#"{"servers":{"srv":{"command":"npx"}}}"#).unwrap();

        set_mcp_server_disabled_in_json_file(&target, "srv", true).unwrap();

        let configs = load_config_file(&target, McpConfigSource::Project).unwrap();
        assert!(configs[0].disabled);
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            !text.contains("\"servers\""),
            "the legacy key is folded into mcpServers, same as the other writers: {text}"
        );
    }

    #[test]
    fn a_config_with_comments_refuses_the_disable_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        let original = "{\n  // 别删我\n  \"mcpServers\": {\"srv\": {\"command\": \"npx\"}}\n}";
        std::fs::write(&target, original).unwrap();

        let error = set_mcp_server_disabled_in_json_file(&target, "srv", true).unwrap_err();
        assert!(
            error.to_string().contains("contains comments"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            original,
            "a refused rewrite must leave the file byte-identical"
        );
    }
}

#[cfg(test)]
mod example_template_tests {
    use super::*;

    /// The shipped template is JSONC and tells users to copy it verbatim. If this
    /// breaks, that instruction is a lie again — which is exactly the bug this
    /// comment support was added to fix.
    #[test]
    fn shipped_mcp_json_example_parses_as_is() {
        let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.mcp.json.example");
        let path = std::path::Path::new(example);
        if !path.exists() {
            return; // not a repo checkout (packaged crate) — nothing to assert
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".mcp.json");
        std::fs::copy(path, &target).unwrap();

        let configs = load_config_file(&target, McpConfigSource::Project)
            .expect(".mcp.json.example must parse verbatim — it tells users to copy it as-is");
        // Every template entry ships disabled:true, so nothing is enabled by accident.
        assert!(!configs.is_empty(), "template defines servers");
        assert!(
            configs.iter().all(|c| c.disabled),
            "every template server must ship disabled"
        );
    }
}

/// The text functions are the file functions without the file: for one config,
/// the same edit yields exactly the bytes the file function writes, and the
/// same refusals — so a host that keeps the config elsewhere (encrypted, say)
/// gets the edits the product makes to its own file, not a second version.
#[cfg(test)]
mod text_tests {
    use super::*;

    const START: &str = r#"{
  "keep": 1,
  "servers": { "old": { "command": "a" } },
  "mcpServers": { "srv": { "command": "npx", "args": ["x"] } }
}"#;

    /// Run `file_edit` on a file holding `start` (or no file for `None`) and
    /// return what it wrote.
    fn file_result(start: Option<&str>, file_edit: impl FnOnce(&Path) -> Result<()>) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        if let Some(start) = start {
            std::fs::write(&path, start).unwrap();
        }
        file_edit(&path).unwrap();
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn each_text_edit_is_byte_for_byte_its_file_edit() {
        for start in [Some(START), None] {
            let args = vec!["-y".to_string(), "pkg".to_string()];
            assert_eq!(
                merge_stdio_mcp_server_into_text(start, "new", "node", &args).unwrap(),
                file_result(start, |p| merge_stdio_mcp_server_into_json_file(
                    p, "new", "node", &args
                ))
            );
            assert_eq!(
                merge_http_oauth_mcp_server_into_text(start, "web", "https://m.test", "github")
                    .unwrap(),
                file_result(start, |p| merge_http_oauth_mcp_server_into_json_file(
                    p,
                    "web",
                    "https://m.test",
                    "github"
                ))
            );
            assert_eq!(
                add_auto_approved_tool_to_text(start, "srv", "query").unwrap(),
                file_result(start, |p| {
                    let user = p.parent().unwrap();
                    let project = tempfile::tempdir().unwrap();
                    add_auto_approved_tool(project.path(), user, "srv", "query")
                })
            );
        }
        for disabled in [true, false] {
            assert_eq!(
                set_mcp_server_disabled_in_text(START, "srv", disabled).unwrap(),
                file_result(Some(START), |p| set_mcp_server_disabled_in_json_file(
                    p, "srv", disabled
                ))
            );
        }
    }

    #[test]
    fn parsing_text_reads_what_reading_the_file_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        // Not `START`: a file carrying both the legacy and the current key does not
        // parse on the read path (they are aliases), and that is the file's to say.
        let commented = r#"// a note
{ "mcpServers": { "a": { "command": "x" }, "b": { "url": "https://b.test", "disabled": true } } }"#;
        std::fs::write(&path, commented).unwrap();
        let from_file = load_config_file(&path, McpConfigSource::User).unwrap();
        let from_text = parse_mcp_servers(commented, McpConfigSource::User).unwrap();
        let names = |c: &[McpServerConfig]| {
            c.iter()
                .map(|s| (s.name.clone(), s.source, s.disabled))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&from_text), names(&from_file));
        assert_eq!(names(&from_text).len(), 2, "disabled entries are kept");
    }

    /// The same refusals, naming the text where the file functions name the file;
    /// and a refused edit hands back no text to store.
    #[test]
    fn the_text_edits_refuse_what_the_file_edits_refuse() {
        let commented = format!("// a note\n{START}");
        let error = set_mcp_server_disabled_in_text(&commented, "srv", true)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("the given text contains comments"),
            "{error}"
        );
        let error = set_mcp_server_disabled_in_text(START, "nope", true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("'nope' is not defined in the given text"),
            "{error}"
        );
        let error = format!(
            "{:#}",
            merge_stdio_mcp_server_into_text(Some("{not json"), "s", "c", &[]).unwrap_err()
        );
        assert!(
            error.contains("Failed to parse MCP config JSON from the given text"),
            "{error}"
        );
        let error = format!(
            "{:#}",
            parse_mcp_servers("{not json", McpConfigSource::User).unwrap_err()
        );
        assert!(
            error.contains("Failed to parse MCP config from the given text"),
            "{error}"
        );
        assert!(merge_stdio_mcp_server_into_text(None, "", "c", &[]).is_err());
        assert!(merge_http_oauth_mcp_server_into_text(None, "s", "", "p").is_err());
    }
}

/// What [`McpStorage`] does with documents the host keeps: reads them through
/// `read`, edits them through `update`, never touches the user tree's files for
/// them, and never takes an unreadable document for an empty one.
#[cfg(test)]
mod hosted_tests {
    use super::*;
    use crate::mcp::oauth::{McpOAuthToken, McpTokenStore};
    use std::sync::Mutex;

    /// A document kept "sealed": stored with a prefix the library must never see
    /// or write. `update` holds the lock across read, edit and write, as a host's
    /// store would hold its own.
    #[derive(Debug, Default)]
    struct Sealed {
        stored: Mutex<Option<String>>,
        updates: Mutex<usize>,
    }

    const SEAL: &str = "SEALED:";

    impl Sealed {
        fn holding(plain: &str) -> Arc<Self> {
            let doc = Self::default();
            *doc.stored.lock().unwrap() = Some(format!("{SEAL}{plain}"));
            Arc::new(doc)
        }
        fn raw(&self) -> Option<String> {
            self.stored.lock().unwrap().clone()
        }
        fn plain(&self) -> String {
            self.raw()
                .and_then(|raw| raw.strip_prefix(SEAL).map(str::to_string))
                .expect("stored sealed")
        }
        fn open(raw: &Option<String>) -> Result<Option<String>> {
            match raw {
                None => Ok(None),
                Some(raw) => match raw.strip_prefix(SEAL) {
                    Some(plain) => Ok(Some(plain.to_string())),
                    None => bail!("not sealed with this key"),
                },
            }
        }
    }

    impl atomcode_config::DocumentStore for Sealed {
        fn read(&self) -> Result<Option<String>> {
            Self::open(&self.stored.lock().unwrap())
        }
        fn update(
            &self,
            edit: &mut dyn FnMut(Option<&str>) -> Result<Option<String>>,
        ) -> Result<()> {
            let mut stored = self.stored.lock().unwrap();
            let plain = Self::open(&stored)?;
            let next = edit(plain.as_deref())?;
            *stored = next.map(|text| format!("{SEAL}{text}"));
            *self.updates.lock().unwrap() += 1;
            Ok(())
        }
    }

    const USER: &str =
        r#"{ "mcpServers": { "u": { "command": "uu", "env": { "TOKEN": "s3cret" } } } }"#;

    struct Dirs {
        project: tempfile::TempDir,
        user: tempfile::TempDir,
    }

    fn dirs() -> Dirs {
        let dirs = Dirs {
            project: tempfile::tempdir().unwrap(),
            user: tempfile::tempdir().unwrap(),
        };
        std::fs::write(
            dirs.project.path().join(".mcp.json"),
            r#"{ "mcpServers": { "p": { "command": "pp" } } }"#,
        )
        .unwrap();
        // A user-tree file that must never be read or written once the host keeps
        // the config: reading it would fail, writing it would change it.
        std::fs::write(dirs.user.path().join("mcp.json"), "not json at all").unwrap();
        dirs
    }

    fn storage(dirs: &Dirs, doc: &Arc<Sealed>) -> McpStorage {
        McpStorage::new(dirs.project.path(), dirs.user.path()).with_user_config(doc.clone())
    }

    fn untouched_user_file(dirs: &Dirs) {
        assert_eq!(
            std::fs::read_to_string(dirs.user.path().join("mcp.json")).unwrap(),
            "not json at all",
            "the user tree's file is not the config any more"
        );
    }

    #[test]
    fn a_hosted_user_config_is_read_through_its_store_and_the_project_file_as_a_file() {
        let dirs = dirs();
        let doc = Sealed::holding(USER);
        let servers = storage(&dirs, &doc).load_including_disabled().unwrap();
        let names: Vec<_> = servers
            .iter()
            .map(|s| (s.name.as_str(), s.source))
            .collect();
        assert_eq!(
            names,
            vec![
                ("p", McpConfigSource::Project),
                ("u", McpConfigSource::User)
            ]
        );
        assert_eq!(storage(&dirs, &doc).path_for(McpConfigSource::User), None);
        assert_eq!(
            storage(&dirs, &doc).path_for(McpConfigSource::Project),
            Some(dirs.project.path().join(".mcp.json"))
        );
        untouched_user_file(&dirs);
    }

    #[test]
    fn edits_to_a_hosted_user_config_go_through_update_and_keep_what_was_there() {
        let dirs = dirs();
        let doc = Sealed::holding(USER);
        let storage = storage(&dirs, &doc);

        storage
            .merge_stdio_server(McpConfigSource::User, "n", "nn", &["-x".to_string()])
            .unwrap();
        storage
            .set_server_disabled(McpConfigSource::User, "u", true)
            .unwrap();
        storage.add_auto_approved_tool("u", "query").unwrap();

        let plain: Value = serde_json::from_str(&doc.plain()).unwrap();
        let servers = &plain["mcpServers"];
        assert_eq!(servers["n"]["command"], "nn", "{plain}");
        assert_eq!(servers["u"]["disabled"], true, "{plain}");
        assert_eq!(
            servers["u"]["env"]["TOKEN"], "s3cret",
            "the rest of the entry stays"
        );
        assert_eq!(servers["u"]["autoApprove"], json!(["query"]), "{plain}");
        assert_eq!(*doc.updates.lock().unwrap(), 3, "one update per edit");
        assert!(
            doc.raw().unwrap().starts_with(SEAL),
            "stored as the host keeps it"
        );
        untouched_user_file(&dirs);
    }

    #[test]
    fn an_always_allow_for_a_project_server_lands_in_the_project_file() {
        let dirs = dirs();
        let doc = Sealed::holding(USER);
        storage(&dirs, &doc)
            .add_auto_approved_tool("p", "run")
            .unwrap();
        let project: Value = serde_json::from_str(
            &std::fs::read_to_string(dirs.project.path().join(".mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(project["mcpServers"]["p"]["autoApprove"], json!(["run"]));
        assert_eq!(
            *doc.updates.lock().unwrap(),
            0,
            "the host's document is not touched"
        );
    }

    /// An unreadable document is an error naming it — and the edits that read it
    /// first stop there, so it is not replaced by an empty config.
    #[test]
    fn a_hosted_config_that_cannot_be_read_is_an_error_and_is_left_as_it_is() {
        let dirs = dirs();
        let doc = Arc::new(Sealed::default());
        *doc.stored.lock().unwrap() = Some("sealed with another key".to_string());
        let storage = storage(&dirs, &doc);

        let error = format!("{:#}", storage.load().unwrap_err());
        assert!(
            error.contains(HOSTED_USER_CONFIG) && error.contains("not sealed with this key"),
            "{error}"
        );
        assert!(storage
            .merge_stdio_server(McpConfigSource::User, "n", "nn", &[])
            .is_err());
        assert!(storage
            .set_server_disabled(McpConfigSource::User, "u", true)
            .is_err());
        assert!(storage.add_auto_approved_tool("u", "query").is_err());
        assert_eq!(
            doc.raw().as_deref(),
            Some("sealed with another key"),
            "nothing was written over it"
        );
        untouched_user_file(&dirs);
    }

    /// Without hosted documents, the storage is the user tree's files: the same
    /// bytes as the free functions write.
    #[test]
    fn without_hosted_documents_the_storage_is_the_files() {
        let via_storage = tempfile::tempdir().unwrap();
        let via_functions = tempfile::tempdir().unwrap();
        for dir in [&via_storage, &via_functions] {
            std::fs::write(dir.path().join("mcp.json"), USER).unwrap();
        }
        let storage = McpStorage::new(via_storage.path(), via_storage.path());
        storage
            .merge_stdio_server(McpConfigSource::User, "n", "nn", &[])
            .unwrap();
        storage
            .set_server_disabled(McpConfigSource::User, "u", true)
            .unwrap();
        storage.add_auto_approved_tool("u", "query").unwrap();
        let path = via_functions.path().join("mcp.json");
        merge_stdio_mcp_server_into_json_file(&path, "n", "nn", &[]).unwrap();
        set_mcp_server_disabled_in_json_file(&path, "u", true).unwrap();
        add_auto_approved_tool(via_functions.path(), via_functions.path(), "u", "query").unwrap();
        assert_eq!(
            std::fs::read(via_storage.path().join("mcp.json")).unwrap(),
            std::fs::read(path).unwrap()
        );
        assert_eq!(
            storage.path_for(McpConfigSource::User),
            Some(via_storage.path().join("mcp.json"))
        );
    }

    fn token(access: &str) -> McpOAuthToken {
        McpOAuthToken {
            access_token: access.to_string(),
            refresh_token: Some("r".to_string()),
            expires_at: None,
            client_id: None,
            client_secret_env: None,
            token_endpoint: None,
            issuer: None,
            resource: None,
            scopes: Vec::new(),
            provider: "p".to_string(),
            token_type: "Bearer".to_string(),
        }
    }

    /// Tokens the host keeps are saved, read and deleted through its store, in
    /// `mcp_auth.toml`'s format, and nothing lands in the user tree. Saves from
    /// two threads at once each keep the other's token: every save is one update.
    #[test]
    fn hosted_tokens_go_through_the_store_and_concurrent_saves_both_stay() {
        let user = tempfile::tempdir().unwrap();
        let doc = Arc::new(Sealed::default());
        let storage = McpStorage::new(user.path(), user.path()).with_tokens(doc.clone());
        let tokens = storage.tokens();

        let threads: Vec<_> = (0..8)
            .map(|n| {
                let tokens = tokens.clone();
                std::thread::spawn(move || {
                    tokens
                        .save_token(&format!("s{n}"), token(&format!("a{n}")))
                        .unwrap()
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        for n in 0..8 {
            assert_eq!(
                tokens
                    .load_token(&format!("s{n}"))
                    .unwrap()
                    .unwrap()
                    .access_token,
                format!("a{n}")
            );
        }
        assert!(tokens.delete_token("s3").unwrap());
        assert!(tokens.load_token("s3").unwrap().is_none());
        assert!(!tokens.delete_token("s3").unwrap());
        assert!(
            doc.plain().contains("[servers.s0]"),
            "mcp_auth.toml's format"
        );
        assert!(
            !user.path().join("mcp_auth.toml").exists(),
            "nothing written to the user tree"
        );

        let unreadable = Arc::new(Sealed::default());
        *unreadable.stored.lock().unwrap() = Some("sealed with another key".to_string());
        let tokens = McpTokenStore::hosted(unreadable.clone());
        assert!(tokens.load_token("s0").is_err());
        assert!(tokens.save_token("s0", token("x")).is_err());
        assert_eq!(unreadable.raw().as_deref(), Some("sealed with another key"));
    }
}
