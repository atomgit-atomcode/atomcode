pub mod claude;
pub mod ollama;
pub mod openai;
pub mod retry;

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

    fn availability_error(&self) -> Option<&str> {
        None
    }
}

/// Shared HTTP client with common timeouts and User-Agent.
/// `ua_override` comes from `ProviderConfig::user_agent`; falls back to the
/// workspace-wide `ATOMCODE_USER_AGENT` (`AtomCode/<version>`), required by
/// AtomGit's API gateway — see the constant's doc-comment.
pub(super) fn build_http_client(ua_override: Option<&str>) -> reqwest::Client {
    let ua = ua_override.unwrap_or(crate::ATOMCODE_USER_AGENT);
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
    let mut config = if config.api_key.is_none() && config.provider_type != "ollama" {
        let mut c = config.clone();
        c.api_key = Some(load_auth_token()?);
        c
    } else {
        config.clone()
    };
    // Sanitize api_key at load time so the user sees an actionable
    // config error instead of a cryptic "request body must be
    // cloneable" panic downstream. Trailing `\n` from paste-from-web
    // is the single most common trigger: `http::HeaderValue` rejects
    // control chars, `reqwest::RequestBuilder::header` silently
    // stashes the error, and `try_clone()` panics later when retry
    // tries to repeat the request.
    if let Some(key) = config.api_key.as_deref() {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "API key for provider type '{}' is empty (or whitespace only) \
                 — check the value in your config.toml",
                config.provider_type
            );
        }
        if trimmed.chars().any(|c| c.is_control()) {
            anyhow::bail!(
                "API key for provider type '{}' contains control characters \
                 (newline/tab/etc.) — re-copy the key without surrounding \
                 whitespace",
                config.provider_type
            );
        }
        if trimmed.len() != key.len() {
            // Silently strip surrounding whitespace so a harmless
            // paste artefact doesn't block the request.
            config.api_key = Some(trimmed.to_string());
        }
    }
    match config.provider_type.as_str() {
        "claude" => Ok(Box::new(claude::ClaudeProvider::new(&config)?)),
        "openai" => Ok(Box::new(openai::OpenAiProvider::new(&config)?)),
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new(&config)?)),
        other => anyhow::bail!("Unknown provider type: {}", other),
    }
}

pub fn unavailable_provider(reason: impl Into<String>) -> Box<dyn LlmProvider> {
    Box::new(UnavailableProvider {
        reason: reason.into(),
    })
}

struct UnavailableProvider {
    reason: String,
}

#[async_trait]
impl LlmProvider for UnavailableProvider {
    fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        anyhow::bail!("{}", self.reason);
    }

    fn model_name(&self) -> &str {
        ""
    }

    fn availability_error(&self) -> Option<&str> {
        Some(&self.reason)
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
    let builder = client
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ]);
    let policy = crate::provider::retry::RetryPolicy::default_policy();
    let resp = crate::provider::retry::send_with_retry_blocking(builder, &policy)
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
    let _ = crate::auth::write_auth_file_secure(auth_path, &content);

    Ok(access_token)
}

#[cfg(test)]
mod tests {
    use super::unavailable_provider;

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

    use crate::config::provider::ProviderConfig;

    fn cfg(provider_type: &str, api_key: &str) -> ProviderConfig {
        ProviderConfig {
            provider_type: provider_type.to_string(),
            api_key: Some(api_key.to_string()),
            model: "m".to_string(),
            base_url: Some("http://127.0.0.1:1/".to_string()),
            system_prompt: None,
            user_agent: None,
            context_window: 8000,
            max_tokens: None,
            ephemeral: false,
        }
    }

    #[test]
    fn unavailable_provider_reports_reason() {
        let provider = unavailable_provider("未配置 provider");
        assert_eq!(provider.model_name(), "");
        assert_eq!(provider.availability_error(), Some("未配置 provider"));
    }

    /// INTERNAL control characters (vs surrounding whitespace, which
    /// is silently trimmed) must fail at config-load time with an
    /// actionable error — not at request time as a cryptic try_clone
    /// panic. These are genuinely suspicious values (partial paste,
    /// rendering glitch, someone editing config.toml in an editor
    /// that inserted a CR) and cannot appear in a valid API key.
    #[test]
    fn create_provider_rejects_api_key_with_internal_control_chars() {
        let result = super::create_provider(&cfg("openai", "sk-ab\nc"));
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected Err for api_key with internal \\n"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("control character"),
            "expected control-char error, got: {}",
            msg
        );
    }

    /// Trailing `\n` (paste-from-web artefact) gets silently trimmed.
    /// The user's config remains functional without needing a manual
    /// edit — this is the user-friendly path for the common case.
    #[test]
    fn create_provider_silently_trims_trailing_newline() {
        let result = super::create_provider(&cfg("openai", "sk-abc\n"));
        assert!(
            result.is_ok(),
            "trailing \\n should be trimmed silently, got: {:?}",
            result.err().map(|e| e.to_string())
        );
    }

    #[test]
    fn create_provider_rejects_empty_or_whitespace_api_key() {
        let result = super::create_provider(&cfg("openai", "   "));
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected Err for whitespace-only api_key"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("empty") || msg.contains("whitespace"),
            "expected empty/whitespace error, got: {}",
            msg
        );
    }

    /// Harmless surrounding whitespace (the typical copy-paste
    /// artefact) gets trimmed — no error, the provider constructs
    /// cleanly with the trimmed key.
    #[test]
    fn create_provider_silently_trims_surrounding_whitespace() {
        let result = super::create_provider(&cfg("openai", "  sk-abc  "));
        assert!(
            result.is_ok(),
            "trimmable key should be accepted, got: {:?}",
            result.err().map(|e| e.to_string())
        );
    }
}
