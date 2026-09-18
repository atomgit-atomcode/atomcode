//! OAuth token storage and login helpers for remote MCP.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use super::config::{McpHttpAuthConfig, McpOAuthConfig, McpServerConfig, McpTransportConfig};

const GITHUB_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_MCP_RESOURCE: &str = "https://api.githubcopilot.com/mcp/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthToken {
    #[serde(default)]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct McpOAuthLoginOptions {
    pub client_id: Option<String>,
    pub client_secret_env: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct McpAuthFile {
    #[serde(default)]
    servers: BTreeMap<String, McpOAuthToken>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: String,
}

#[derive(Debug, Deserialize)]
struct ClientRegistrationResponse {
    client_id: String,
    #[serde(default)]
    #[serde(rename = "client_secret")]
    _client_secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthorizationServerMetadata {
    #[serde(default)]
    issuer: Option<String>,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    #[serde(rename = "scopes_supported")]
    _scopes_supported: Vec<String>,
}

pub struct McpTokenStore {
    path: PathBuf,
}

impl Default for McpTokenStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

impl McpTokenStore {
    pub fn default_path() -> PathBuf {
        crate::mcp::util::config_dir().join("mcp_auth.toml")
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load_token(&self, server_name: &str) -> Result<Option<McpOAuthToken>> {
        Ok(self.load_file()?.servers.remove(server_name))
    }

    pub fn save_token(&self, server_name: &str, token: McpOAuthToken) -> Result<()> {
        let mut file = self.load_file()?;
        file.servers.insert(server_name.to_string(), token);
        self.save_file(&file)
    }

    pub fn delete_token(&self, server_name: &str) -> Result<bool> {
        let mut file = self.load_file()?;
        let removed = file.servers.remove(server_name).is_some();
        self.save_file(&file)?;
        Ok(removed)
    }

    fn load_file(&self) -> Result<McpAuthFile> {
        if !self.path.exists() {
            return Ok(McpAuthFile::default());
        }
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("Failed to read {}", self.path.display()))?;
        toml::from_str(&text).with_context(|| format!("Invalid {}", self.path.display()))
    }

    fn save_file(&self, file: &McpAuthFile) -> Result<()> {
        let text = toml::to_string_pretty(file).context("Failed to serialize MCP auth")?;
        crate::fs::atomic_write(&self.path, text.as_bytes(), 0o600)
            .with_context(|| format!("Failed to write {}", self.path.display()))
    }
}

pub fn token_is_expired(token: &McpOAuthToken) -> bool {
    let Some(expires_at) = token.expires_at else {
        return false;
    };
    now_unix() + 60 >= expires_at
}

/// Refresh an expired MCP OAuth token when refresh metadata is available.
pub fn refresh_mcp_oauth_token(server_name: &str, token: &McpOAuthToken) -> Result<McpOAuthToken> {
    let Some(refresh_token) = token.refresh_token.as_deref() else {
        bail!(
            "MCP server {} OAuth token is expired and has no refresh token",
            server_name
        );
    };
    let Some(token_endpoint) = token.token_endpoint.as_deref() else {
        bail!(
            "MCP server {} OAuth token is expired and has no saved token endpoint",
            server_name
        );
    };
    let Some(client_id) = token.client_id.as_deref() else {
        bail!(
            "MCP server {} OAuth token is expired and has no saved client id",
            server_name
        );
    };

    let client_secret = token
        .client_secret_env
        .as_deref()
        .and_then(|name| std::env::var(name).ok());
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", client_id.to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    if let Some(resource) = &token.resource {
        form.push(("resource", resource.clone()));
    }

