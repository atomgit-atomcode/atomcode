//! MCP configuration loading.

use std::collections::BTreeMap;
use std::path::Path;

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
}

impl McpConfigSource {
    /// Returns the string representation for telemetry JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            McpConfigSource::Project => "project",
            McpConfigSource::User => "global",
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

/// Load and merge MCP configurations from project and user levels.
///
/// Project config (`.mcp.json` in project root) overrides user config
/// (`ATOMCODE_HOME/mcp.json`) for servers with the same name.
pub fn load_mcp_config(project_dir: &Path) -> Result<Vec<McpServerConfig>> {
    let user_config = load_config_file(
        &crate::mcp::util::config_dir().join("mcp.json"),
        McpConfigSource::User,
    )?;

    let project_config =
        load_config_file(&project_dir.join(".mcp.json"), McpConfigSource::Project)?;

    // Merge: project overrides user
    let mut merged: BTreeMap<String, McpServerConfig> = BTreeMap::new();

    for config in user_config {
        merged.insert(config.name.clone(), config);
    }

    for config in project_config {
        merged.insert(config.name.clone(), config);
    }

    Ok(merged.into_values().filter(|c| !c.disabled).collect())
}

/// Blank out `//` and `/* … */` comments so a JSONC-flavoured config parses.
///
/// Editors (VS Code, Cursor) and our own `.mcp.json.example` all treat this file as
/// JSONC, so people paste commented configs. Comment bytes are replaced with spaces
/// rather than removed, and newlines inside block comments are kept, so every surviving
/// byte stays at its original offset — a `serde_json` parse error still reports the
/// line/column the user sees in their editor.
///
/// String literals are never touched: `"https://mcp.example.com/mcp"` must survive the
/// `//` in its scheme, and `"a\"//b"` must not end the string at the escaped quote.
///
/// Trailing commas remain invalid — this is comment tolerance, not full JSON5.
fn strip_json_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_string {
            out.push(b);
            // A backslash escapes the next byte, including `\"` — consume both so an
            // escaped quote does not look like the end of the string.
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                in_string = true;
                out.push(b);
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.extend_from_slice(b"  ");
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        break;
                    }
                    // Keep newlines so line numbers in parse errors stay truthful.
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }

    // Every emitted byte is either copied verbatim or an ASCII space/newline, and a
    // multi-byte UTF-8 sequence inside a comment has ALL of its bytes replaced, so the
    // result is still valid UTF-8. The fallback keeps this infallible rather than
    // panicking: the caller's parse then fails on the original text.
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

/// True when `text` carries JSONC comments, i.e. rewriting it as plain JSON would
/// silently discard something the user wrote.
fn has_json_comments(text: &str) -> bool {
    strip_json_comments(text) != text
}

/// Read a config file that is about to be REWRITTEN by a machine edit.
///
/// A rewrite serialises the parsed `Value` back out, which would silently delete every
/// comment in the file. Refuse instead: the read path tolerates comments, so the config
/// still works — it just cannot be edited for you. Losing someone's annotations without
/// telling them is worse than making them edit by hand.
fn read_json_for_rewrite(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read MCP config from {}", path.display()))?;
    if has_json_comments(&text) {
        bail!(
            "{} contains comments, and rewriting it would delete them. \
             Edit the file by hand, or remove the comments and retry.",
            path.display()
        );
    }
    serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse MCP config JSON from {}", path.display()))
}

fn load_config_file(path: &Path, source: McpConfigSource) -> Result<Vec<McpServerConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read MCP config from {}", path.display()))?;

    let raw: McpConfigFile = serde_json::from_str(&strip_json_comments(&content))
        .with_context(|| format!("Failed to parse MCP config from {}", path.display()))?;

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
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }
    if program.is_empty() {
        bail!("command must not be empty");
    }

    let mut root: Value = if path.exists() {
        read_json_for_rewrite(path)?
    } else {
        json!({})
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

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory for {}", path.display())
            })?;
        }
    }

    let text = serde_json::to_string_pretty(&root).context("Failed to serialize MCP config")?;
    std::fs::write(path, format!("{text}\n"))
        .with_context(|| format!("Failed to write MCP config to {}", path.display()))?;

    Ok(())
}

