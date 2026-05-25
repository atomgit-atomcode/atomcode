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

/// Per-provider strategy for echoing back `reasoning_content` from historical
/// assistant tool_call messages on subsequent requests.
///
/// Different thinking-model APIs contradict each other:
/// - **Moonshot Kimi K2-thinking / K2.5 / K2.6** — MUST echo reasoning_content
///   on every assistant tool_call message in history; otherwise returns 400
///   with "thinking is enabled but reasoning_content is missing in assistant
///   tool call message at index N".
/// - **DeepSeek-R1 / deepseek-reasoner (V3 family)** — MUST NOT include
///   reasoning_content in subsequent requests; returns 400 if present.
/// - **DeepSeek V4 family (`deepseek-v4*`, thinking mode)** — opposite of V3:
///   MUST echo reasoning_content on every assistant tool_call message, or the
///   API returns 400 "The `reasoning_content` in the thinking mode must be
///   passed back to the API".
/// - **MiniMax-M2 (default)** — thinking is embedded in content as
///   `<think>...</think>`, goes through the plain-text path; no separate field
///   handling needed.
/// - **Anthropic** — uses a different `thinking` block structure in its own
///   messages format; not affected by this policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningPolicy {
    /// Echo the stored reasoning_content back on every assistant tool_call
    /// message. When the stored value is None, emit an empty string so the
    /// field is always present (some providers treat a missing field as
    /// "field absent" and error out even with empty content).
    Include,
    /// Strip reasoning_content — never emit the field in outbound requests,
    /// even if we captured it from the stream. This is the safe default.
    Exclude,
}

/// Sentinel emitted on outbound `reasoning_content` when we have nothing to
/// echo (cross-provider handoff, pre-fix session, non-thinking model that
/// still tool-called) but the receiving API requires the field to be
/// non-empty (DeepSeek V4 thinking mode rejects empty strings).
///
/// `TurnRunner::Done` checks reasoning_buf against this exact value and
/// refuses to promote it back into the assistant text channel — without that
/// gate, a buggy gateway echoing our placeholder caused silent
/// `Nailed it · 0 tok` mid-task stops (user reported `(no reasoning
/// recorded)` showing up as the only assistant output after 17 reading
/// rounds).
pub const REASONING_PLACEHOLDER: &str = "(no reasoning recorded)";

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

    /// Whether historical `reasoning_content` should be echoed back to the
    /// provider on subsequent requests. Default `Exclude` (safe for all
    /// providers that don't demand it). Providers that hit a thinking-model
    /// API should override.
    fn reasoning_history_policy(&self) -> ReasoningPolicy {
        ReasoningPolicy::Exclude
    }
}

/// Shared HTTP client with common timeouts and User-Agent.
/// `ua_override` comes from `ProviderConfig::user_agent`; falls back to the
/// workspace-wide `ATOMCODE_USER_AGENT` (`atomcode/<version>`) — see the
/// constant's doc-comment for why lowercasing matters on the LLM gateway.
/// `skip_tls_verify` disables TLS certificate verification when true.
pub(super) fn build_http_client(ua_override: Option<&str>, skip_tls_verify: bool) -> reqwest::Client {
    let ua = ua_override.unwrap_or(crate::ATOMCODE_USER_AGENT);
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        // Total request timeout. Long edit_file / write_file generations
        // on self-hosted GLM/Qwen NPU clusters can stream for 5-15 min on
        // 100+ line file rewrites (decode 24-30 tok/s × 5K-10K tokens).
        // The previous 300 s ceiling killed Turn 6 of the 5/12 atomgr
        // session mid-flight ("Endpoint terminated the response stream").
        // 1800 s (30 min) is wide enough for any plausible single
        // generation while still bounding truly-stuck connections;
        // SSE-level idle timeout (openai.rs, 120 s) catches dead streams
        // before the request-level timeout.
        .timeout(std::time::Duration::from_secs(1800))
        .user_agent(ua);

    if skip_tls_verify {
        builder = builder.danger_accept_invalid_certs(true);
    }

    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Distill an upstream HTTP error body down to a human-readable message.
///
/// Gateways wrap the real message in JSON envelopes: AtomCode returns
/// `{"detail":{"code":"X","message":"Y"}}`, FastAPI defaults to
/// `{"detail":"Y"}`, OpenAI/Anthropic use `{"error":{"message":"Y",...}}`.
/// Surfacing the raw body shows users the envelope instead of `Y`. Try the
/// known shapes; fall back to the original body when nothing matches.
/// Format an upstream HTTP error for the end-user line.
///
/// 429 (rate limit) errors compress to `[429] <message>` — the URL
/// is the user's own configured gateway and stays the same all
/// session, and the status text "Too Many Requests" is redundant
/// once they see `[429]`. The real signal is the inner message
/// ("codingplan rate limit exceeded for type='Pro'", "No deployments
/// available", etc.) — making that the leading content saves the
/// user a line of horizontal scrolling past the URL.
///
/// All other statuses keep the verbose `API error (status) at \`url\`:\n<msg>`
/// form because URL + status text are real diagnostics for 4xx/5xx
/// triage (404 path wrong, 401 auth issue, 500 upstream — the URL
/// disambiguates).
///
/// Downstream matcher invariant: `is_rate_limited_error` in
/// `agent/mod.rs` scans for "429" / "rate" / "Too Many". The
/// compact form keeps "429" inline AND the inner `<msg>` typically
/// carries "rate" / "rate limit" — so the auto-retry backoff path
/// fires identically against the new format.
pub(super) fn format_http_error(
    status: reqwest::StatusCode,
    url: &str,
    msg: &str,
) -> String {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        format!("[429] {}", msg)
    } else {
        format!("API error ({}) at `{}`:\n{}", status, url, msg)
    }
}