    let client = crate::proxy::apply_blocking_proxy_policy(reqwest::blocking::Client::builder())
        .build()
        // No `Client::new()` fallback — it panics on TLS/resolver init
        // failure and `panic = "abort"` turns that into a process kill.
        .context("failed to build MCP OAuth HTTP client")?;
    let resp = client
        .post(token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .context("Failed to refresh MCP OAuth token")?;
    if !resp.status().is_success() {
        bail!("MCP OAuth refresh failed: HTTP {}", resp.status());
    }
    let refreshed: TokenResponse = resp
        .json()
        .context("Failed to parse MCP OAuth refresh response")?;
    let mut new_token = token_from_response(
        refreshed,
        token.provider.clone(),
        token.issuer.clone(),
        token.resource.clone(),
        Some(client_id.to_string()),
        token.client_secret_env.clone(),
        Some(token_endpoint.to_string()),
    );
    if new_token.refresh_token.is_none() {
        new_token.refresh_token = token.refresh_token.clone();
    }
    McpTokenStore::default().save_token(server_name, new_token.clone())?;
    Ok(new_token)
}

pub fn login_mcp_oauth(
    server: &McpServerConfig,
    opts: McpOAuthLoginOptions,
) -> Result<McpOAuthToken> {
    let (url, auth) = match &server.config {
        McpTransportConfig::Http {
            url,
            auth: Some(McpHttpAuthConfig::OAuth(auth)),
            ..
        } => (url.as_str(), auth.clone()),
        McpTransportConfig::Http { .. } => {
            bail!(
                "MCP server '{}' is HTTP but does not use OAuth auth",
                server.name
            )
        }
        McpTransportConfig::Stdio { .. } => {
            bail!(
                "MCP server '{}' uses stdio; OAuth login only applies to HTTP MCP servers",
                server.name
            )
        }
    };

    if auth.provider.as_deref() == Some("github")
        && auth.issuer.is_none()
        && auth.resource.is_none()
        && opts.client_id.is_some()
    {
        let client_secret_env = opts.client_secret_env.or(auth.client_secret_env.clone());
        return login_github_oauth(
            &server.name,
            opts.client_id.as_deref().unwrap_or_default(),
            client_secret_env.as_deref(),
            if opts.scopes.is_empty() {
                &auth.scopes
            } else {
                &opts.scopes
            },
        );
    }

    let client = crate::proxy::apply_blocking_proxy_policy(reqwest::blocking::Client::builder())
        .build()
        // No `Client::new()` fallback — it panics on TLS/resolver init
        // failure and `panic = "abort"` turns that into a process kill.
        .context("failed to build MCP OAuth HTTP client")?;
    let discovered = discover_oauth_metadata(&client, url, &auth)?;
    let (redirect_uri, listener) = bind_callback_listener()?;
    let state = Uuid::new_v4().to_string();
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let challenge = base64_url_no_pad(&Sha256::digest(verifier.as_bytes()));

    let client_secret_env = opts.client_secret_env.or(auth.client_secret_env.clone());
    let client_secret = client_secret_env
        .as_deref()
        .and_then(|name| std::env::var(name).ok());
    let client_id = match opts.client_id.or(auth.client_id.clone()) {
        Some(id) => id,
        None => register_oauth_client(&client, &discovered.metadata, &redirect_uri)?.client_id,
    };
    let scopes = if !opts.scopes.is_empty() {
        opts.scopes
    } else if !auth.scopes.is_empty() {
        auth.scopes.clone()
    } else {
        Vec::new()
    };

    let mut authorize_url = Url::parse(&discovered.metadata.authorization_endpoint)
        .context("Invalid OAuth authorization endpoint")?;
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    if !scopes.is_empty() {
        authorize_url
            .query_pairs_mut()
            .append_pair("scope", &scopes.join(" "));
    }
    if let Some(resource) = &discovered.resource {
        authorize_url
            .query_pairs_mut()
            .append_pair("resource", resource);
    }

    println!(
        "  Browser didn't open? Open the URL below to authorize MCP server '{}':",
        server.name
    );
    println!("  {}", authorize_url);
    let _ = open_browser(authorize_url.as_str());

    let (code, returned_state) = await_oauth_callback(listener)?;
    if returned_state != state {
        bail!("OAuth state mismatch");
    }

    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("client_id", client_id.clone()),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    if let Some(resource) = &discovered.resource {
        form.push(("resource", resource.clone()));
    }

    let resp = client
        .post(&discovered.metadata.token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .context("Failed to exchange MCP OAuth code")?;
    if !resp.status().is_success() {
        bail!("MCP OAuth token exchange failed: HTTP {}", resp.status());
    }
    let token: TokenResponse = resp
        .json()
        .context("Failed to parse MCP OAuth token response")?;
    let token = token_from_response(
        token,
        auth.provider.unwrap_or_else(|| server.name.clone()),
        discovered.metadata.issuer,
        discovered.resource,
        Some(client_id),
        client_secret_env,
        Some(discovered.metadata.token_endpoint),
    );
    McpTokenStore::default().save_token(&server.name, token.clone())?;
    Ok(token)
}

pub fn login_github_oauth(
    server_name: &str,
    client_id: &str,
    client_secret_env: Option<&str>,
    scopes: &[String],
) -> Result<McpOAuthToken> {
    if client_id.trim().is_empty() {
        bail!("GitHub OAuth client id is required");
    }
    let Some(client_secret_env) = client_secret_env else {
        bail!(
            "GitHub MCP OAuth (the bring-your-own GitHub OAuth App flow) requires \
             --client-secret-env or auth.client_secret_env in mcp.json.\n\
             If '{server_name}' is a standard remote MCP server (most are), you probably \
             don't need this flow at all: drop --provider/--client-id (and the `provider`/\
             `client_id` fields in mcp.json) and run `atomcode mcp login {server_name}` — it \
             auto-discovers OAuth (RFC 9728/8414) and registers a client dynamically \
             (RFC 7591), no client secret needed."
        );
    };
    let client_secret = std::env::var(client_secret_env).with_context(|| {
        format!(
            "GitHub MCP OAuth client secret environment variable {} is not set",
            client_secret_env
        )
    })?;

    let (redirect_uri, listener) = bind_callback_listener()?;
    let state = Uuid::new_v4().to_string();
    let scope = if scopes.is_empty() {
        "repo read:org notifications".to_string()
    } else {
        scopes.join(" ")
    };

    let mut url = Url::parse(GITHUB_AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &scope)
        .append_pair("state", &state);

    println!("  Browser didn't open? Open the URL below to authorize GitHub MCP:");
    println!("  {}", url);
    let _ = open_browser(url.as_str());

    let (code, returned_state) = await_oauth_callback(listener)?;
    if returned_state != state {
        bail!("OAuth state mismatch");
    }

    let client = crate::proxy::apply_blocking_proxy_policy(reqwest::blocking::Client::builder())
        .build()
        // No `Client::new()` fallback — it panics on TLS/resolver init
        // failure and `panic = "abort"` turns that into a process kill.
        .context("failed to build MCP OAuth HTTP client")?;
    let resp = client
        .post(GITHUB_TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id),
            ("client_secret", &client_secret),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
        ])
        .send()
        .context("Failed to exchange GitHub OAuth code")?;
    if !resp.status().is_success() {
        bail!("GitHub OAuth token exchange failed: HTTP {}", resp.status());
    }
    let token: TokenResponse = resp
        .json()
        .context("Failed to parse GitHub OAuth token response")?;
    let token = token_from_response(
        token,
        "github".to_string(),
        Some("https://github.com".to_string()),
        Some(GITHUB_MCP_RESOURCE.to_string()),
        Some(client_id.to_string()),
        Some(client_secret_env.to_string()),
        Some(GITHUB_TOKEN_URL.to_string()),
    );
    McpTokenStore::default().save_token(server_name, token.clone())?;
    Ok(token)
}

