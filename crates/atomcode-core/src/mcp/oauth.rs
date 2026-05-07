//! OAuth token storage and provider-specific login helpers for remote MCP.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

const GITHUB_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_MCP_RESOURCE: &str = "https://api.githubcopilot.com/mcp/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthToken {
    pub provider: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct McpAuthFile {
    #[serde(default)]
    servers: BTreeMap<String, McpOAuthToken>,
}

pub struct McpTokenStore {
    path: PathBuf,
}

impl McpTokenStore {
    pub fn default_path() -> PathBuf {
        crate::config::Config::config_dir().join("mcp_auth.toml")
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default() -> Self {
        Self::new(Self::default_path())
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
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(file).context("Failed to serialize MCP auth")?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("Failed to write {}", self.path.display()))
    }
}

pub fn token_is_expired(token: &McpOAuthToken) -> bool {
    let Some(expires_at) = token.expires_at else {
        return false;
    };
    now_unix() + 60 >= expires_at
}

pub fn login_github_oauth(
    server_name: &str,
    client_id: &str,
    scopes: &[String],
) -> Result<McpOAuthToken> {
    if client_id.trim().is_empty() {
        bail!("GitHub OAuth client id is required");
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("Failed to bind local OAuth callback listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{}/callback", port);
    let state = Uuid::new_v4().to_string();
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let challenge = base64_url_no_pad(&Sha256::digest(verifier.as_bytes()));
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
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    println!("  Browser didn't open? Open the URL below to authorize GitHub MCP:");
    println!("  {}", url);
    let _ = open_browser(url.as_str());

    let (code, returned_state) = await_oauth_callback(listener)?;
    if returned_state != state {
        bail!("OAuth state mismatch");
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<i64>,
        #[serde(default)]
        scope: String,
    }

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(GITHUB_TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ])
        .send()
        .context("Failed to exchange GitHub OAuth code")?;
    if !resp.status().is_success() {
        bail!("GitHub OAuth token exchange failed: HTTP {}", resp.status());
    }
    let token: TokenResponse = resp
        .json()
        .context("Failed to parse GitHub OAuth token response")?;
    let token = McpOAuthToken {
        provider: "github".to_string(),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token.expires_in.map(|seconds| now_unix() + seconds),
        scopes: token.scope.split_whitespace().map(str::to_string).collect(),
        resource: Some(GITHUB_MCP_RESOURCE.to_string()),
    };
    McpTokenStore::default().save_token(server_name, token.clone())?;
    Ok(token)
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
    std::process::Command::new("open").arg(url).spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("cmd")
        .raw_arg(format!("/C start \"\" \"{}\"", url))
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{base64_url_no_pad, McpOAuthToken, McpTokenStore};

    #[test]
    fn base64_url_omits_padding() {
        assert_eq!(base64_url_no_pad(b"abc"), "YWJj");
        assert_eq!(base64_url_no_pad(b"ab"), "YWI");
        assert_eq!(base64_url_no_pad(b"a"), "YQ");
    }

    #[test]
    fn token_store_round_trips_server_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = McpTokenStore::new(dir.path().join("mcp_auth.toml"));
        let token = McpOAuthToken {
            provider: "github".to_string(),
            access_token: "token".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: vec!["repo".to_string()],
            resource: Some("https://api.githubcopilot.com/mcp/".to_string()),
        };
        store.save_token("github", token).unwrap();

        let loaded = store.load_token("github").unwrap().unwrap();
        assert_eq!(loaded.provider, "github");
        assert_eq!(loaded.access_token, "token");
        assert_eq!(loaded.scopes, vec!["repo"]);
        assert!(store.delete_token("github").unwrap());
        assert!(store.load_token("github").unwrap().is_none());
    }
}