pub(super) fn extract_error_message(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(detail) = v.get("detail") {
            if let Some(msg) = detail.get("message").and_then(|m| m.as_str()) {
                return msg.to_string();
            }
            if let Some(s) = detail.as_str() {
                return s.to_string();
            }
        }
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod extract_error_message_tests {
    use super::extract_error_message;

    #[test]
    fn openai_envelope_codingplan_rate_limit() {
        // The exact shape users have been hitting since CodingPlan
        // enforcement landed: an OpenAI-style `{"error":{"message"...}}`
        // wrapper around the rate-limit description. Surfacing the raw
        // JSON dumped a wall of `"type":"None","param":"None","code":...`
        // noise on top of the real reason; we want only the inner
        // message — "codingplan rate limit exceeded for type='Pro'".
        let body = r#"{"error":{"message":"codingplan rate limit exceeded for type='Pro'","type":"auth_error","param":"None","code":"429"}}"#;
        assert_eq!(
            extract_error_message(body),
            "codingplan rate limit exceeded for type='Pro'"
        );
    }

    #[test]
    fn openai_envelope_no_deployments_available() {
        // LiteLLM-style deployment-pool-exhaustion 429. Same OpenAI
        // wrapper shape; long inner message including a cooldown_list
        // we want intact (it tells the user which deployments are
        // currently unhealthy + the suggested retry wait).
        let body = r#"{"error":{"message":"No deployments available for selected model. Try again in 30 seconds. Passed model=deepseek-v4-flash.","type":"None","param":"None","code":"429"}}"#;
        let out = extract_error_message(body);
        assert!(out.starts_with("No deployments available"));
        assert!(out.contains("Try again in 30 seconds"));
        assert!(!out.contains("\"code\""), "envelope keys must not leak");
    }

    #[test]
    fn atomcode_detail_envelope() {
        // AtomCode's own gateway wraps under `detail.message` — the
        // shape this function was originally written for. Keep it
        // pinned so a future refactor that drops the `error.*` branch
        // doesn't silently regress this older shape.
        let body = r#"{"detail":{"code":"X","message":"detail message body"}}"#;
        assert_eq!(extract_error_message(body), "detail message body");
    }

    #[test]
    fn fastapi_string_detail() {
        let body = r#"{"detail":"plain string detail"}"#;
        assert_eq!(extract_error_message(body), "plain string detail");
    }

    #[test]
    fn top_level_message() {
        let body = r#"{"message":"top-level message"}"#;
        assert_eq!(extract_error_message(body), "top-level message");
    }

    #[test]
    fn non_json_body_passes_through_trimmed() {
        // Some gateways return text/plain on errors. Should NOT crash;
        // returns the body as-is (trimmed) so the user still sees
        // something actionable.
        assert_eq!(
            extract_error_message("  upstream timeout  "),
            "upstream timeout"
        );
    }
}

#[cfg(test)]
mod format_http_error_tests {
    use super::format_http_error;
    use reqwest::StatusCode;

    #[test]
    fn rate_limit_compresses_to_bracketed_form() {
        assert_eq!(
            format_http_error(
                StatusCode::TOO_MANY_REQUESTS,
                "https://llm-api.atomgit.com/v1/chat/completions",
                "codingplan rate limit exceeded for type='Pro'",
            ),
            "[429] codingplan rate limit exceeded for type='Pro'"
        );
    }

    #[test]
    fn rate_limit_preserves_retry_matcher_keywords() {
        // `is_rate_limited_error` (agent/mod.rs:3281) scans for
        // "429" / "rate" / "Too Many" / 限流 keywords to decide
        // whether to trigger the 5-retry exponential backoff path.
        // The compact format MUST keep at least one of those so
        // rate-limit auto-retry doesn't silently break.
        let out = format_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            "https://x",
            "codingplan rate limit exceeded for type='Pro'",
        );
        assert!(out.contains("429"), "must contain literal `429`");
        assert!(out.contains("rate"), "must contain `rate` for matcher");
    }

    #[test]
    fn rate_limit_with_chinese_upstream_message_still_matches() {
        // GitCode's litellm proxy on GLM-5.1 emits Chinese rate-limit
        // text — make sure those still match the matcher's CJK
        // keywords even without "429" in the inner msg (we add it).
        let out = format_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            "https://x",
            "请求过于频繁，请稍后再试",
        );
        assert!(out.contains("429"));
        assert!(out.contains("请求过于频繁"));
    }

    #[test]
    fn non_rate_limit_keeps_verbose_form() {
        // 5xx errors need the URL — operators want to know whether
        // the gateway path was right and whether the request even
        // reached the intended host.
        let out = format_http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "https://x/v1/chat/completions",
            "upstream gateway timeout",
        );
        assert!(out.contains("500"));
        assert!(out.contains("https://x/v1/chat/completions"));
        assert!(out.contains("upstream gateway timeout"));
    }

    #[test]
    fn bad_request_keeps_url_for_diagnostics() {
        // 400 errors are usually misconfigured base_url or wrong
        // model name — the URL is part of the answer.
        let out = format_http_error(
            StatusCode::BAD_REQUEST,
            "https://x/v1/chat/completions",
            "Invalid model `xyz`",
        );
        assert!(out.contains("400"));
        assert!(out.contains("https://x"));
        assert!(out.contains("Invalid model"));
    }
}