struct DiscoveredOAuth {
    metadata: AuthorizationServerMetadata,
    resource: Option<String>,
}

fn discover_oauth_metadata(
    client: &reqwest::blocking::Client,
    mcp_url: &str,
    auth: &McpOAuthConfig,
) -> Result<DiscoveredOAuth> {
    if let Some(issuer) = &auth.issuer {
        let metadata = fetch_authorization_server_metadata(client, issuer)?;
        return Ok(DiscoveredOAuth {
            metadata,
            resource: auth.resource.clone().or_else(|| Some(mcp_url.to_string())),
        });
    }

    let resource_metadata_urls = discover_resource_metadata_urls(client, mcp_url, auth)?;
    let mut prm: Option<ProtectedResourceMetadata> = None;
    let mut last_err = None;
    for url in &resource_metadata_urls {
        match client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json::<ProtectedResourceMetadata>())
        {
            Ok(metadata) => {
                prm = Some(metadata);
                break;
            }
            Err(e) => last_err = Some((url.clone(), e)),
        }
    }
    let prm: ProtectedResourceMetadata = prm.ok_or_else(|| match last_err {
        Some((url, e)) => {
            anyhow::anyhow!("MCP OAuth resource metadata request failed for {url}: {e}")
        }
        None => anyhow::anyhow!("MCP OAuth resource metadata: no candidate URL to try"),
    })?;
    let auth_server = prm.authorization_servers.first().ok_or_else(|| {
        anyhow::anyhow!("MCP OAuth resource metadata has no authorization_servers")
    })?;
    let metadata = fetch_authorization_server_metadata(client, auth_server)?;
    Ok(DiscoveredOAuth {
        metadata,
        resource: auth
            .resource
            .clone()
            .or(prm.resource)
            .or_else(|| Some(mcp_url.to_string())),
    })
}