/// Add or replace an **HTTP OAuth** MCP server entry in a JSON config file.
pub fn merge_http_oauth_mcp_server_into_json_file(
    path: &Path,
    server_key: &str,
    url: &str,
    provider: &str,
) -> Result<()> {
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }
    if url.is_empty() {
        bail!("url must not be empty");
    }
    if provider.is_empty() {
        bail!("provider must not be empty");
    }

    let mut root: Value = if path.exists() {
        read_json_for_rewrite(path)?
    } else {
        json!({})
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

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory for {}", path.display())
            })?;
        }
    }

    let pretty = serde_json::to_string_pretty(&root).context("Failed to serialize MCP config")?;
    std::fs::write(path, format!("{}\n", pretty))
        .with_context(|| format!("Failed to write MCP config to {}", path.display()))?;
    Ok(())
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
pub fn add_auto_approved_tool(project_dir: &Path, server: &str, tool: &str) -> Result<()> {
    let project_path = project_dir.join(".mcp.json");
    let user_path = crate::mcp::util::config_dir().join("mcp.json");

    let defines = |p: &Path| -> bool {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&strip_json_comments(&s)).ok())
            .and_then(|v| {
                let obj = v.get("mcpServers").or_else(|| v.get("servers")).cloned()?;
                obj.as_object().map(|m| m.contains_key(server))
            })
            .unwrap_or(false)
    };

    let target = if defines(&project_path) {
        project_path
    } else {
        user_path
    };

    let mut root: Value = if target.exists() {
        read_json_for_rewrite(&target)?
    } else {
        serde_json::json!({ "mcpServers": {} })
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

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, serde_json::to_string_pretty(&root)?)?;
    Ok(())
}

#[cfg(test)]
mod jsonc_tests {
    use super::*;

    /// The trap: a URL's `//` sits inside a string and must survive untouched.
    #[test]
    fn double_slash_inside_a_string_is_not_a_comment() {
        let src = r#"{"url": "https://mcp.example.com/mcp"}"#;
        assert_eq!(
            strip_json_comments(src),
            src,
            "stripping must not touch string contents"
        );
        let parsed: Value = serde_json::from_str(&strip_json_comments(src)).unwrap();
        assert_eq!(parsed["url"], "https://mcp.example.com/mcp");
    }

    #[test]
    fn escaped_quote_does_not_end_the_string_early() {
        // The `//` here is still inside the string: the `\"` must not close it.
        let src = r#"{"a": "x\"//y", "b": 1}"#;
        assert_eq!(strip_json_comments(src), src);
        let parsed: Value = serde_json::from_str(&strip_json_comments(src)).unwrap();
        assert_eq!(parsed["a"], "x\"//y");
        assert_eq!(parsed["b"], 1);
    }

    #[test]
    fn line_and_block_comments_are_blanked_not_removed() {
        let src = "{\n  // 说明\n  \"a\": 1, /* 尾注 */\n  \"b\": 2\n}";
        let out = strip_json_comments(src);
        assert_eq!(
            out.len(),
            src.len(),
            "byte offsets must be preserved so parse errors keep pointing at the right column"
        );
        assert_eq!(
            out.lines().count(),
            src.lines().count(),
            "line count must be preserved"
        );
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn multi_line_block_comment_keeps_line_numbers() {
        let src = "{\n/*\n\n*/\n  \"a\": 1\n}";
        let out = strip_json_comments(src);
        assert_eq!(out.matches('\n').count(), src.matches('\n').count());
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["a"], 1);
    }

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
        assert!(serde_json::from_str::<Value>(&strip_json_comments("{\"a\":1,}")).is_err());
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

        add_auto_approved_tool(dir.path(), "srv", "query").expect("write ok");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        let arr = written["mcpServers"]["srv"]["autoApprove"]
            .as_array()
            .expect("autoApprove array");
        assert!(arr.iter().any(|v| v == "query"));

        // Idempotent: second call must not duplicate.
        add_auto_approved_tool(dir.path(), "srv", "query").unwrap();
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

        let error = load_mcp_config(project.path()).unwrap_err();

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