/// Factory: create the right provider from config.
/// If `api_key` is `None`, automatically loads from `$ATOMCODE_HOME/auth.toml`
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

// Platform OAuth refresh endpoint — uses the same configurable URL
// as auth::oauth (reads ATOMCODE_PLATFORM_SERVER env var).
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

/// Read a valid access token from `$ATOMCODE_HOME/auth.toml`.
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

/// Exchange refresh_token for a new access_token via Platform, save updated auth.toml.
fn refresh_and_save(refresh_token: &str, auth_path: &std::path::Path) -> Result<String> {
    // Same 5s/10s budget as `auth::oauth::blocking_client` — this runs on
    // the TUI thread via `get_valid_token`, so an unreachable OAuth host
    // must never hang the UI.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());
    let builder = client
        .post(crate::auth::oauth::platform_refresh_url())
        .json(&serde_json::json!({ "refresh_token": refresh_token, "provider": "atomgit" }));
    let policy = crate::provider::retry::RetryPolicy::default_policy();
    let resp = crate::provider::retry::send_with_retry_blocking(builder, &policy)
        .map_err(|e| anyhow::anyhow!("Token refresh failed: {} — please /login", e))?;

    if !resp.status().is_success() {
        anyhow::bail!("Token refresh failed ({}) — please /login", resp.status());
    }

    #[derive(serde::Deserialize)]
    struct RefreshedAuth {
        access_token: String,
        #[serde(default)]
        token_type: Option<String>,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<i64>,
        #[serde(default)]
        user: Option<RefreshedUser>,
    }

    #[derive(serde::Deserialize)]
    struct RefreshedUser {
        id: String,
        username: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        avatar_url: Option<String>,
    }

    let token: RefreshedAuth = resp
        .json()
        .map_err(|e| anyhow::anyhow!("Token refresh parse error: {} — please /login", e))?;

    // Preserve original token_type or use default
    let token_type = token.token_type.as_deref().unwrap_or("Bearer");

    // Save updated auth.toml
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let new_rt = token.refresh_token.as_deref().unwrap_or(refresh_token);
    let mut content = format!(
        "access_token = \"{}\"\ncreated_at = {}\nrefresh_token = \"{}\"\n",
        token.access_token, now, new_rt,
    );
    if let Some(e) = token.expires_in {
        content.push_str(&format!("expires_in = {}\n", e));
    }
    content.push_str(&format!("token_type = \"{}\"\n", token_type));
    if let Some(user) = token.user {
        content.push_str(&format!(
            "\n[user]\nid = \"{}\"\nusername = \"{}\"\n",
            user.id, user.username,
        ));
        if let Some(name) = user.name {
            content.push_str(&format!("name = \"{}\"\n", name));
        }
        if let Some(email) = user.email {
            content.push_str(&format!("email = \"{}\"\n", email));
        }
        if let Some(avatar_url) = user.avatar_url {
            content.push_str(&format!("avatar_url = \"{}\"\n", avatar_url));
        }
    }
    let _ = crate::auth::write_auth_file_secure(auth_path, &content);

    Ok(token.access_token)
}