/// Candidate RFC 9728 protected-resource-metadata URLs to try, best first.
/// A `WWW-Authenticate: … resource_metadata="…"` challenge (or an explicit
/// `auth.resource` pointing at a `.well-known` doc) pins the exact URL; otherwise
/// we fall back to all path shapes of the well-known document (insert / append /
/// origin-root — see [`well_known_metadata_urls`]).
fn discover_resource_metadata_urls(
    client: &reqwest::blocking::Client,
    mcp_url: &str,
    auth: &McpOAuthConfig,
) -> Result<Vec<String>> {
    if let Some(resource) = &auth.resource {
        if resource.contains("/.well-known/") {
            return Ok(vec![resource.clone()]);
        }
    }

    let probe = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": super::types::initialize_params()
    });
    if let Ok(resp) = client
        .post(mcp_url)
        .header("Accept", "application/json, text/event-stream")
        .json(&probe)
        .send()
    {
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            if let Some(header) = resp
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
            {
                if let Some(url) = parse_www_authenticate_resource_metadata(header) {
                    return Ok(vec![url]);
                }
            }
        }
    }

    let candidates = well_known_metadata_urls(mcp_url, "oauth-protected-resource");
    if candidates.is_empty() {
        bail!("Invalid MCP server URL: {mcp_url}");
    }
    Ok(candidates)
}

