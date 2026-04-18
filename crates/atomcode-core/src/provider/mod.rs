pub mod claude;
pub mod ollama;
pub mod openai;

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;

use crate::config::provider::ProviderConfig;
use crate::conversation::message::Message;
use crate::stream::StreamEvent;
use crate::tool::ToolDef;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    fn model_name(&self) -> &str;
}

/// Shared HTTP client with common timeouts and User-Agent.
/// `ua_override` comes from `ProviderConfig::user_agent`; falls back to `atomcode/<version>`.
pub(super) fn build_http_client(ua_override: Option<&str>) -> reqwest::Client {
    let ua = ua_override.unwrap_or(concat!("atomcode/", env!("CARGO_PKG_VERSION")));
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .user_agent(ua)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Factory: create the right provider from config.
/// If `api_key` is `None`, automatically loads from `~/.atomcode/auth.toml`
/// (with token refresh if expired).
pub fn create_provider(config: &ProviderConfig) -> Result<Box<dyn LlmProvider>> {
    let config = if config.api_key.is_none() && config.provider_type != "ollama" {
        let mut c = config.clone();
        c.api_key = Some(load_auth_token()?);
        c
    } else {
        config.clone()
    };
    match config.provider_type.as_str() {
        "claude" => Ok(Box::new(claude::ClaudeProvider::new(&config)?)),
        "openai" => Ok(Box::new(openai::OpenAiProvider::new(&config)?)),
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new(&config)?)),
        other => anyhow::bail!("Unknown provider type: {}", other),
    }
}

// ── auth.toml token loading ──

/// OAuth constants (shared with atomcode-tui / atomcode-cli).
const OAUTH_CLIENT_ID: &str = "b9956e5327e544578128af8979ba3ccb";
const OAUTH_CLIENT_SECRET: &str = "756ef00061884c7aa1ac64bd4eae3be7";
const OAUTH_TOKEN_URL: &str = "https://atomgit.com/oauth/token";

/// Minimal auth.toml representation.
#[derive(serde::Deserialize)]
struct StoredAuth {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    created_at: i64,
}

/// Token endpoint response.
#[derive(serde::Deserialize)]
struct RefreshResponse {
    #[serde(alias = "access_token", alias = "accessToken")]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Read a valid access token from `~/.atomcode/auth.toml`.
/// Automatically refreshes expired tokens via the OAuth refresh_token flow.
fn load_auth_token() -> Result<String> {
    let auth_path = crate::auth::auth_file_path();
    let content = std::fs::read_to_string(&auth_path)
        .map_err(|_| anyhow::anyhow!("Not logged in — please use /login"))?;
    let auth: StoredAuth = toml::from_str(&content)
        .map_err(|_| anyhow::anyhow!("Invalid auth.toml — please use /login"))?;

    // Check expiry (5-minute safety margin)
    if let Some(expires_in) = auth.expires_in {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        if now >= auth.created_at + expires_in - 300 {
            // Token expired — try refresh
            if let Some(ref rt) = auth.refresh_token {
                return refresh_and_save(rt, &auth_path);
            }
            anyhow::bail!("Token expired — please use /login");
        }
    }

    Ok(auth.access_token)
}

/// Exchange refresh_token for a new access_token, save updated auth.toml.
fn refresh_and_save(refresh_token: &str, auth_path: &std::path::Path) -> Result<String> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|e| anyhow::anyhow!("Token refresh failed: {} — please /login", e))?;

    if !resp.status().is_success() {
        anyhow::bail!("Token refresh failed ({}) — please /login", resp.status());
    }

    let token: RefreshResponse = resp.json()
        .map_err(|e| anyhow::anyhow!("Token refresh parse error: {} — please /login", e))?;

    let access_token = token.access_token
        .ok_or_else(|| anyhow::anyhow!("Refresh response missing access_token — please /login"))?;

    // Save updated auth.toml
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let new_rt = token.refresh_token.as_deref().unwrap_or(refresh_token);
    let mut content = format!(
        "access_token = \"{}\"\ncreated_at = {}\nrefresh_token = \"{}\"\n",
        access_token, now, new_rt,
    );
    if let Some(e) = token.expires_in {
        content.push_str(&format!("expires_in = {}\n", e));
    }
    let _ = std::fs::write(auth_path, content);

    Ok(access_token)
}

#[cfg(test)]
mod tests {
    /// Test that auth token is loaded from the correct unified path.
    /// This prevents regressions where OAuth login token persistence breaks
    /// after program restart due to path mismatch.
    #[test]
    fn test_auth_token_path_consistency() {
        // Both paths should resolve to the same location: ~/.atomcode/auth.toml
        let auth_module_path = crate::auth::auth_file_path();
        let expected_path = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".atomcode")
            .join("auth.toml");

        assert_eq!(auth_module_path, expected_path,
            "auth_file_path() should always return ~/.atomcode/auth.toml");

        // Verify the path ends with the expected directory structure
        assert!(auth_module_path.ends_with(".atomcode/auth.toml") ||
                auth_module_path.ends_with(".atomcode\\auth.toml"), // Windows compatibility
                "Path should end with .atomcode/auth.toml, got: {}",
                auth_module_path.display());
    }
}