/// Heuristic: does this model name look like a vision-capable model?
///
/// Used by the TUI's Ctrl+V image-paste handler to refuse attaching an
/// image when the active model almost certainly can't accept it (e.g.
/// `glm-5.1`, `deepseek-v4-flash`, `qwen3-coder`). Without this gate
/// the user wastes a turn on a 400 from the upstream — see the
/// `ModelArts.81001` `message[3].content[0] has invalid field(s):
/// text, type` failure pattern that surfaced in production.
///
/// Also used by `vision_preprocessor::maybe_preprocess` to decide
/// whether the active main provider needs preprocessing (vision-capable
/// → skip) and by `coding_plan::setup` to auto-pick a VL preprocessor
/// from the AtomGit model list.
///
/// "OCR" is included because OCR-on-VLM endpoints (PaddleOCR-VL,
/// GOT-OCR, MonkeyOCR, etc.) accept image input via the same
/// OpenAI-compatible `image_url` schema and are first-class candidates
/// for the vision-preprocessor role.
///
/// Conservative — only matches well-known vision/OCR patterns.
/// False-negatives are safe: extend this list when a new vision/OCR model
/// ships rather than threading a per-provider config knob (no
/// user-discoverable opt-in exists). False-positives waste a turn on
/// a 400, so when in doubt this returns false.
pub fn model_name_suggests_vision(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("vision")
        || n.contains("-vl")
        || n.contains("vl-")
        || n.contains("ocr")
        || n.contains("-4v")
        || n.contains("-4.1v")
        || n.starts_with("gpt-4o")
        // Claude 3 onwards is vision-capable. Anthropic uses two naming
        // forms: the legacy `claude-<gen>-<variant>` (claude-3-5-sonnet)
        // and the newer `claude-<variant>-<gen>-<rev>` (claude-sonnet-4-6).
        || n.starts_with("claude-3")
        || n.starts_with("claude-4")
        || n.starts_with("claude-5")
        || n.starts_with("claude-6")
        || n.starts_with("claude-7")
        || n.starts_with("claude-sonnet")
        || n.starts_with("claude-opus")
        || n.starts_with("claude-haiku")
        || n.starts_with("gemini")
        || n.starts_with("pixtral")
        || n.contains("llava")
        || n.contains("qvq")
}

#[cfg(test)]
mod tests {
    use super::{model_name_suggests_vision, unavailable_provider};

    /// Test that auth token is loaded from the correct unified path.
    /// This prevents regressions where OAuth login token persistence breaks
    /// after program restart due to path mismatch.
    #[test]
    fn test_auth_token_path_consistency() {
        // Both paths should resolve to the same location: ~/.atomcode/auth.toml
        let auth_module_path = crate::auth::auth_file_path();
        let expected_path = crate::tool::real_home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".atomcode")
            .join("auth.toml");

        assert_eq!(
            auth_module_path, expected_path,
            "auth_file_path() should always return ~/.atomcode/auth.toml"
        );