pub fn parse_www_authenticate_resource_metadata(header: &str) -> Option<String> {
    for part in header.split(',') {
        let part = part.trim().strip_prefix("Bearer ").unwrap_or(part.trim());
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("resource_metadata") {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Candidate `.well-known` metadata URLs for `base_url` + a well-known `suffix`
/// (e.g. `oauth-authorization-server`), most-canonical first, deduped.
///
/// For a base WITH a path (`https://host/docs`), RFC 8414 §3.1 / RFC 9728 §3.1
/// put the document at the path-INSERT form `https://host/.well-known/<suffix>/docs`.
/// atomcode previously only built the naive APPEND (`https://host/docs/.well-known/<suffix>`),
/// which 404s on gateways that route by sub-path (e.g. `mcp.espressif.com/docs`,
/// which serves `…/.well-known/oauth-protected-resource/docs`). We now try, in
/// order: insert (spec-canonical) → append (OIDC/legacy) → origin-root. A base
/// with no path collapses all three to the single origin-root form, so existing
/// servers see identical behaviour. Mirrors oh-my-pi's `buildWellKnownUrls`.
fn well_known_metadata_urls(base_url: &str, suffix: &str) -> Vec<String> {
    let Ok(parsed) = Url::parse(base_url) else {
        return Vec::new();
    };
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/'); // "" when the base is origin-only
    let mut candidates = Vec::new();
    if !path.is_empty() {
        // RFC 8414 §3.1 path-ful (insert the well-known segment before the path):
        candidates.push(format!("{origin}/.well-known/{suffix}{path}"));
        // OIDC Discovery / legacy (append to the full issuer):
        candidates.push(format!("{origin}{path}/.well-known/{suffix}"));
    }
    // Origin-root default (also the RFC 8414 form when the issuer has no path).
    let root = format!("{origin}/.well-known/{suffix}");
    if !candidates.contains(&root) {
        candidates.push(root);
    }
    candidates
}

fn fetch_authorization_server_metadata(
    client: &reqwest::blocking::Client,
    issuer: &str,
) -> Result<AuthorizationServerMetadata> {
    if issuer.contains("/.well-known/") {
        return fetch_metadata_url(client, issuer);
    }
    // Try the RFC 8414 form AND the OIDC form, each across all path shapes.
    let mut last_err = None;
    for suffix in ["oauth-authorization-server", "openid-configuration"] {
        for candidate in well_known_metadata_urls(issuer, suffix) {
            match fetch_metadata_url(client, &candidate) {
                Ok(metadata) => return Ok(metadata),
                Err(e) => last_err = Some(e),
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No OAuth metadata URL candidates")))
}

fn fetch_metadata_url(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<AuthorizationServerMetadata> {
    client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .with_context(|| format!("Failed to fetch OAuth authorization server metadata from {url}"))?
        .error_for_status()
        .with_context(|| format!("OAuth authorization server metadata request failed for {url}"))?
        .json()
        .with_context(|| format!("Failed to parse OAuth authorization server metadata from {url}"))
}

fn register_oauth_client(
    client: &reqwest::blocking::Client,
    metadata: &AuthorizationServerMetadata,
    redirect_uri: &str,
) -> Result<ClientRegistrationResponse> {
    let Some(registration_endpoint) = metadata.registration_endpoint.as_deref() else {
        bail!(
            "MCP OAuth requires a pre-registered client_id because the authorization server \
             does not support dynamic client registration (RFC 7591). \
             Add a pre-registered client_id to auth.client_id in your .mcp.json and try again."
        );
    };
    let resp = client
        .post(registration_endpoint)
        .header("Accept", "application/json")
        .json(&json!({
            "client_name": "AtomCode",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .context("Failed to dynamically register MCP OAuth client")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
            bail!(
                "MCP OAuth dynamic client registration failed: HTTP {status} — \
                 the authorization server rejected the request. \
                 Add a pre-registered client_id to auth.client_id \
                 in your .mcp.json and try again.\n\
                 Response: {body}"
            );
        }
        bail!("MCP OAuth dynamic client registration failed: HTTP {status}\nResponse: {body}");
    }
    resp.json()
        .context("Failed to parse MCP OAuth dynamic client registration response")
}

fn token_from_response(
    token: TokenResponse,
    provider: String,
    issuer: Option<String>,
    resource: Option<String>,
    client_id: Option<String>,
    client_secret_env: Option<String>,
    token_endpoint: Option<String>,
) -> McpOAuthToken {
    McpOAuthToken {
        provider,
        issuer,
        token_type: token.token_type.unwrap_or_else(default_token_type),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token.expires_in.map(|seconds| now_unix() + seconds),
        scopes: token.scope.split_whitespace().map(str::to_string).collect(),
        resource,
        client_id,
        client_secret_env,
        token_endpoint,
    }
}

fn bind_callback_listener() -> Result<(String, TcpListener)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("Failed to bind local OAuth callback listener")?;
    let port = listener.local_addr()?.port();
    Ok((format!("http://127.0.0.1:{}/callback", port), listener))
}

fn await_oauth_callback(listener: TcpListener) -> Result<(String, String)> {
    let (mut stream, _) = listener
        .accept()
        .context("Failed to accept OAuth callback")?;
    let mut buf = [0_u8; 4096];
    let n = stream
        .read(&mut buf)
        .context("Failed to read OAuth callback")?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("Invalid OAuth callback request"))?;
    let url =
        Url::parse(&format!("http://127.0.0.1{}", path)).context("Invalid OAuth callback URL")?;
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| anyhow::anyhow!("OAuth callback did not include code"))?;
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| anyhow::anyhow!("OAuth callback did not include state"))?;

    let body = "Authorization complete. You can close this tab.";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
    Ok((code, state))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    match bytes.len() - i {
        1 => {
            let n = (bytes[i] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        }
        2 => {
            let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        _ => {}
    }
    out
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    std::process::Command::new("cmd")
        .raw_arg(format!("/C start \"\" \"{}\"", url))
        // Suppress cmd.exe's own window flash; the browser it launches is unaffected.
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        base64_url_no_pad, login_github_oauth, parse_www_authenticate_resource_metadata,
        well_known_metadata_urls, McpOAuthToken, McpTokenStore,
    };

    #[test]
    fn github_flow_without_secret_signposts_the_discovery_flow() {
        // The bring-your-own GitHub-App flow legitimately needs a client secret;
        // when it's missing the error must POINT the user at the plain discovery
        // login (which needs no secret) instead of dead-ending. Bails before any
        // network/browser work, so this is a pure error-shape check.
        let err = login_github_oauth("espressif-documentation", "cid", None, &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("mcp login espressif-documentation"),
            "should signpost the discovery login: {err}"
        );
        assert!(
            err.contains("RFC 7591") || err.contains("dynamically"),
            "should mention dynamic registration: {err}"
        );
    }

    #[test]
    fn well_known_urls_path_bearing_issuer_includes_rfc8414_insert_form() {
        // The espressif case: issuer/base has a `/docs` path. The RFC 8414 §3.1
        // insert form MUST be tried (it's the only one the server serves) and be
        // FIRST (spec-canonical), alongside the legacy append + origin-root.
        let urls = well_known_metadata_urls(
            "https://mcp.espressif.com/docs",
            "oauth-authorization-server",
        );
        assert_eq!(
            urls,
            vec![
                "https://mcp.espressif.com/.well-known/oauth-authorization-server/docs".to_string(),
                "https://mcp.espressif.com/docs/.well-known/oauth-authorization-server".to_string(),
                "https://mcp.espressif.com/.well-known/oauth-authorization-server".to_string(),
            ]
        );
        // Resource-metadata suffix gets the same treatment (the fallback path).
        assert!(well_known_metadata_urls(
            "https://mcp.espressif.com/docs",
            "oauth-protected-resource"
        )
        .contains(
            &"https://mcp.espressif.com/.well-known/oauth-protected-resource/docs".to_string()
        ));
    }

    #[test]
    fn well_known_urls_multi_segment_path_uses_full_path_and_is_well_formed() {
        // Multi-segment base (e.g. an endpoint URL used for the resource-metadata
        // fallback). We preserve the FULL path per RFC 9728 §3.1 — no dropped
        // segments, no double slashes. (The common case still comes via the exact
        // WWW-Authenticate `resource_metadata` URL, so this is a best-effort probe.)
        let urls = well_known_metadata_urls("https://host/api/v1/mcp", "oauth-protected-resource");
        assert_eq!(
            urls,
            vec![
                "https://host/.well-known/oauth-protected-resource/api/v1/mcp".to_string(),
                "https://host/api/v1/mcp/.well-known/oauth-protected-resource".to_string(),
                "https://host/.well-known/oauth-protected-resource".to_string(),
            ]
        );
        assert!(urls.iter().all(|u| !u.contains("//.well-known")
            && !u.contains(".well-known//")
            && u.matches("://").count() == 1));
        // A malformed base yields no candidates (caller turns this into an error).
        assert!(well_known_metadata_urls("not a url", "oauth-authorization-server").is_empty());
    }

    #[test]
    fn well_known_urls_pathless_issuer_collapses_to_origin_root() {
        // No path → the three shapes collapse to the single origin-root form, so
        // existing (path-less) servers see byte-identical behaviour.
        for base in ["https://mcp.example.com", "https://mcp.example.com/"] {
            assert_eq!(
                well_known_metadata_urls(base, "oauth-authorization-server"),
                vec!["https://mcp.example.com/.well-known/oauth-authorization-server".to_string()]
            );
        }
    }

    #[test]
    fn base64_url_omits_padding() {
        assert_eq!(base64_url_no_pad(b"abc"), "YWJj");
        assert_eq!(base64_url_no_pad(b"ab"), "YWI");
        assert_eq!(base64_url_no_pad(b"a"), "YQ");
    }

    #[test]
    fn www_authenticate_resource_metadata_is_parsed() {
        let header = r#"Bearer realm="mcp", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#;
        assert_eq!(
            parse_www_authenticate_resource_metadata(header).as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn token_store_round_trips_server_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_auth.toml");
        let store = McpTokenStore::new(path.clone());
        let token = McpOAuthToken {
            provider: "github".to_string(),
            issuer: Some("https://github.com".to_string()),
            token_type: "Bearer".to_string(),
            access_token: "token".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec!["repo".to_string()],
            resource: Some("https://api.githubcopilot.com/mcp/".to_string()),
            client_id: Some("client".to_string()),
            client_secret_env: None,
            token_endpoint: Some("https://github.com/login/oauth/access_token".to_string()),
        };
        store.save_token("github", token).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "new OAuth token stores must be private");

            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let loaded = store.load_token("github").unwrap().unwrap();
        assert_eq!(loaded.provider, "github");
        assert_eq!(loaded.access_token, "token");
        assert_eq!(loaded.scopes, vec!["repo"]);
        assert!(store.delete_token("github").unwrap());
        assert!(store.load_token("github").unwrap().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "rewriting an existing OAuth token store must tighten its permissions"
            );
        }
    }
}