        // Verify the path ends with the expected directory structure
        assert!(
            auth_module_path.ends_with(".atomcode/auth.toml")
                || auth_module_path.ends_with(".atomcode\\auth.toml"), // Windows compatibility
            "Path should end with .atomcode/auth.toml, got: {}",
            auth_module_path.display()
        );
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
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
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

    // ── model_name_suggests_vision ────────────────────────────────

    #[test]
    fn vision_heuristic_recognises_known_vision_models() {
        // Anthropic — vision-capable since Claude 3.
        assert!(model_name_suggests_vision("claude-3-5-sonnet"));
        assert!(model_name_suggests_vision("claude-4-opus"));
        assert!(model_name_suggests_vision("claude-sonnet-4-6"));
        // OpenAI — gpt-4o family is multimodal.
        assert!(model_name_suggests_vision("gpt-4o"));
        assert!(model_name_suggests_vision("gpt-4o-mini"));
        assert!(model_name_suggests_vision("gpt-4-vision-preview"));
        // Zhipu GLM vision suffixes — `-4v`, `-4.1v` (NOT `-5.1`).
        assert!(model_name_suggests_vision("GLM-4V"));
        assert!(model_name_suggests_vision("glm-4.1v-thinking"));
        // Qwen / DeepSeek / generic VL family.
        assert!(model_name_suggests_vision("Qwen2-VL-7B"));
        assert!(model_name_suggests_vision("deepseek-vl"));
        // Other major vision lines.
        assert!(model_name_suggests_vision("gemini-2.0-flash"));
        assert!(model_name_suggests_vision("pixtral-12b"));
        assert!(model_name_suggests_vision("llava-1.6"));
        assert!(model_name_suggests_vision("qvq-72b-preview"));
    }

    /// Regression for the user's exact failure: pasting an image while
    /// `GLM-5.1` was the active model produced a `ModelArts.81001 ...
    /// message[3].content[0] has invalid field(s): text, type` 400.
    /// The heuristic must NOT classify GLM-5.1 (or other text-only
    /// models the user is likely to be on) as vision-capable.
    #[test]
    fn vision_heuristic_rejects_text_only_models() {
        assert!(!model_name_suggests_vision("GLM-5.1"));
        assert!(!model_name_suggests_vision("glm-5.1"));
        assert!(!model_name_suggests_vision("deepseek-v4-flash"));
        assert!(!model_name_suggests_vision("Qwen/Qwen3.6-35B-A3B"));
        assert!(!model_name_suggests_vision("gpt-4-turbo")); // text-only base
        assert!(!model_name_suggests_vision("kimi-k2-thinking"));
        assert!(!model_name_suggests_vision("o1-preview")); // not a vision tag
        assert!(!model_name_suggests_vision(""));
    }

    /// OCR family: PaddleOCR-VL is already covered by the `-vl` clause,
    /// but pure-OCR names (no VL/vision substring) need the dedicated
    /// `ocr` clause to be recognized as vision-eligible.
    #[test]
    fn vision_heuristic_recognises_ocr_models() {
        // Names with both ocr + vl/vision (already worked, regression check).
        assert!(model_name_suggests_vision("PaddleOCR-VL-0.9B"));
        assert!(model_name_suggests_vision("Qwen2-VL-OCR-7B"));
        // Pure OCR names — should now match via the dedicated clause.
        assert!(model_name_suggests_vision("GOT-OCR-2.0"));
        assert!(model_name_suggests_vision("PaddleOCR-2.0"));
        assert!(model_name_suggests_vision("MinerU-OCR"));
        assert!(model_name_suggests_vision("MonkeyOCR-1.2B"));
        assert!(model_name_suggests_vision("got-ocr-1.0")); // lowercase
    }

    /// Documented false-positive risk on the new `ocr` clause: any model
    /// name containing the substring `ocr` would match. None of today's
    /// well-known text-only models trigger this. If a future text-only
    /// model name does, this test will fail and a maintainer will know
    /// to tighten the heuristic.
    #[test]
    fn vision_heuristic_documented_false_positives() {
        // Contrived placeholder — `focar` contains `ocar`, not `ocr`,
        // so this is actually safe. Kept here so the comment lives in
        // a real test and a future false-positive case can be added.
        assert!(!model_name_suggests_vision("focar-text-7b"));
    }
}
