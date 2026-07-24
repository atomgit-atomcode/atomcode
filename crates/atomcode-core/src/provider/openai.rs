use std::pin::Pin;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::StreamExt;
use futures::Stream;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::conversation::message::{Message, MessageContent, Role};
use crate::stream::StreamEvent;
use crate::tool::ToolDef;
use atomcode_config::config::provider::ProviderConfig;

use crate::auth::oauth::{get_stored_auth, refresh_access_token};
use crate::i18n::{t, Msg};
use atomcode_auth::gateway_crypto::{self as crypto, SignError, SignInput};

use super::{LlmProvider, ReasoningPolicy};

/// Compute the signing headers (if any) for an outbound request.
///
/// Returns:
/// - `Ok(vec![])` — host doesn't require signing (the common case for
///   user-configured providers); caller proceeds unchanged.
/// - `Ok(non-empty)` — host requires signing; caller merges these
///   headers onto the request before `.send().await`.
/// - `Err(_)` — host requires signing but we cannot produce a valid
///   signature (signer unavailable, no stored auth, etc.). Caller
///   surfaces the error to the user.
///
/// `override_auth` is a test seam: production callers pass `None` and
/// the function reads `get_stored_auth()`.
fn build_codingplan_headers(
    base_url: &str,
    body_bytes: &[u8],
    override_auth: Option<(&str, &str)>,
) -> Result<Vec<(&'static str, String)>> {
    if !crypto::is_atomgit_gateway(base_url) {
        return Ok(Vec::new());
    }

    // Order matters: signer availability is checked FIRST.
    //
    // An open-source build (no `codingplan-crypto` feature) can never
    // produce a valid signature — `/login` will not fix that. If we
    // checked auth first, an open-source user with empty auth.toml
    // would be told "run /codingplan", they'd log in successfully,
    // and STILL hit `Unavailable` on the next chat. The signer-first
    // order gives them the actionable hint (`CpOfficialBuildRequired`)
    // immediately.
    //
    // Official builds (`signer_available() == true`) fall through to
    // the auth check, so an official user with no `~/.atomcode/auth.toml`
    // still sees `CpAuthRequired` (the originally-intended UX).
    if !crypto::signer_available() {
        return Err(anyhow::anyhow!("{}", t(Msg::CpOfficialBuildRequired)));
    }

    let (user_id_string, token_string);
    let (user_id, oauth_token) = match override_auth {
        Some((uid, tok)) => (uid, tok),
        None => {
            let auth =
                get_stored_auth().ok_or_else(|| anyhow::anyhow!("{}", t(Msg::CpAuthRequired)))?;
            user_id_string = auth.user.id.clone();
            token_string = auth.access_token.clone();
            (user_id_string.as_str(), token_string.as_str())
        }
    };

    if user_id.is_empty() || oauth_token.is_empty() {
        return Err(anyhow::anyhow!("{}", t(Msg::CpAuthRequired)));
    }

    let path = url::Url::parse(base_url)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| "/v1/chat/completions".to_string());
    let path = if path.ends_with("/chat/completions") {
        path
    } else {
        format!("{}/chat/completions", path.trim_end_matches('/'))
    };

    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| anyhow::anyhow!("nonce generation failed: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before UNIX epoch: {e}"))?
        .as_secs();

    let input = SignInput {
        method: "POST",
        path: &path,
        body: body_bytes,
        oauth_token,
        user_id,
        timestamp_unix: ts,
        nonce,
    };

    match crypto::signer().sign(input) {
        Ok(out) => Ok(out.headers),
        Err(SignError::Unavailable) => Err(anyhow::anyhow!("{}", t(Msg::CpOfficialBuildRequired))),
        Err(SignError::Derive(detail)) => Err(anyhow::anyhow!(
            "{} (signing-key derivation: {})",
            t(Msg::CpOfficialBuildRequired),
            detail
        )),
    }
}

pub struct OpenAiProvider {
    client: std::sync::Arc<std::sync::RwLock<Client>>,
    /// Rebuild seam used for the endpoint-scoped TLS-1.2 fallback. The bool
    /// forces the cap for the probe; all other client settings stay identical.
    build_client: std::sync::Arc<dyn Fn(bool) -> anyhow::Result<Client> + Send + Sync + 'static>,
    /// Shared so `chat_stream` can refresh the OAuth access_token in-place
    /// when an AtomGit-gateway request comes back 401, without rebuilding
    /// the whole provider. For non-AtomGit providers this stays as the
    /// user-supplied API key for the lifetime of the process — there's
    /// no auth flow for those.
    api_key: std::sync::Arc<tokio::sync::RwLock<String>>,
    model: String,
    base_url: String,
    max_tokens: usize,
    /// Kimi-family thinking knob: `thinking.type` in the request body.
    /// Only emitted when the user configures it — other OpenAI-compatible
    /// gateways may reject unknown top-level fields.
    thinking_type: Option<String>,
    /// Kimi K2.6 Preserved Thinking: `thinking.keep` in the request body.
    thinking_keep: Option<String>,
    /// User-provided override for the reasoning-history echo policy. When
    /// `Some`, bypasses the auto-detect heuristic entirely. Parsed from
    /// `ProviderConfig::reasoning_history` at construction so bad values
    /// fail early at load time with a clear error, not silently mid-turn.
    reasoning_history_override: Option<ReasoningPolicy>,
    /// DeepSeek V4 reasoning effort control (high / max / off).
    reasoning_effort: Option<String>,
    /// Whether the active model accepts image inputs. Drives `MultiPart`
    /// serialisation: vision-capable → OpenAI image_url schema, text-only
    /// → flat string. Computed once from `ProviderConfig::accepts_images()`
    /// at construction; a `/model` switch rebuilds the provider so this
    /// stays in sync with the live config.
    supports_vision: bool,
    /// Stable per-conversation id, set once via `set_session_id` after the
    /// owning agent generates it. Sent as the `x-atomcode-session-id` header
    /// on every request so a forwarding gateway (LiteLLM) can pin the
    /// conversation to one upstream account/replica for prefix-cache
    /// affinity. Empty (the default) → header omitted (e.g. summary / sub-
    /// agent calls that never had an id attached). `std::sync::RwLock`: set
    /// once at startup, read (cheaply cloned, never held across `.await`)
    /// per request.
    session_id: std::sync::Arc<std::sync::RwLock<String>>,
}

impl OpenAiProvider {
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let api_key = config
            .api_key
            .clone()
            .context("OpenAI provider requires an api_key")?;
        let reasoning_history_override = match config.reasoning_history.as_deref() {
            None => None,
            Some(s) => match s.trim().to_ascii_lowercase().as_str() {
                "include" => Some(ReasoningPolicy::Include),
                "exclude" => Some(ReasoningPolicy::Exclude),
                other => anyhow::bail!(
                    "Invalid `reasoning_history` value {:?} for provider type '{}' — \
                     expected \"include\" or \"exclude\" (unset = use auto-detect)",
                    other,
                    config.provider_type,
                ),
            },
        };
        // Validate at load time (like `reasoning_history`) so a typo fails
        // fast with a clear error instead of silently 400-ing mid-turn.
        // Normalised to lowercase so config casing ("High") doesn't matter.
        let reasoning_effort = match config.reasoning_effort.as_deref() {
            None => None,
            Some(s) => match s.trim().to_ascii_lowercase().as_str() {
                "high" => Some("high".to_string()),
                "max" => Some("max".to_string()),
                other => anyhow::bail!(
                    "Invalid `reasoning_effort` value {:?} for provider type '{}' — \
                     expected \"high\" or \"max\" (unset = use API default)",
                    other,
                    config.provider_type,
                ),
            },
        };
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let user_agent = config.user_agent.clone();
        let skip_tls_verify = config.skip_tls_verify;
        let build_client: std::sync::Arc<
            dyn Fn(bool) -> anyhow::Result<Client> + Send + Sync + 'static,
        > = std::sync::Arc::new(move |force_tls12| {
            super::build_http_client(user_agent.as_deref(), skip_tls_verify, force_tls12)
        });
        let initial_tls12 = atomcode_config::tls::should_cap_url(&base_url);
        let client = build_client(initial_tls12)?;
        Ok(Self {
            client: std::sync::Arc::new(std::sync::RwLock::new(client)),
            build_client,
            api_key: std::sync::Arc::new(tokio::sync::RwLock::new(api_key)),
            model: config.model.clone(),
            base_url,
            // Cap at 16K: prevents models from spending 250s on thinking
            // with zero visible output. CC uses fixed 16-32K, not proportional.
            max_tokens: config
                .max_tokens
                .unwrap_or((config.context_window / 4).clamp(8_000, 16_384)),
            thinking_type: config.thinking_type.clone(),
            thinking_keep: config.thinking_keep.clone(),
            reasoning_history_override,
            reasoning_effort,
            supports_vision: config.accepts_images(),
            session_id: std::sync::Arc::new(std::sync::RwLock::new(String::new())),
        })
    }

    /// Derive the reasoning echo policy from model name / base_url.
    /// - `kimi-*` / base_url contains `moonshot` → Include (Moonshot requires
    ///   reasoning_content on every assistant tool_call or returns 400).
    /// - `deepseek-reasoner` / `deepseek-r1` (V3 family) → Exclude (DeepSeek
    ///   V3 rejects the request if reasoning_content is echoed back).
    /// - `deepseek-v4*` (V4 family thinking mode) → Include. DeepSeek flipped
    ///   the contract in V4: thinking-mode requests with tool calls now
    ///   REQUIRE reasoning_content on every historical assistant tool_call
    ///   message, or the API returns 400 "The `reasoning_content` in the
    ///   thinking mode must be passed back to the API". See
    ///   <https://api-docs.deepseek.com/zh-cn/guides/thinking_mode>.
    /// - `mimo-*` / base_url contains `xiaomimimo` → Include. MiMo (小米开源)
    ///   v2.5 / v2.5-pro reuses DeepSeek V4 thinking-mode protocol — gateway
    ///   returns the identical "The reasoning_content in the thinking mode
    ///   must be passed back to the API" 400 when the field is stripped.
    /// - Other OpenAI-compatible endpoints → Exclude (safe default; normal
    ///   OpenAI models don't emit reasoning_content, so there's nothing to
    ///   strip, and non-thinking models typically ignore the field).
    fn derive_reasoning_policy(model: &str, base_url: &str) -> ReasoningPolicy {
        let m = model.to_ascii_lowercase();
        let u = base_url.to_ascii_lowercase();
        if m.contains("deepseek-reasoner") || m.contains("deepseek-r1") {
            return ReasoningPolicy::Exclude;
        }
        if m.contains("deepseek-v4") {
            return ReasoningPolicy::Include;
        }
        if m.starts_with("kimi-")
            || m.starts_with("moonshot")
            || u.contains("moonshot")
            || u.contains("kimi")
            || u.contains("xiaomimimo")
            || u.contains("mimo")
        {
            return ReasoningPolicy::Include;
        }
        if m.starts_with("mimo-") || u.contains("xiaomimimo") {
            return ReasoningPolicy::Include;
        }
        ReasoningPolicy::Exclude
    }

    /// Check whether the model supports DeepSeek's `reasoning_effort` field.
    /// Only DeepSeek V4 (and V4-derived, e.g. `deepseek-v4-pro`) models accept
    /// it. Single source of truth for the gate — the TUI's
    /// `reasoning_effort_applicable_on_provider` delegates here so the
    /// "applicable" hint and the actual send stay in lock-step.
    pub fn reason_effort_applicable(model: &str) -> bool {
        model.to_ascii_lowercase().contains("deepseek-v4")
    }

    /// Build Kimi's `thinking` request-body object from the two flat
    /// config fields. Returns `None` when both are unset so the caller
    /// omits the whole key — safer for non-Kimi gateways that might
    /// error on an unknown top-level `thinking`.
    fn thinking_body_value(
        thinking_type: Option<&str>,
        thinking_keep: Option<&str>,
    ) -> Option<serde_json::Value> {
        if thinking_type.is_none() && thinking_keep.is_none() {
            return None;
        }
        let mut obj = serde_json::Map::new();
        if let Some(t) = thinking_type {
            obj.insert("type".into(), json!(t));
        }
        if let Some(k) = thinking_keep {
            obj.insert("keep".into(), json!(k));
        }
        Some(serde_json::Value::Object(obj))
    }

    /// `supports_vision` toggles how `MessageContent::MultiPart` historical
    /// turns are serialised. When the target model accepts images, the
    /// content is emitted as the OpenAI vision schema (array of
    /// `image_url` + `text` blocks). When it doesn't (text-only proxies
    /// like GLM-5.1 on ModelArts), `MultiPart` is degraded to a flat
    /// string — keeps the conversation replayable across `/model`
    /// switches between vision-capable and text-only providers without
    /// throwing the upstream's `invalid field(s): text, type` 400.
    fn format_messages(
        messages: &[Message],
        reasoning_policy: ReasoningPolicy,
        supports_vision: bool,
    ) -> Vec<serde_json::Value> {
        messages
            .iter()
            .filter_map(|m| {
                match &m.content {
                    MessageContent::Text(s) => {
                        // Tool role with plain Text is invalid for the OpenAI API —
                        // tool results must use MessageContent::ToolResult.
                        let role = match m.role {
                            Role::System => "system",
                            Role::User => "user",
                            Role::Assistant => "assistant",
                            Role::Tool => return None,
                        };
                        // Skip empty messages
                        if s.trim().is_empty() {
                            return None;
                        }
                        let mut obj = json!({"role": role, "content": s});
                        // DeepSeek V4 tool-call round: per official docs, when a
                        // turn had tool_calls ANYWHERE, ALL reasoning_content from
                        // that turn (including the final-answer text's reasoning)
                        // must be echoed in every subsequent request — 400
                        // otherwise. A plain `Text` variant can't persist per-turn
                        // reasoning (c61bfd07 routes final answers that DID capture
                        // reasoning through AssistantWithToolCalls instead), so by
                        // the time we land here there is no real reasoning to echo —
                        // emit the shared non-prose REASONING_PLACEHOLDER under
                        // Include: present + non-empty for the 400 contract, but
                        // nothing the thinking model can mimic back as its output
                        // (the old fluent English sentence got reproduced verbatim
                        // at high context, stalling the turn). Kimi only validates
                        // tool_call messages, so the extra key on Text is fine there.
                        if matches!(m.role, Role::Assistant)
                            && matches!(reasoning_policy, ReasoningPolicy::Include)
                        {
                            obj["reasoning_content"] = json!(super::REASONING_PLACEHOLDER);
                        }
                        Some(obj)
                    }
                    MessageContent::AssistantWithToolCalls {
                        text,
                        tool_calls,
                        reasoning_content,
                        // Anthropic-only field; OpenAI-style endpoints don't
                        // accept `thinking` content blocks. We persist them
                        // for cross-provider switches but don't emit here.
                        thinking_blocks: _,
                    } => {
                        if tool_calls.is_empty() {
                            // No tool calls — send as plain assistant text
                            let t = text.as_deref().unwrap_or("");
                            if t.is_empty() {
                                return None;
                            }
                            let mut obj = json!({"role": "assistant", "content": t});
                            if matches!(reasoning_policy, ReasoningPolicy::Include) {
                                let echo = reasoning_content
                                    .as_deref()
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(super::REASONING_PLACEHOLDER);
                                obj["reasoning_content"] = json!(echo);
                            }
                            return Some(obj);
                        }
                        let mut msg = json!({"role": "assistant"});
                        // Always include content field — some APIs (DeepSeek/SiliconFlow)
                        // reject messages without it even when tool_calls is present.
                        msg["content"] = json!(text.as_deref().unwrap_or(""));
                        // Thinking-model providers require reasoning_content to
                        // appear on every assistant tool_call message in history.
                        // Kimi only checks the key is present (empty ok). DeepSeek
                        // V4 additionally rejects an empty string ("must be passed
                        // back to the API"), so when we have no captured reasoning
                        // — cross-provider handoff (glm→deepseek), pre-fix session,
                        // or a non-thinking model that still tool-called — we emit
                        // a short non-empty placeholder. Both APIs accept any
                        // non-empty string, DeepSeek does the opposite of Kimi for
                        // Exclude so this block is gated on policy.
                        if matches!(reasoning_policy, ReasoningPolicy::Include) {
                            let echo = reasoning_content
                                .as_deref()
                                .filter(|s| !s.is_empty())
                                .unwrap_or(super::REASONING_PLACEHOLDER);
                            msg["reasoning_content"] = json!(echo);
                        }
                        msg["tool_calls"] = json!(tool_calls
                            .iter()
                            .map(|tc| {
                                // Ensure arguments is valid JSON — some APIs reject invalid JSON strings.
                                let args =
                                    if serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                        .is_ok()
                                    {
                                        tc.arguments.clone()
                                    } else {
                                        // Try repair; if still invalid, wrap as a simple object
                                        let repaired = repair_tool_args(&tc.arguments);
                                        if serde_json::from_str::<serde_json::Value>(&repaired)
                                            .is_ok()
                                        {
                                            repaired
                                        } else {
                                            json!({"input": tc.arguments}).to_string()
                                        }
                                    };
                                json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": args,
                                    }
                                })
                            })
                            .collect::<Vec<_>>());
                        Some(msg)
                    }
                    MessageContent::MultiPart { text, images } => {
                        if supports_vision {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
                            for img in images {
                                parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!(
                                            "data:{};base64,{}",
                                            img.media_type, img.data
                                        ),
                                    }
                                }));
                            }
                            if let Some(t) = text {
                                parts.push(json!({"type": "text", "text": t}));
                            }
                            Some(json!({"role": "user", "content": parts}))
                        } else {
                            // Degrade to text-only — the model's wire schema
                            // doesn't support image blocks. The user's
                            // caption survives (and already has `[Image #N]`
                            // markers from the input buffer); the image bytes
                            // simply aren't representable here.
                            let content = match text {
                                Some(t) if !t.is_empty() => t.clone(),
                                _ => "[image attached]".to_string(),
                            };
                            Some(json!({"role": "user", "content": content}))
                        }
                    }
                    MessageContent::ToolResult(r) => {
                        if r.call_id.is_empty() {
                            return None;
                        }
                        Some(json!({
                            "role": "tool",
                            "tool_call_id": r.call_id,
                            "content": r.output,
                        }))
                    }
                    MessageContent::ToolResultRef(r) => {
                        if r.call_id.is_empty() {
                            return None;
                        }
                        Some(json!({
                            "role": "tool",
                            "tool_call_id": r.call_id,
                            "content": r.summary,
                        }))
                    }
                }
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    usage: Option<ChunkUsage>,
}

#[derive(Deserialize)]
struct ChunkUsage {
    prompt_tokens: Option<usize>,
    completion_tokens: Option<usize>,
    // Provider-specific cache fields (different providers use different names):
    // OpenAI: prompt_tokens_details.cached_tokens
    // DeepSeek/SiliconFlow: prompt_cache_hit_tokens
    // Zhipu: cached_tokens
    prompt_cache_hit_tokens: Option<usize>,
    cached_tokens: Option<usize>,
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChunkDelta {
    content: Option<String>,
    /// MiniMax M2.7 / DeepSeek R1 send thinking via this field. We forward
    /// it as `StreamEvent::Reasoning` so the kernel agent can promote it to
    /// the final text if `content` ends up empty — some gateways route
    /// *entire* responses to `reasoning_content` for these models, which
    /// previously showed up as a silent 0-token "Nailed it" turn.
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    index: Option<usize>,
    id: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<ResponseChoice>,
    usage: Option<ChunkUsage>,
}

#[derive(Deserialize)]
struct ResponseChoice {
    message: Option<ResponseMessage>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let url = normalize_base_url(&self.base_url);
        let mut body = json!({
            "model": self.model,
            "messages": Self::format_messages(messages, self.reasoning_history_policy(), self.supports_vision),
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": self.max_tokens,
        });

        if let Some(tool_defs) = tools {
            if !tool_defs.is_empty() {
                body["tools"] = json!(tool_defs
                    .iter()
                    .map(|td| json!({
                        "type": "function",
                        "function": {
                            "name": td.name,
                            "description": td.description,
                            "parameters": td.parameters,
                        }
                    }))
                    .collect::<Vec<_>>());
                // Allow the model to decide whether to call multiple tools in parallel
            }
        }

        // Kimi K2.5 / K2.6 top-level `thinking` object. Only sent when the
        // user configured it — other OpenAI-compatible gateways may reject
        // unknown fields, and omitting lets Kimi's default behavior apply.
        if let Some(th) =
            Self::thinking_body_value(self.thinking_type.as_deref(), self.thinking_keep.as_deref())
        {
            body["thinking"] = th;
        }

        // DeepSeek V4 reasoning_effort: only emitted when applicable
        if let Some(ref effort) = self.reasoning_effort {
            if Self::reason_effort_applicable(&self.model) {
                body["reasoning_effort"] = json!(effort);
            }
        }

        let policy = crate::provider::retry::RetryPolicy::default_policy();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // ── TEMP WIRE-DUMP (debug only) ────────────────────────────────
        // Set ATOMCODE_WIRE_DUMP=1 to dump every outbound LLM request body
        // to ~/.atomcode/wire-dump/<timestamp>.json so we can verify what
        // litellm / the proxy actually receives. Used to diagnose
        // "tool_call results appear empty to the model" — comparing the
        // wire body's `messages[N].content` against the conversation
        // snapshot proves whether atomcode or the proxy is the source of
        // truncation. Remove once root-caused.
        if std::env::var("ATOMCODE_WIRE_DUMP").ok().as_deref() == Some("1") {
            if let Ok(home) = std::env::var("HOME") {
                let dir = std::path::PathBuf::from(home).join(".atomcode/wire-dump");
                let _ = std::fs::create_dir_all(&dir);
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
                    .unwrap_or_else(|_| "0".to_string());
                let path = dir.join(format!("{}.json", ts));
                if let Ok(serialized) = serde_json::to_string_pretty(&body) {
                    let _ = std::fs::write(&path, serialized);
                }
                // Sidecar: the `x-atomcode-session-id` header value sent with
                // THIS request (header isn't part of the body dump). Lets us
                // correlate a wire-dump request to the session id the gateway
                // sees: `grep -rl <uuid> ~/.atomcode/wire-dump/*.session`.
                let sid = self
                    .session_id
                    .read()
                    .ok()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let _ = std::fs::write(dir.join(format!("{}.session", ts)), &sid);
            }
        }
        // ────────────────────────────────────────────────────────────────

        // Provider truncation detector input: char count of message contents
        // and tool_call arguments. We compare this to provider-reported
        // prompt_tokens; if the ratio is way above any tokenizer can
        // explain, the proxy is silently dropping content (e.g. GitCode
        // litellm's hidden ~6.2K cap on glm-5 — 5/8 atomgr session).
        let body_content_chars = sum_message_content_chars(&body);

        // Move the pieces needed to rebuild the request into the task — the
        // outer mid-stream retry loop reconstructs the builder on each
        // attempt because `RequestBuilder` is single-use.
        let client_slot = self.client.clone();
        let build_client = self.build_client.clone();
        let api_key = self.api_key.clone();
        let provider_label = self.model.clone();
        let base_url_for_signing = self.base_url.clone();
        // Stable per-conversation routing key for the forwarding gateway.
        // Read once here (never held across `.await`); empty → header omitted.
        let session_id_hdr = self
            .session_id
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();

        tokio::spawn(async move {
            // Upfront signing precheck. Without this, the retry factory
            // below silently `.unwrap_or_default()`s any signing error
            // (open-source build → empty headers → 403 from gateway with
            // an unhelpful raw body), so users on an open-source build
            // pointed at the AtomGit gateway would never see the
            // actionable `CpOfficialBuildRequired` hint. Run it here
            // (signer availability + auth state don't depend on body
            // content, so a zero-byte probe is sufficient) so the
            // friendly error surfaces BEFORE the first network call.
            if let Err(e) = build_codingplan_headers(&base_url_for_signing, &[], None) {
                let _ = tx.send(Ok(StreamEvent::Error(e.to_string())));
                return;
            }

            // Mid-stream retry: when the provider opens the stream but the
            // chunked body errors out BEFORE any SSE `data:` line is parsed,
            // it's safe to redo the whole request — no text/tool-call has
            // been committed to the conversation, no UI delta has been
            // emitted. Common cause: self-hosted endpoints that reset the
            // connection at request open under load (the failure mode
            // `error decoding response body` surfaces as). Once `data:` has
            // been seen, retry would produce duplicated output, so the
            // error is surfaced verbatim with a humanised explanation.
            const MAX_STREAM_ATTEMPTS: u32 = 2;
            let mut attempt: u32 = 0;
            // AtomGit-gateway 401 reactive refresh: one shot per
            // chat_stream call. The proactive `load_auth_token` path
            // (provider/mod.rs) only refreshes when the local clock
            // says the token is expired, which misses server-side
            // revocation/rotation/clock-skew failures — those surface
            // as a hard 401 mid-conversation. Without this flag a
            // dead-loop is theoretically possible if the broker's
            // refresh response still leaves us 401-able.
            let mut auth_retry_used = false;
            let mut tls12_probe = false;
            'retry: loop {
                attempt += 1;
                let client = client_slot
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                // Sample before sending: another managed client may latch while
                // this request is in flight, but this client would still need
                // rebuilding because it began with the old TLS policy.
                let was_capped =
                    tls12_probe || atomcode_config::tls::should_cap_url(&base_url_for_signing);
                // Serialize once per outer-loop iteration. The body content
                // does NOT change between inner retries — only the signing
                // headers do (fresh nonce/ts/sig per attempt to avoid
                // SIG_REPLAY / SIG_STALE from the atomgit-gateway).
                let body_bytes = match serde_json::to_vec(&body) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Ok(StreamEvent::Error(format!(
                            "Failed to serialize chat request body: {e}"
                        ))));
                        return;
                    }
                };
                // Snapshot the current token. After a successful auth
                // refresh below, the shared `api_key` will hold the new
                // value and the next iteration picks it up here.
                let current_token = api_key.read().await.clone();

                // Build a factory closure that re-signs on every inner
                // retry attempt. Each call to the factory generates a
                // fresh nonce + timestamp + HMAC signature via
                // `build_codingplan_headers`, preventing SIG_REPLAY
                // (server's nonce cache holds the first attempt's nonce)
                // and SIG_STALE (cumulative backoff pushes ts past the
                // 300 s freshness window).
                //
                // On signing failure during a retry we fall back to
                // empty extra headers (no panic, worst case we get a
                // fresh 403 which the caller already handles). The
                // initial request has already been signed successfully
                // at this point, so retries are the only code path that
                // can reach the fallback.
                let url_for_factory = url.clone();
                let body_for_factory = body_bytes.clone();
                let base_url_clone = base_url_for_signing.clone();
                let client_for_factory = client.clone();
                let token_for_factory = current_token.clone();
                let session_id_for_factory = session_id_hdr.clone();
                let resign = move || -> reqwest::RequestBuilder {
                    let extra_headers =
                        build_codingplan_headers(&base_url_clone, &body_for_factory, None)
                            .unwrap_or_default();
                    let mut req = client_for_factory
                        .post(&url_for_factory)
                        .header("Authorization", format!("Bearer {}", token_for_factory))
                        .header("Content-Type", "application/json")
                        .body(body_for_factory.clone());
                    for (name, value) in extra_headers {
                        req = req.header(name, value);
                    }
                    // Stable session id → lets the forwarding gateway pin this
                    // conversation to one upstream for prefix-cache affinity.
                    if !session_id_for_factory.is_empty() {
                        req = req.header("x-atomcode-session-id", session_id_for_factory.clone());
                    }
                    req
                };

                let response = match crate::provider::retry::send_with_retry_resign(resign, &policy)
                    .await
                {
                    Ok(resp) => {
                        if tls12_probe {
                            atomcode_config::tls::latch_managed_tls12();
                            tls12_probe = false;
                        }
                        resp
                    }
                    Err(e) => {
                        if atomcode_config::tls::should_try_fallback(
                            &base_url_for_signing,
                            was_capped,
                            e.is_connect(),
                        ) {
                            match build_client(true) {
                                Ok(fresh) => {
                                    *client_slot
                                        .write()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
                                    tls12_probe = true;
                                    attempt -= 1;
                                    continue 'retry;
                                }
                                Err(build_error) => {
                                    let _ = tx.send(Ok(StreamEvent::Error(format!(
                                        "TLS 1.2 fallback client build failed: {build_error:#}"
                                    ))));
                                    return;
                                }
                            }
                        }
                        let _ =
                            tx.send(Ok(StreamEvent::Error(format!("Connection failed: {}", e))));
                        return;
                    }
                };

                if !response.status().is_success() {
                    let status = response.status();

                    // AtomGit-gateway 401: try `refresh_access_token`
                    // once. On success, write the new token back to the
                    // shared slot and retry the SAME request (doesn't
                    // count against MAX_STREAM_ATTEMPTS — that budget is
                    // for transient body-decode failures, not auth).
                    if status == reqwest::StatusCode::UNAUTHORIZED
                        && !auth_retry_used
                        && crypto::is_atomgit_gateway(&base_url_for_signing)
                    {
                        auth_retry_used = true;
                        // Drain the response body so the connection can
                        // be returned to the pool cleanly; otherwise
                        // hyper logs a "connection reset" on drop.
                        let _ = response.text().await;
                        let api_key_handle = api_key.clone();
                        let refresh_outcome =
                            tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                                let auth = get_stored_auth().ok_or_else(|| {
                                    anyhow::anyhow!("no stored auth — cannot refresh")
                                })?;
                                let new_auth = refresh_access_token(&auth)?;
                                Ok(new_auth.access_token)
                            })
                            .await;
                        if let Ok(Ok(new_token)) = refresh_outcome {
                            *api_key_handle.write().await = new_token;
                            // Auth retry doesn't burn a stream-attempt
                            // slot — mid-stream retries are for
                            // body-decode failures, which is orthogonal.
                            attempt -= 1;
                            continue 'retry;
                        }
                        // Fall through to the friendly-error branch
                        // below; response is already consumed so build
                        // the error from `status` alone.
                        let _ =
                            tx.send(Ok(StreamEvent::Error(t(Msg::ChatAuthExpired).to_string())));
                        return;
                    }

                    let resp_url = response.url().to_string();
                    let body = response.text().await.unwrap_or_default();
                    // 401 from an AtomGit gateway after the auth-retry
                    // shot was already used (or refresh-then-still-401):
                    // the raw server message ("Gitcode auth: token
                    // rejected (status=401)") is not actionable. Swap
                    // for an i18n hint pointing at /login. Non-atomgit
                    // gateways (user-supplied sk-... keys) keep the
                    // verbatim message so developers see the real
                    // diagnostic.
                    let formatted = if status == reqwest::StatusCode::UNAUTHORIZED
                        && crypto::is_atomgit_gateway(&base_url_for_signing)
                    {
                        t(Msg::ChatAuthExpired).to_string()
                    } else {
                        let msg = super::extract_error_message(&body);
                        super::format_http_error(status, &resp_url, &msg)
                    };
                    let _ = tx.send(Ok(StreamEvent::Error(formatted)));
                    return;
                }

                // Per-attempt local state. Reset on each retry so a partial
                // first attempt's accumulated bytes don't leak into the
                // second attempt's parser.
                let mut byte_buffer: Vec<u8> = Vec::with_capacity(4096);
                let mut buffer = String::new();
                let mut byte_stream = response.bytes_stream();

                // ── TEMP RESPONSE WIRE-DUMP (debug only) ──────────────
                // Pairs with the request dump — captures the raw SSE
                // bytes coming back so we can verify whether litellm /
                // proxy returns a standard OpenAI stream format. Files
                // are named with `_resp` suffix to pair with the
                // request dump preceding them. Bytes are appended so
                // multi-chunk streams accumulate into one file.
                let resp_dump_path: Option<std::path::PathBuf> =
                    if std::env::var("ATOMCODE_WIRE_DUMP").ok().as_deref() == Some("1") {
                        std::env::var("HOME").ok().map(|home| {
                            let dir = std::path::PathBuf::from(home).join(".atomcode/wire-dump");
                            let _ = std::fs::create_dir_all(&dir);
                            let ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
                                .unwrap_or_else(|_| "0".to_string());
                            dir.join(format!("{}_resp.sse", ts))
                        })
                    } else {
                        None
                    };
                // ────────────────────────────────────────────────────────
                // (id, name, accumulated_args) per tool-call index.
                let mut tool_calls: Vec<(String, String, String)> = Vec::new();
                // Tracks whether StreamEvent::ToolCallStart has already
                // been emitted for each index. Some providers (GPT-5
                // family — confirmed via wire-dump 2026-05-27) repeat
                // the SAME non-empty id on EVERY chunk but only send
                // function.name on the first chunk. Without an "emitted
                // once" guard we'd fire ToolCallStart on every chunk;
                // without a "don't clobber name with empty" guard we'd
                // overwrite the captured name with `""` from subsequent
                // chunks where func.name is None.
                let mut tool_start_emitted: Vec<bool> = Vec::new();
                let mut last_usage: Option<crate::stream::TokenUsage> = None;
                let mut saw_data_line = false;
                let mut saw_valid_chunk = false;
                let mut invalid_chunk_samples: Vec<String> = Vec::new();
                // Track how much real content the stream actually
                // produced. Used by the abrupt-close branch below to
                // distinguish:
                //   * many chunks + much content → real mid-output
                //     truncation (table cut, list mid-row, …) → keep
                //     emitting Done(truncated=true) so the agent's
                //     "resume where you left off" retry can fire.
                //   * 0-2 chunks, short text, no tool calls → gateway
                //     streamed a single error blob like 「请求负载
                //     过高，请稍后再试」 and hung up. NOT a real
                //     truncation; emit StreamEvent::Error so the
                //     agent's rate-limit / failure path takes over
                //     instead of looping the resume retry.
                let mut content_chunks: usize = 0;
                let mut accumulated_content = String::new();
                // One-shot guard: if the provider's prompt_tokens looks
                // implausibly low for our content size, log a warning once
                // per request stream so we don't spam.
                let mut truncation_warned = false;
                // Held-back Done event: GitCode-style gateways emit usage
                // in a chunk AFTER finish_reason. We capture finish_reason
                // here, keep parsing for the trailing usage chunk, and
                // emit this on `[DONE]` (or stream end) so token counters
                // and the truncation detector see real numbers.
                let mut pending_finish: Option<crate::stream::StreamEvent> = None;

                // Idle timeout: abort only if NO bytes arrive for this long.
                // Defaults to 300s (override via ATOMCODE_IDLE_TIMEOUT_SECS) to
                // match the runner's first-token / stream timeout. Thinking models
                // (DeepSeek V4 Flash etc.) can take >3min to emit the FIRST byte
                // after a large (~200K) prompt; the old hard-coded 120s guard
                // preempted that window and killed legitimately-slow requests
                // mid-prefill, surfacing as "模型响应超时" + a full-context resend
                // on retry. 300s here no longer undercuts the runner's 300s
                // first-token allowance.
                let idle_timeout_secs = std::env::var("ATOMCODE_IDLE_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(300);
                let idle_timeout = std::time::Duration::from_secs(idle_timeout_secs);
                loop {
                    let chunk = match tokio::time::timeout(idle_timeout, byte_stream.next()).await {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break, // stream ended
                        Err(_) => {
                            let _ = tx.send(Ok(StreamEvent::Error(format!(
                                "Stream timeout: no data received for {idle_timeout_secs}s"
                            ))));
                            return;
                        }
                    };

                    match chunk {
                        Ok(bytes) => {
                            // TEMP wire-dump (response side): append raw
                            // bytes as they arrive so we can inspect the
                            // exact SSE stream litellm sent back.
                            if let Some(ref p) = resp_dump_path {
                                use std::io::Write;
                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(p)
                                {
                                    let _ = f.write_all(&bytes);
                                }
                            }
                            byte_buffer.extend_from_slice(&bytes);
                        }
                        Err(e) => {
                            // Safe-to-retry condition: stream opened but no SSE
                            // `data:` line was parsed yet. Common with
                            // self-hosted endpoints that open the response,
                            // immediately fail to start streaming, and reset
                            // the chunked body — at this point nothing has
                            // been committed downstream, so a fresh request
                            // is equivalent to a first attempt.
                            if !saw_data_line && attempt < MAX_STREAM_ATTEMPTS {
                                continue 'retry;
                            }
                            let _ = tx.send(Ok(StreamEvent::Error(humanise_stream_error(&e))));
                            return;
                        }
                    }

                    // Convert bytes to string, keeping incomplete UTF-8 sequences for next chunk
                    let text = match String::from_utf8(byte_buffer.clone()) {
                        Ok(s) => {
                            byte_buffer.clear();
                            s
                        }
                        Err(e) => {
                            let valid_len = e.utf8_error().valid_up_to();
                            if valid_len == 0 {
                                // No valid UTF-8 yet, wait for more bytes
                                continue;
                            }
                            let valid =
                                String::from_utf8_lossy(&byte_buffer[..valid_len]).to_string();
                            byte_buffer = byte_buffer[valid_len..].to_vec();
                            valid
                        }
                    };

                    buffer.push_str(&text);

                    while let Some(pos) = buffer.find('\n') {
                        let line = buffer[..pos].trim().to_string();
                        buffer = buffer[pos + 1..].to_string();

                        if line.starts_with("data:") {
                            saw_data_line = true;
                            let data = line.strip_prefix("data:").unwrap().trim();
                            if data == "[DONE]" {
                                if let Some(usage) = last_usage.take() {
                                    let _ = tx.send(Ok(StreamEvent::Usage(usage)));
                                }
                                // Emit the held-back Done from finish_reason if present;
                                // otherwise default to a non-truncated Done (e.g. providers
                                // that close the stream with [DONE] but never emit a
                                // finish_reason field).
                                let done = pending_finish
                                    .take()
                                    .unwrap_or(StreamEvent::Done { truncated: false });
                                let _ = tx.send(Ok(done));
                                return;
                            }
                            if let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) {
                                saw_valid_chunk = true;
                                // Store usage — don't emit yet. Some providers send cumulative
                                // usage in multiple chunks; we only want the final value.
                                if let Some(usage) = &chunk.usage {
                                    // Extract cached tokens from whichever field the provider uses
                                    let cached = usage
                                        .prompt_cache_hit_tokens
                                        .or(usage.cached_tokens)
                                        .or_else(|| {
                                            usage
                                                .prompt_tokens_details
                                                .as_ref()
                                                .and_then(|d| d.cached_tokens)
                                        })
                                        .unwrap_or(0);
                                    let pt = usage.prompt_tokens.unwrap_or(0);
                                    if !truncation_warned {
                                        if let Some(ratio) =
                                            check_truncation(body_content_chars, pt)
                                        {
                                            truncation_warned = true;
                                            let msg = format!(
                                                "Provider may be truncating input on \
                                             model={}: {} content chars vs {} reported \
                                             prompt_tokens (ratio {:.1} chars/token; \
                                             normal mixed-content runs 2-4). If turns \
                                             spiral, the proxy may be capping context.",
                                                provider_label, body_content_chars, pt, ratio,
                                            );
                                            let _ = tx.send(Ok(StreamEvent::Warning(msg)));
                                        }
                                    }
                                    last_usage = Some(crate::stream::TokenUsage {
                                        prompt_tokens: pt,
                                        completion_tokens: usage.completion_tokens.unwrap_or(0),
                                        cached_tokens: cached,
                                    });
                                }
                                for choice in chunk.choices {
                                    if let Some(content) = choice.delta.content {
                                        if !content.is_empty() {
                                            content_chunks += 1;
                                            accumulated_content.push_str(&content);
                                            let _ = tx.send(Ok(StreamEvent::Delta(content)));
                                        }
                                    }
                                    if let Some(reasoning) = choice.delta.reasoning_content {
                                        if !reasoning.is_empty() {
                                            let _ = tx.send(Ok(StreamEvent::Reasoning(reasoning)));
                                        }
                                    }
                                    if let Some(delta_tcs) = &choice.delta.tool_calls {
                                        for tc in delta_tcs {
                                            let idx = tc.index.unwrap_or(0);
                                            // Grow the vec if this is a new tool call index
                                            while tool_calls.len() <= idx {
                                                tool_calls.push((
                                                    String::new(),
                                                    String::new(),
                                                    String::new(),
                                                ));
                                                tool_start_emitted.push(false);
                                            }
                                            let entry = &mut tool_calls[idx];
                                            // 1) Capture id only when present + non-empty.
                                            //    ModelScope sends empty-string id on
                                            //    increment chunks — skipping keeps the
                                            //    earlier non-empty value.
                                            if let Some(id) = &tc.id {
                                                if !id.is_empty() {
                                                    entry.0 = id.clone();
                                                }
                                            }
                                            // 2) Accumulate name + args INDEPENDENTLY of
                                            //    id presence.
                                            //    * GPT-5 family (confirmed via wire-dump):
                                            //      repeats same non-empty id on EVERY chunk
                                            //      but only sends function.name on chunk 1.
                                            //      Old code did `entry.1 = func.name.clone()
                                            //      .unwrap_or_default()` which clobbered the
                                            //      captured "use_skill" with "" on every
                                            //      subsequent chunk — final ToolCallDone had
                                            //      empty name → dropped as malformed.
                                            //    * Guard `!name.is_empty()` also protects
                                            //      against any provider that ever sends
                                            //      `name: ""` placeholder.
                                            if let Some(func) = &tc.function {
                                                if let Some(name) = &func.name {
                                                    if !name.is_empty() {
                                                        entry.1 = name.clone();
                                                    }
                                                }
                                                if let Some(args) = &func.arguments {
                                                    entry.2.push_str(args);
                                                    let _ = tx.send(Ok(
                                                        StreamEvent::ToolCallDelta(args.clone()),
                                                    ));
                                                }
                                            }
                                            // 3) Emit ToolCallStart EXACTLY ONCE per index
                                            //    — as soon as both id AND name are known.
                                            //    Old code emitted on every chunk where id
                                            //    was non-empty (N events for an N-chunk
                                            //    stream); downstream UI handles the
                                            //    duplicates but it's wasted work + log
                                            //    noise.
                                            if !tool_start_emitted[idx]
                                                && !entry.0.is_empty()
                                                && !entry.1.is_empty()
                                            {
                                                tool_start_emitted[idx] = true;
                                                let _ = tx.send(Ok(StreamEvent::ToolCallStart {
                                                    id: entry.0.clone(),
                                                    name: entry.1.clone(),
                                                }));
                                            }
                                        }
                                    }
                                    if let Some(ref reason) = choice.finish_reason {
                                        // Don't return here — flush tool_calls + remember the
                                        // finish_reason, then keep parsing until [DONE]. Some
                                        // gateways (GitCode litellm proxy on glm-5 confirmed
                                        // 5/8) send `usage` in a chunk AFTER `finish_reason`,
                                        // and a previous version of this code returned on
                                        // finish_reason → usage chunk silently dropped → both
                                        // the token counters and the truncation detector saw
                                        // 0 prompt_tokens for entire sessions.
                                        match reason.as_str() {
                                            "tool_calls" => {
                                                for (id, name, args) in &tool_calls {
                                                    let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                                                        crate::tool::ToolCall {
                                                            id: id.clone(),
                                                            name: name.clone(),
                                                            arguments: args.clone(),
                                                        },
                                                    )));
                                                }
                                                tool_calls.clear();
                                                tool_start_emitted.clear();
                                                pending_finish =
                                                    Some(StreamEvent::Done { truncated: false });
                                            }
                                            "length" | "max_tokens" => {
                                                // Model hit token limit — flush partial tool
                                                // calls so downstream sees what the model was
                                                // attempting. (Args may be malformed;
                                                // `repair_tool_args` + write.rs friendly errors
                                                // handle that.)
                                                for (id, name, args) in &tool_calls {
                                                    let _ = tx.send(Ok(StreamEvent::ToolCallDone(
                                                        crate::tool::ToolCall {
                                                            id: id.clone(),
                                                            name: name.clone(),
                                                            arguments: args.clone(),
                                                        },
                                                    )));
                                                }
                                                tool_calls.clear();
                                                tool_start_emitted.clear();
                                                pending_finish =
                                                    Some(StreamEvent::Done { truncated: true });
                                            }
                                            "stop" | _ => {
                                                pending_finish =
                                                    Some(StreamEvent::Done { truncated: false });
                                            }
                                        }
                                    }
                                }
                            } else if invalid_chunk_samples.len() < 3 && !data.is_empty() {
                                invalid_chunk_samples.push(sample_for_error(data));
                            }
                        }
                    }
                }

                let tail = buffer.trim();
                if !tail.is_empty() {
                    if let Some(events) = parse_nonstream_response(tail) {
                        for event in events {
                            let _ = tx.send(Ok(event));
                        }
                        return;
                    }
                }

                if saw_data_line && !saw_valid_chunk {
                    let detail = if invalid_chunk_samples.is_empty() {
                        "no chunk could be parsed".to_string()
                    } else {
                        format!("samples: {}", invalid_chunk_samples.join(" | "))
                    };
                    let _ = tx.send(Ok(StreamEvent::Error(format!(
                        "Provider returned an unparseable OpenAI-compatible stream ({})",
                        detail
                    ))));
                    return;
                }

                if !tail.is_empty() {
                    let _ = tx.send(Ok(StreamEvent::Error(format!(
                        "Provider returned a non-SSE response AtomCode could not parse: {}",
                        sample_for_error(tail)
                    ))));
                    return;
                }

                // ── Stream ended without close marker ──
                // Reaching here means we parsed valid SSE chunks but the
                // stream's `bytes_stream.next()` returned `Ok(None)` (clean
                // close at TCP/HTTP level) WITHOUT either:
                //   a. a `data: [DONE]` line (handled at line ~519, returns
                //      with truncated=false), or
                //   b. a `finish_reason` of `stop` / `length` / `tool_calls`
                //      (handled inline in the chunk parser around line ~610,
                //      returns with the appropriate truncated flag).
                //
                // Observed three times across May 2026 atomgr/atomcode
                // sessions on the self-hosted glm-5.1 endpoint:
                //   - 5/4 21:21 Turn 23 — `error decoding response body` (Err
                //     path, separately fixed by mid-stream retry).
                //   - 5/5 10:06 Turn 10 — text response stopped at "1.\n"
                //     mid-list, no close marker (this path).
                //   - 5/5 19:37 Turn 72-73 — markdown table truncated
                //     mid-row, no close marker (this path).
                //
                // Pre-fix this branch emitted `Done { truncated: false }`,
                // making the agent loop treat the partial output as a
                // complete response and `finish_turn(Natural)` immediately.
                // The user saw a cut-off table / list with no error, no
                // retry, and no indication that anything went wrong.
                //
                // Post-fix (this commit):
                //   1. Flush any in-flight tool calls so partial-args don't
                //      silently disappear (mirrors the `length` branch's
                //      handling at line ~622).
                //   2. Emit a TextDelta marker so the user (and datalog) can
                //      see why the response was cut. Goes through the
                //      normal stream_filter path; doesn't pollute model
                //      context with control sequences.
                //   3. Emit `Done { truncated: true }` so the agent loop's
                //      existing retry-with-resume path (`agent/mod.rs:1854`,
                //      `if truncated && retry_count < 1`) injects the
                //      "Output limit hit. … resume where you left off"
                //      hint and triggers a continuation turn.
                // If finish_reason had already arrived (we held the Done
                // back waiting for trailing usage), don't downgrade it to
                // a truncated=true close — the model finished cleanly and
                // the stream just lacked a [DONE] marker. Flush any
                // buffered usage first so token counters are honest.
                if let Some(usage) = last_usage.take() {
                    let _ = tx.send(Ok(StreamEvent::Usage(usage)));
                }
                if let Some(done) = pending_finish.take() {
                    let _ = tx.send(Ok(done));
                    return;
                }

                // Abrupt close discriminator: if the model never made
                // tool-call progress AND the body arrived as a single
                // burst (≤ 2 content chunks), this wasn't a real
                // truncation — gateways like GitCode's litellm proxy
                // stream a single error blob (「请求负载过高，请稍后
                // 再试」 / a verbose `litellm.InternalServerError` JSON
                // envelope) and slam the connection closed without a
                // [DONE] marker. Promoting that to `truncated=true`
                // makes the agent inject "resume where you left off"
                // and retry, which renders the SAME error a second
                // time (see issue: GLM-5.1 网关限流双重渲染 /
                // LiteLLM 429 cooldown_list 双重渲染).
                // Diverting to `StreamEvent::Error` instead lets the
                // agent's `is_rate_limited` retry path (with 3-30s
                // backoff) handle it correctly — or, if it's an
                // unfamiliar error string, surface it once and stop.
                //
                // Discriminator is `content_chunks <= 2` alone: real
                // streamed completions emit many small deltas (tens
                // to hundreds of chunks), while gateway errors arrive
                // as 1-2 large chunks regardless of payload size
                // (Chinese 10-80-char banners or 700+-char LiteLLM
                // JSON envelopes both qualify). The earlier ≤ 200
                // char cap let the LiteLLM JSON shape slip through
                // to the truncated-retry path and caused the double
                // render. The real risk — misclassifying a 1-chunk
                // legit reply — is mitigated by the fact that
                // successful completions virtually always emit
                // `[DONE]`; reaching this branch already means the
                // stream ended anomalously.
                let trimmed = accumulated_content.trim();
                let looks_like_gateway_error =
                    tool_calls.is_empty() && content_chunks <= 2 && !trimmed.is_empty();
                if looks_like_gateway_error {
                    let _ = tx.send(Ok(StreamEvent::Error(trimmed.to_string())));
                    return;
                }

                for (id, name, args) in &tool_calls {
                    let _ = tx.send(Ok(StreamEvent::ToolCallDone(crate::tool::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: args.clone(),
                    })));
                }
                tool_calls.clear();
                tool_start_emitted.clear();
                let _ = tx.send(Ok(StreamEvent::Delta(
                    "\n[stream ended without close marker — response above may be incomplete]\n"
                        .to_string(),
                )));
                let _ = tx.send(Ok(StreamEvent::Done { truncated: true }));
                return;
            }
        });

        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        ))
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn set_session_id(&self, session_id: &str) {
        if let Ok(mut g) = self.session_id.write() {
            *g = session_id.to_string();
        }
    }

    fn session_id(&self) -> String {
        self.session_id
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    fn reasoning_history_policy(&self) -> ReasoningPolicy {
        // Explicit user override wins over the name/url heuristic so a new
        // provider quirk can be worked around via config.toml without a
        // code change.
        if let Some(p) = self.reasoning_history_override {
            return p;
        }
        Self::derive_reasoning_policy(&self.model, &self.base_url)
    }
}

/// Repair common JSON issues in tool call arguments from weak models.
fn repair_tool_args(s: &str) -> String {
    let mut r = s.trim().to_string();

    // Remove markdown code fences
    if r.starts_with("```") {
        r = r.lines().skip(1).collect::<Vec<_>>().join("\n");
    }
    if r.ends_with("```") {
        r = r.strip_suffix("```").unwrap_or(&r).trim().to_string();
    }

    // Remove trailing commas before } or ]
    loop {
        let before = r.clone();
        r = r.replace(",}", "}").replace(",]", "]");
        if r == before {
            break;
        }
    }

    // Ensure wrapped in braces
    if !r.starts_with('{') && !r.starts_with('[') {
        r = format!("{{{}}}", r);
    }

    // Balance braces
    let open = r.chars().filter(|c| *c == '{').count();
    let close = r.chars().filter(|c| *c == '}').count();
    for _ in 0..open.saturating_sub(close) {
        r.push('}');
    }

    r
}

/// Normalize a user-provided base_url to always end with `/chat/completions`.
/// Handles common mistakes:
///   - Trailing slash: "https://api.example.com/v1/" → "https://api.example.com/v1/chat/completions"
///   - Already has endpoint: "https://api.example.com/v1/chat/completions" → kept as-is
///   - Missing /v1: "https://api.example.com" → "https://api.example.com/chat/completions"
fn normalize_base_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{}/chat/completions", base)
    }
}

fn parse_nonstream_response(body: &str) -> Option<Vec<StreamEvent>> {
    let response: ChatCompletionResponse = serde_json::from_str(body).ok()?;
    let mut events = Vec::new();

    if let Some(usage) = response.usage {
        let cached = usage
            .prompt_cache_hit_tokens
            .or(usage.cached_tokens)
            .or_else(|| {
                usage
                    .prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
            })
            .unwrap_or(0);
        events.push(StreamEvent::Usage(crate::stream::TokenUsage {
            prompt_tokens: usage.prompt_tokens.unwrap_or(0),
            completion_tokens: usage.completion_tokens.unwrap_or(0),
            cached_tokens: cached,
        }));
    }

    for choice in response.choices {
        if let Some(message) = choice.message {
            if let Some(content) = message.content {
                if !content.is_empty() {
                    events.push(StreamEvent::Delta(content));
                }
            }
            if let Some(reasoning) = message.reasoning_content {
                if !reasoning.is_empty() {
                    events.push(StreamEvent::Reasoning(reasoning));
                }
            }
        }

        let truncated = matches!(
            choice.finish_reason.as_deref(),
            Some("length") | Some("max_tokens")
        );
        events.push(StreamEvent::Done { truncated });
    }

    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

fn sample_for_error(s: &str) -> String {
    let compact = s.replace('\n', "\\n");
    let mut sample: String = compact.chars().take(160).collect();
    if compact.chars().count() > 160 {
        sample.push_str("...");
    }
    sample
}

/// Translate a `reqwest::Error` from the streaming body into something a
/// non-engineer user can act on. The bare `Display` for these errors is
/// shaped for HTTP-protocol context ("error decoding response body",
/// "operation timed out") and lands in the chat as gibberish — users
/// can't tell whether to retry, switch providers, or wait. Three buckets:
///
/// 1. `is_decode()` — the most common self-hosted-endpoint failure: the
///    server cut the chunked body mid-flight (worker timeout, OOM,
///    upstream proxy reset). Recoverable by resending; tell the user so.
/// 2. `is_timeout()` — request-level timeout. Same recovery signal.
/// 3. `is_connect()` — TCP connect failed late (rare mid-stream, but
///    possible on connection-pool churn). Recoverable.
/// Everything else falls through to the bare error text.
pub(crate) fn humanise_stream_error(e: &reqwest::Error) -> String {
    if e.is_decode() {
        format!(
            "Endpoint terminated the response stream mid-flight ({}). \
             The provider may have hit a worker timeout or upstream-proxy \
             read limit on a long generation. Try resending the message; \
             if it recurs, increase the endpoint's read/write timeouts \
             or split the request into smaller chunks.",
            e
        )
    } else if e.is_timeout() {
        format!(
            "Stream timeout ({}). The provider didn't deliver chunks \
             within the configured window. Try resending or check provider \
             status.",
            e
        )
    } else if e.is_connect() {
        format!(
            "Connection lost mid-stream ({}). Try resending; check \
             network reachability if it persists.",
            e
        )
    } else {
        format!("Stream error: {}", e)
    }
}

/// Sum of every message's `content` length plus every tool_call's
/// `arguments` length. Used as the denominator for the
/// chars/prompt_tokens ratio that flags a silently-truncating proxy.
/// We deliberately ignore JSON keys/braces — those are constant overhead
/// across all bodies and would dilute the signal.
fn sum_message_content_chars(body: &serde_json::Value) -> usize {
    let mut total = 0usize;
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return 0;
    };
    for m in msgs {
        if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
            total = total.saturating_add(s.len());
        } else if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
            // Vision multipart content: sum text fragments only (image
            // payloads are URL-or-base64 strings the model doesn't read
            // as text tokens, so counting them inflates the ratio).
            for part in arr {
                if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                    total = total.saturating_add(s.len());
                }
            }
        }
        if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    total = total.saturating_add(args.len());
                }
            }
        }
    }
    total
}

/// Returns `Some(ratio)` if the chars-per-token ratio is high enough to
/// suggest the provider silently truncated the input.
///
/// Normal tokenizers across mixed CJK/English/code run 2-4 chars/token.
/// The threshold is 6.0: any tokenizer producing 6+ chars/token would
/// be doing something unprecedented; the realistic explanation is that
/// the proxy capped the input and reported tokens for the truncated
/// view. Returns None when there's nothing to compare against.
fn check_truncation(content_chars: usize, prompt_tokens: usize) -> Option<f64> {
    // Skip tiny requests (system-only ping, etc.) — ratio noise.
    if content_chars < 4_000 || prompt_tokens == 0 {
        return None;
    }
    let ratio = content_chars as f64 / prompt_tokens as f64;
    if ratio > 6.0 {
        Some(ratio)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_truncation, parse_nonstream_response, sample_for_error, sum_message_content_chars,
        OpenAiProvider, ReasoningPolicy,
    };
    use crate::conversation::message::{ImagePart, Message, MessageContent, Role};
    use crate::stream::StreamEvent;

    /// Wire shape for `MessageContent::MultiPart`: must match OpenAI's
    /// vision schema exactly — `role: user`, `content: [...]` array,
    /// each block tagged with `type` ("image_url" or "text"). Order
    /// is image(s) first, text second. The PR added the multipart code
    /// path but no test for the wire output; without this regression
    /// guard a future field-rename or order-flip would silently break
    /// every vision-capable provider.
    #[test]
    fn multipart_serialises_to_openai_vision_schema() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("describe this".to_string()),
                images: vec![ImagePart {
                    media_type: "image/png".to_string(),
                    data: "AAAA".to_string(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let out = OpenAiProvider::format_messages(&[msg], ReasoningPolicy::Exclude, true);
        assert_eq!(out.len(), 1, "one message in, one out");
        let m = &out[0];
        assert_eq!(m["role"], "user");
        let content = m["content"].as_array().expect("content must be an array");
        assert_eq!(content.len(), 2, "image + text = 2 blocks");
        // Block 0: image, must have exactly `type` and `image_url`.
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert!(
            content[0].get("text").is_none(),
            "image block must not have text field"
        );
        // Block 1: text, must use `type: text` + `text: <string>`.
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "describe this");
    }

    /// Multi-image variant: all images come before the text block, in
    /// the order they were attached.
    #[test]
    fn multipart_preserves_image_order_then_text() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("compare".to_string()),
                images: vec![
                    ImagePart {
                        media_type: "image/png".into(),
                        data: "FIRST".into(),
                    },
                    ImagePart {
                        media_type: "image/jpeg".into(),
                        data: "SECOND".into(),
                    },
                ],
            },
            synthetic: false,
            internal_origin: None,
        };
        let out = OpenAiProvider::format_messages(&[msg], ReasoningPolicy::Exclude, true);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/png;base64,FIRST"
        );
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/jpeg;base64,SECOND"
        );
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "compare");
    }

    #[test]
    fn format_messages_skips_internal_verify_empty_assistant() {
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Text(String::new()),
            synthetic: false,
            internal_origin: Some("verify_cadence".to_string()),
        };

        let out = OpenAiProvider::format_messages(&[msg], ReasoningPolicy::Exclude, true);

        assert!(
            out.is_empty(),
            "internal verify cadence assistant must not become an empty OpenAI message"
        );
    }

    /// Image-only multipart (no caption): content array contains just
    /// the image block, no empty trailing text block.
    #[test]
    fn multipart_without_text_omits_text_block() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: None,
                images: vec![ImagePart {
                    media_type: "image/png".into(),
                    data: "X".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let out = OpenAiProvider::format_messages(&[msg], ReasoningPolicy::Exclude, true);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "single image block, no text block");
        assert_eq!(content[0]["type"], "image_url");
    }

    /// Regression: the user pasted an image with a vision-capable model
    /// (Claude/Opus), got a reply, then ran `/model` to switch to GLM-5.1
    /// (text-only) and tried to send a follow-up. The conversation still
    /// carried the historical `MultiPart` user turn; serialising it
    /// against GLM-5.1's text-only schema sent `content: [...]` to the
    /// upstream which rejected with `ModelArts.81001 message[N].content[0]
    /// has invalid field(s): text, type`. The provider must gracefully
    /// degrade `MultiPart` → text-only string when `supports_vision = false`,
    /// preserving the user's caption (with our `[Image #N]` marker still
    /// inside) but stripping the image bytes the wire schema can't
    /// represent.
    #[test]
    fn multipart_degrades_to_text_when_target_is_text_only() {
        let history = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: Some("[Image #1] 这是什么图啊".into()),
                images: vec![ImagePart {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let out = OpenAiProvider::format_messages(&[history], ReasoningPolicy::Exclude, false);
        assert_eq!(out.len(), 1);
        let m = &out[0];
        assert_eq!(m["role"], "user");
        // Content must be a flat string, NOT an array — anything else is
        // a 400 against text-only proxies (ModelArts, ZhipuAI, etc.).
        assert!(
            m["content"].is_string(),
            "text-only target must receive content as a string, got: {}",
            m["content"]
        );
        let content = m["content"].as_str().unwrap();
        assert!(
            content.contains("这是什么图啊"),
            "user's caption must survive degradation: {:?}",
            content
        );
        // No image_url block leakage.
        assert!(
            !content.contains("data:image"),
            "image bytes must not appear in degraded payload: {:?}",
            content
        );
    }

    /// When `MultiPart` had no text at all (image-only paste, no caption)
    /// and the target is text-only, the degraded payload must still be
    /// non-empty — empty user content is rejected by some proxies (e.g.
    /// "messages must contain a non-empty content"). Use a placeholder
    /// so the conversation flow stays valid.
    #[test]
    fn multipart_text_only_target_uses_placeholder_when_caption_empty() {
        let history = Message {
            role: Role::User,
            content: MessageContent::MultiPart {
                text: None,
                images: vec![ImagePart {
                    media_type: "image/png".into(),
                    data: "X".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let out = OpenAiProvider::format_messages(&[history], ReasoningPolicy::Exclude, false);
        let content = out[0]["content"].as_str().expect("string content");
        assert!(!content.is_empty(), "must be non-empty placeholder");
    }

    #[test]
    fn parses_nonstream_text_response() {
        let body = r#"{
          "choices": [
            {
              "message": { "content": "hello" },
              "finish_reason": "stop"
            }
          ],
          "usage": { "prompt_tokens": 11, "completion_tokens": 3 }
        }"#;

        let events = parse_nonstream_response(body).expect("should parse non-stream response");
        assert!(matches!(events[0], StreamEvent::Usage(_)));
        assert!(matches!(events[1], StreamEvent::Delta(ref s) if s == "hello"));
        assert!(matches!(events[2], StreamEvent::Done { truncated: false }));
    }

    #[test]
    fn parses_nonstream_reasoning_only_response() {
        let body = r#"{
          "choices": [
            {
              "message": { "reasoning_content": "thinking" },
              "finish_reason": "length"
            }
          ]
        }"#;

        let events = parse_nonstream_response(body).expect("should parse non-stream response");
        assert!(matches!(events[0], StreamEvent::Reasoning(ref s) if s == "thinking"));
        assert!(matches!(events[1], StreamEvent::Done { truncated: true }));
    }

    #[test]
    fn sample_for_error_flattens_newlines() {
        assert_eq!(sample_for_error("a\nb"), "a\\nb");
    }

    // ── ReasoningPolicy: model / base_url routing ──

    #[test]
    fn reasoning_policy_moonshot_kimi_routes_to_include() {
        use super::{OpenAiProvider, ReasoningPolicy};
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy(
                "kimi-k2-thinking",
                "https://api.moonshot.cn/v1"
            ),
            ReasoningPolicy::Include,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("kimi-k2.6", "https://api.kimi.com/v1"),
            ReasoningPolicy::Include,
        );
    }

    #[test]
    fn reasoning_policy_deepseek_reasoner_routes_to_exclude() {
        use super::{OpenAiProvider, ReasoningPolicy};
        // DeepSeek-R1 rejects the request if reasoning_content is echoed back.
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy(
                "deepseek-reasoner",
                "https://api.deepseek.com/v1"
            ),
            ReasoningPolicy::Exclude,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("deepseek-r1", "https://api.deepseek.com/v1"),
            ReasoningPolicy::Exclude,
        );
    }

    #[test]
    fn reasoning_policy_deepseek_v4_routes_to_include() {
        use super::{OpenAiProvider, ReasoningPolicy};
        // DeepSeek V4 thinking mode requires reasoning_content echoed back on
        // assistant tool_call messages — opposite of V3/R1.
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("deepseek-v4-pro", "https://api.deepseek.com"),
            ReasoningPolicy::Include,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("deepseek-v4", "https://api.deepseek.com"),
            ReasoningPolicy::Include,
        );
    }

    #[test]
    fn reasoning_policy_mimo_routes_to_include() {
        use super::{OpenAiProvider, ReasoningPolicy};
        // MiMo gateway reuses DeepSeek V4 thinking-mode protocol: returns the
        // identical 400 "reasoning_content in the thinking mode must be passed
        // back" when the field is stripped from historical tool_call messages.
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy(
                "mimo-v2.5-pro",
                "https://token-plan-cn.xiaomimimo.com/v1"
            ),
            ReasoningPolicy::Include,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy(
                "mimo-v2.5",
                "https://token-plan-cn.xiaomimimo.com/v1"
            ),
            ReasoningPolicy::Include,
        );
    }

    #[test]
    fn reasoning_history_config_override_wins_over_heuristic() {
        // `reasoning_history = "exclude"` forces Exclude even on a model that
        // the heuristic would route to Include (deepseek-v4-pro).
        use super::OpenAiProvider;
        use crate::provider::{LlmProvider, ReasoningPolicy};
        use atomcode_config::config::provider::ProviderConfig;
        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "deepseek-v4-pro".into(),
            base_url: Some("https://api.deepseek.com".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: Some("exclude".into()),
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
        };
        let p = OpenAiProvider::new(&cfg).expect("provider builds");
        assert_eq!(p.reasoning_history_policy(), ReasoningPolicy::Exclude);

        // And vice versa: "include" on a plain OpenAI model (heuristic = Exclude)
        // forces Include — lets users unblock new providers without a code change.
        let cfg_inc = ProviderConfig {
            model: "gpt-4o".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            reasoning_history: Some("include".into()),
            ..cfg
        };
        let p2 = OpenAiProvider::new(&cfg_inc).expect("provider builds");
        assert_eq!(p2.reasoning_history_policy(), ReasoningPolicy::Include);
    }

    #[test]
    fn reasoning_history_config_invalid_value_fails_fast() {
        // Typos in config should surface at load time with a clear error,
        // not a silent policy-mismatch 400 mid-turn.
        use super::OpenAiProvider;
        use atomcode_config::config::provider::ProviderConfig;
        let cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "gpt-4o".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: Some("always".into()),
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
        };
        let err = match OpenAiProvider::new(&cfg) {
            Err(e) => e,
            Ok(_) => panic!("bad reasoning_history value must reject"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("reasoning_history") && msg.contains("always"),
            "error must name the bad field and value, got: {msg}"
        );
    }

    #[test]
    fn reason_effort_applicable_only_for_deepseek_v4() {
        use super::OpenAiProvider;
        // V4 family (case-insensitive) → applicable.
        assert!(OpenAiProvider::reason_effort_applicable("deepseek-v4"));
        assert!(OpenAiProvider::reason_effort_applicable("deepseek-v4-pro"));
        assert!(OpenAiProvider::reason_effort_applicable(
            "DeepSeek-V4-Flash"
        ));
        // Everything else → no effect (must match the `ReasoningEffortNoEffect`
        // hint, which advertises DeepSeek V4 only — NOT reasoner/chat).
        assert!(!OpenAiProvider::reason_effort_applicable("deepseek-chat"));
        assert!(!OpenAiProvider::reason_effort_applicable(
            "deepseek-reasoner"
        ));
        assert!(!OpenAiProvider::reason_effort_applicable("gpt-4o"));
    }

    #[test]
    fn reasoning_effort_config_validates_and_normalises() {
        use super::OpenAiProvider;
        use atomcode_config::config::provider::ProviderConfig;
        let mut cfg = ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "deepseek-v4-pro".into(),
            base_url: Some("https://api.deepseek.com".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: Some("ultra".into()),
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
        };
        // Bad value rejected at load with a naming error (no silent mid-turn 400).
        let msg = match OpenAiProvider::new(&cfg) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("bad reasoning_effort value must reject"),
        };
        assert!(
            msg.contains("reasoning_effort") && msg.contains("ultra"),
            "error must name field+value, got: {msg}"
        );
        // Valid value, any casing, loads (and normalises to lowercase).
        cfg.reasoning_effort = Some("HIGH".into());
        assert!(
            OpenAiProvider::new(&cfg).is_ok(),
            "HIGH should normalise and load"
        );
    }

    #[test]
    fn reasoning_policy_default_is_exclude() {
        use super::{OpenAiProvider, ReasoningPolicy};
        // Unknown OpenAI-compatible endpoint → safe default: don't emit.
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("gpt-4o", "https://api.openai.com/v1"),
            ReasoningPolicy::Exclude,
        );
        assert_eq!(
            OpenAiProvider::derive_reasoning_policy("some-custom-model", "https://example.com/v1"),
            ReasoningPolicy::Exclude,
        );
    }

    // ── format_messages: reasoning_content emission per policy ──

    fn atc_message(reasoning: Option<&str>) -> crate::conversation::message::Message {
        use crate::conversation::message::{Message, MessageContent, Role};
        use crate::tool::ToolCall;
        Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some("ok".into()),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                reasoning_content: reasoning.map(|s| s.to_string()),
                thinking_blocks: Vec::new(),
            },
            synthetic: false,
            internal_origin: None,
        }
    }

    #[test]
    fn format_messages_include_with_some_reasoning_emits_field() {
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(Some("thinking text"))];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["reasoning_content"], "thinking text");
    }

    #[test]
    fn format_messages_no_tool_call_turn_echoes_real_reasoning_not_placeholder() {
        // The fix for the DeepSeek V4 Flash "(no reasoning recorded)" bug: a
        // final-answer (no tool_calls) turn that captured real reasoning is
        // stored as AssistantWithToolCalls with an empty tool_calls list, so
        // format_messages echoes the REAL reasoning back (satisfying the
        // thinking-mode contract) instead of the placeholder that flash mimics.
        use super::{OpenAiProvider, ReasoningPolicy};
        use crate::conversation::message::{Message, MessageContent, Role};
        let msgs = vec![Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some("当前系统时间是 …".into()),
                tool_calls: Vec::new(), // no tool calls — final answer
                reasoning_content: Some("用户问时间，直接回答".into()),
                thinking_blocks: Vec::new(),
            },
            synthetic: false,
            internal_origin: None,
        }];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        assert_eq!(out.len(), 1, "must not be dropped");
        assert!(
            out[0].get("tool_calls").is_none(),
            "empty tool_calls omitted"
        );
        assert_eq!(
            out[0]["reasoning_content"], "用户问时间，直接回答",
            "must echo the REAL reasoning, not the placeholder",
        );
    }

    #[test]
    fn placeholder_send_side_matches_shared_constant() {
        // The kernel agent skips reasoning→text promotion when the
        // accumulated reasoning_buf equals exactly this placeholder.
        // Send-side (format_messages, three call sites) MUST emit the
        // same byte string — otherwise a buggy gateway echoing it
        // back would slip past the guard and cause silent
        // "(no reasoning recorded) · Nailed it" stops. Pin the
        // contract by routing both sides through one constant.
        use super::{OpenAiProvider, ReasoningPolicy};
        use crate::provider::REASONING_PLACEHOLDER;
        let msgs = vec![atc_message(None)];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        assert_eq!(
            out[0]["reasoning_content"].as_str().unwrap(),
            REASONING_PLACEHOLDER,
        );
    }

    #[test]
    fn reasoning_placeholder_is_non_prose_anti_mimicry() {
        // Regression for the DeepSeek V4 Flash mimicry bug: the outbound
        // placeholder must satisfy DeepSeek's non-empty reasoning_content
        // contract WITHOUT being natural language a thinking model can copy
        // back as its own output (which surfaced "(no reasoning recorded)" as
        // the only reply and stalled the turn at high context). Pin it as a
        // short, whitespace-free, non-English sentinel so a future edit can't
        // silently regress to a mimicable phrase.
        use crate::provider::REASONING_PLACEHOLDER as P;
        assert!(!P.is_empty(), "must be non-empty for the 400 contract");
        assert!(
            !P.chars().any(|c| c.is_whitespace()),
            "must not contain whitespace — a multi-word phrase is mimicable: {P:?}"
        );
        assert!(
            P.chars().count() <= 4,
            "must be a short sentinel, not prose: {P:?}"
        );
        assert_ne!(
            P, "(no reasoning recorded)",
            "the old fluent English phrase is exactly what the model mimicked"
        );
    }

    #[test]
    fn format_messages_include_with_none_reasoning_emits_placeholder() {
        // Kimi's check is "field missing" (empty ok). DeepSeek V4's check is
        // stricter — rejects an empty string on tool_call messages. When we
        // have no stored reasoning (cross-provider session, old jsonl before
        // capture was wired, non-thinking model that tool-called anyway), emit
        // a short non-empty placeholder so BOTH providers accept the message.
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(None)];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        let rc = out[0]["reasoning_content"].as_str().unwrap();
        assert!(
            !rc.is_empty(),
            "placeholder must be non-empty for DeepSeek V4"
        );
    }

    #[test]
    fn format_messages_include_assistant_text_emits_reasoning_content() {
        // DeepSeek V4 tool-call round contract (per official docs): in every
        // subsequent request, ALL reasoning_content from the tool-call turn
        // must be echoed — including the reasoning for the FINAL TEXT answer
        // (思维链1.3 → 回答1 in the docs diagram). A plain Text variant carries
        // no reasoning, so under Include we emit the shared non-prose
        // REASONING_PLACEHOLDER: present + non-empty (the "second prompt 400"
        // contract) but NOT the old fluent sentence the thinking model mimicked
        // back as its own output. Regression for both the 400 bug AND mimicry.
        use super::{OpenAiProvider, ReasoningPolicy};
        use crate::conversation::message::{Message, MessageContent, Role};
        use crate::provider::REASONING_PLACEHOLDER;
        let msgs = vec![Message {
            role: Role::Assistant,
            content: MessageContent::Text("当前系统时间是 …".into()),
            synthetic: false,
            internal_origin: None,
        }];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["reasoning_content"].as_str(),
            Some(REASONING_PLACEHOLDER),
            "assistant Text under Include must carry the shared placeholder, got: {}",
            out[0]
        );

        // Under Exclude (V3/default) the key must NOT appear on Text — sending
        // it would regress V3 R1 which rejects any reasoning_content echo.
        let out_ex = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Exclude, true);
        assert!(
            out_ex[0]
                .as_object()
                .unwrap()
                .get("reasoning_content")
                .is_none(),
            "Exclude must not add reasoning_content to assistant Text, got: {}",
            out_ex[0]
        );
    }

    #[test]
    fn format_messages_include_with_empty_string_reasoning_emits_placeholder() {
        // Same reason as `_none_reasoning_emits_placeholder`: an empty-string
        // reasoning (either stored as "" or decayed from serde) must still be
        // replaced with the non-empty placeholder before sending.
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(Some(""))];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Include, true);
        let rc = out[0]["reasoning_content"].as_str().unwrap();
        assert!(
            !rc.is_empty(),
            "placeholder must replace empty-string reasoning"
        );
    }

    #[test]
    fn format_messages_is_deterministic_and_prefix_stable() {
        // Part-2 lock (assistant prefix-cache stability): a historical
        // assistant message must serialize to BYTE-IDENTICAL JSON every turn,
        // and growing the conversation must NOT change the serialization of the
        // earlier (prefix) messages — otherwise the provider prefix cache
        // breaks at that assistant message. This already holds because we reuse
        // the stored tool_call `arguments` string verbatim (no per-turn
        // re-stringify) and serde_json key order is deterministic; this test
        // pins it against regressions (someone re-encoding args, reordering
        // keys, or trimming reasoning_content off old messages).
        use super::{OpenAiProvider, ReasoningPolicy};
        use crate::conversation::message::{Message, MessageContent, Role};
        use crate::tool::ToolResult;

        let history = vec![
            atc_message(Some("thought about it")),
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    output: "done".into(),
                    success: true,
                }),
                synthetic: false,
                internal_origin: None,
            },
        ];

        // Same input rendered on two turns → identical bytes.
        let a = serde_json::to_string(&OpenAiProvider::format_messages(
            &history,
            ReasoningPolicy::Include,
            false,
        ))
        .unwrap();
        let b = serde_json::to_string(&OpenAiProvider::format_messages(
            &history,
            ReasoningPolicy::Include,
            false,
        ))
        .unwrap();
        assert_eq!(a, b, "assistant/tool serialization must be deterministic");

        // Next turn appends a new assistant message; the ORIGINAL prefix
        // messages must serialize byte-for-byte the same (cache grows at tail).
        let mut grown = history.clone();
        grown.push(atc_message(Some("second thought")));
        let out_prefix = OpenAiProvider::format_messages(&history, ReasoningPolicy::Include, false);
        let out_grown = OpenAiProvider::format_messages(&grown, ReasoningPolicy::Include, false);
        for (i, m) in out_prefix.iter().enumerate() {
            assert_eq!(
                serde_json::to_string(m).unwrap(),
                serde_json::to_string(&out_grown[i]).unwrap(),
                "prefix message {i} re-serialized differently after the conversation grew"
            );
        }
    }

    #[test]
    fn format_messages_exclude_omits_reasoning_content_key() {
        // DeepSeek-R1 rejects the request if reasoning_content key is present,
        // so under Exclude we must NOT emit the key even when we have a value.
        use super::{OpenAiProvider, ReasoningPolicy};
        let msgs = vec![atc_message(Some("should be stripped"))];
        let out = OpenAiProvider::format_messages(&msgs, ReasoningPolicy::Exclude, true);
        assert!(
            out[0]
                .as_object()
                .unwrap()
                .get("reasoning_content")
                .is_none(),
            "reasoning_content key must be absent under Exclude, got: {}",
            out[0]
        );
    }

    // ── thinking config → request body ──

    #[test]
    fn thinking_body_none_when_both_unset() {
        use super::OpenAiProvider;
        // Unset = don't emit the key at all. Some OpenAI-compatible gateways
        // 400 on unknown top-level fields, so missing is safer than `{}`.
        assert!(OpenAiProvider::thinking_body_value(None, None).is_none());
    }

    #[test]
    fn thinking_body_disabled_emits_type_only() {
        use super::OpenAiProvider;
        let out = OpenAiProvider::thinking_body_value(Some("disabled"), None).unwrap();
        assert_eq!(out, serde_json::json!({"type": "disabled"}));
    }

    #[test]
    fn thinking_body_enabled_with_keep_all() {
        use super::OpenAiProvider;
        // K2.6 Preserved Thinking: the reference combination from Kimi docs.
        let out = OpenAiProvider::thinking_body_value(Some("enabled"), Some("all")).unwrap();
        assert_eq!(out, serde_json::json!({"type": "enabled", "keep": "all"}));
    }

    #[test]
    fn thinking_fields_roundtrip_via_toml_provider_config() {
        // The TOML shape users will write in config.toml — flat, with a
        // `thinking_` prefix so each field's purpose is obvious on its own.
        use atomcode_config::config::provider::ProviderConfig;
        let toml = r#"
            type = "openai"
            model = "kimi-k2.6"
            base_url = "https://api.moonshot.cn/v1"
            api_key = "sk-x"
            thinking_type = "enabled"
            thinking_keep = "all"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml).expect("TOML parse");
        assert_eq!(cfg.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(cfg.thinking_keep.as_deref(), Some("all"));
    }

    // ── serde backward compat for old session jsonl ──

    #[test]
    fn old_jsonl_without_reasoning_content_still_deserializes() {
        // Session jsonl written before this field existed must still load.
        // `#[serde(default)]` on the field makes this work.
        use crate::conversation::message::MessageContent;
        let old = r#"{"AssistantWithToolCalls":{"text":"hi","tool_calls":[]}}"#;
        let parsed: MessageContent = serde_json::from_str(old)
            .expect("old-format AssistantWithToolCalls should deserialize");
        match parsed {
            MessageContent::AssistantWithToolCalls {
                text,
                reasoning_content,
                ..
            } => {
                assert_eq!(text.as_deref(), Some("hi"));
                assert!(reasoning_content.is_none());
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    // ── provider truncation detector ──

    #[test]
    fn truncation_detector_flags_gitcode_real_world_ratio() {
        // 5/8 atomgr session: GitCode reported 6233 prompt_tokens for a
        // body atomcode counted at ~78K content chars. Ratio 12.58.
        // This is the canary: if check_truncation ever stops firing on
        // this number, weak-model debugging gets harder by hours.
        let ratio = check_truncation(78_381, 6_233)
            .expect("12.58 chars/token must be flagged as truncation");
        assert!(ratio > 12.0 && ratio < 13.0, "ratio={}", ratio);
    }

    #[test]
    fn truncation_detector_silent_on_normal_tokenizer() {
        // Siliconflow Pro/zai-org/GLM-5 same session: 127K chars / 45K
        // tokens = 2.81. Healthy upstream — must not warn.
        assert!(check_truncation(127_763, 45_518).is_none());
    }

    #[test]
    fn truncation_detector_silent_on_english_heavy_4chars_per_token() {
        // Pure-English code-only request can hit ~4 chars/token. The
        // threshold (6.0) leaves headroom so non-truncated requests
        // never noise the log.
        assert!(check_truncation(40_000, 10_000).is_none());
    }

    #[test]
    fn truncation_detector_skips_tiny_bodies() {
        // System-only ping or a bare "hi" — ratio noise dominates,
        // so the detector stays silent under 4K chars regardless of
        // the count.
        assert!(check_truncation(1_500, 100).is_none());
    }

    #[test]
    fn truncation_detector_handles_zero_prompt_tokens() {
        // Some self-hosted gateways drop usage entirely. Don't divide
        // by zero, just stay silent.
        assert!(check_truncation(50_000, 0).is_none());
    }

    #[test]
    fn sum_message_content_chars_sums_strings_and_tool_args() {
        let body = serde_json::json!({
            "model": "x",
            "messages": [
                {"role": "system", "content": "abc"},     // 3
                {"role": "user", "content": "hello"},      // 5
                {"role": "assistant", "content": "",
                 "tool_calls": [
                     {"function": {"name": "read_file",
                                   // JSON-decoded length = 12 chars
                                   "arguments": "{\"path\":\"a\"}"}},
                 ]},
                {"role": "tool", "content": "result"},     // 6
            ]
        });
        assert_eq!(sum_message_content_chars(&body), 3 + 5 + 12 + 6);
    }

    #[test]
    fn sum_message_content_chars_ignores_image_urls_in_multipart() {
        // Vision payloads have URL/base64 strings that aren't real
        // text tokens — counting them would falsely inflate the
        // chars/token ratio for vision requests.
        let body = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},  // 8
                    {"type": "image_url",
                     "image_url": {"url": "data:image/png;base64,AAAAAAAAAA"}},
                ]
            }]
        });
        assert_eq!(sum_message_content_chars(&body), 8);
    }

    #[test]
    fn sum_message_content_chars_safe_on_missing_messages() {
        let body = serde_json::json!({"model": "x"});
        assert_eq!(sum_message_content_chars(&body), 0);
    }

    // ── abrupt-close gateway-error discriminator ───────────────────
    //
    // GLM-5.1 / litellm-style gateways respond to a 429 by streaming
    // a single SSE chunk carrying a Chinese error message and then
    // hanging up without `data: [DONE]`. Before this code path
    // existed, the provider mapped both that case AND "real
    // mid-output truncation" to `Done { truncated: true }`, causing
    // the agent's resume-from-truncation retry to re-fire the same
    // request and render the same error message twice. Tests below
    // pin the new behavior:
    //
    //   * 1 short content chunk, no `[DONE]`           → Error
    //   * many content chunks + abrupt close           → Done(truncated=true)
    //   * 1 short chunk + tool_call + abrupt close     → Done(truncated=true)
    //     (model was making tool progress; let resume retry try again)

    use crate::provider::LlmProvider;
    use atomcode_config::config::provider::ProviderConfig;
    use futures::StreamExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider_pointing_at(url: &str) -> OpenAiProvider {
        OpenAiProvider::new(&ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "test-model".into(),
            base_url: Some(format!("{}/v1", url)),
            system_prompt: None,
            user_agent: None,
            context_window: 8000,
            max_tokens: Some(1024),
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
        })
        .expect("provider construction")
    }

    async fn collect_stream(p: &OpenAiProvider) -> Vec<StreamEvent> {
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            synthetic: false,
            internal_origin: None,
        };
        let mut stream = p.chat_stream(&[msg], None).expect("stream");
        let mut out = Vec::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(e) => out.push(e),
                Err(e) => panic!("transport error: {:#}", e),
            }
        }
        out
    }

    /// Gateway streams ONE chunk with an error blob, no DONE, then
    /// closes. Provider must surface that as `Error(blob)`, NOT as
    /// `Done { truncated: true }` (which would trigger the agent's
    /// resume-retry and render the same blob twice).
    #[tokio::test]
    async fn abrupt_close_with_single_error_chunk_becomes_stream_error() {
        let server = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\
                   \"模型「GLM-5.1」的请求负载过高，请稍后再试。\"}}]}\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let p = provider_pointing_at(&server.uri());
        let events = collect_stream(&p).await;
        let has_error = events
            .iter()
            .any(|e| matches!(e, StreamEvent::Error(s) if s.contains("请求负载过高")));
        let has_truncated_done = events
            .iter()
            .any(|e| matches!(e, StreamEvent::Done { truncated: true }));
        let has_marker_delta = events.iter().any(|e| {
            matches!(e, StreamEvent::Delta(s) if s.contains("stream ended without close marker"))
        });
        assert!(
            has_error,
            "expected StreamEvent::Error(gateway blob), got: {:?}",
            events
        );
        assert!(
            !has_truncated_done,
            "abrupt close on tiny error blob must NOT emit Done(truncated=true): {:?}",
            events
        );
        assert!(
            !has_marker_delta,
            "abrupt close on tiny error blob must NOT emit the [stream ended …] marker delta: {:?}",
            events
        );
    }

    const CLEAN_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n";

    /// Once `set_session_id` is called, every request must carry the
    /// `x-atomcode-session-id` header so the forwarding gateway can pin the
    /// conversation to one upstream for prefix-cache affinity.
    #[tokio::test]
    async fn sends_x_atomcode_session_id_header_when_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(CLEAN_SSE),
            )
            .mount(&server)
            .await;

        let p = provider_pointing_at(&server.uri());
        p.set_session_id("affinity-key-xyz");
        let _ = collect_stream(&p).await;

        let reqs = server.received_requests().await.expect("requests recorded");
        assert_eq!(reqs.len(), 1, "exactly one request");
        let hdr = reqs[0].headers.get("x-atomcode-session-id");
        assert!(hdr.is_some(), "header must be present when session id set");
        assert_eq!(hdr.unwrap().to_str().unwrap(), "affinity-key-xyz");
    }

    /// `reset_session_id` (on /clear, /session, resume) re-calls
    /// `set_session_id`; the header must reflect the LATEST value so a new
    /// conversation gets its new id, not the process's first one.
    #[tokio::test]
    async fn set_session_id_is_resettable_latest_wins() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(CLEAN_SSE),
            )
            .mount(&server)
            .await;

        let p = provider_pointing_at(&server.uri());
        p.set_session_id("old-session");
        p.set_session_id("new-session"); // simulates reset_session_id
        let _ = collect_stream(&p).await;

        let reqs = server.received_requests().await.expect("requests recorded");
        assert_eq!(
            reqs[0]
                .headers
                .get("x-atomcode-session-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "new-session"
        );
    }

    /// Without a session id attached (e.g. summary / sub-agent calls), the
    /// header must be omitted entirely — an empty value would pin all such
    /// calls together on the gateway, which is wrong.
    #[tokio::test]
    async fn omits_session_header_when_unset() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(CLEAN_SSE),
            )
            .mount(&server)
            .await;

        let p = provider_pointing_at(&server.uri());
        // no set_session_id call
        let _ = collect_stream(&p).await;

        let reqs = server.received_requests().await.expect("requests recorded");
        assert!(
            reqs[0].headers.get("x-atomcode-session-id").is_none(),
            "header must be absent when no session id attached"
        );
    }

    /// Real-truncation case: many chunks of substantive content,
    /// then abrupt close (no DONE / no finish_reason). Stays on the
    /// existing `Done { truncated: true }` path so the agent's
    /// "resume where you left off" retry can salvage the partial
    /// output (table-cut, list-cut, etc.).
    #[tokio::test]
    async fn abrupt_close_with_substantive_content_still_emits_truncated_done() {
        let server = MockServer::start().await;
        // 5 chunks × ~50 chars each = ~250 chars of real content.
        // Above the 200-char and 2-chunk thresholds → not a
        // gateway error.
        let mut sse = String::new();
        for i in 0..5 {
            sse.push_str(&format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\
                 \"line {} with enough content to clear the heuristic thresholds. \"}}}}]}}\n\n",
                i
            ));
        }
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let p = provider_pointing_at(&server.uri());
        let events = collect_stream(&p).await;
        let has_truncated_done = events
            .iter()
            .any(|e| matches!(e, StreamEvent::Done { truncated: true }));
        let has_error = events.iter().any(|e| matches!(e, StreamEvent::Error(_)));
        assert!(
            has_truncated_done,
            "substantive content + abrupt close must keep Done(truncated=true): {:?}",
            events
        );
        assert!(
            !has_error,
            "real truncation must NOT be misclassified as Error: {:?}",
            events
        );
    }

    // 401 from a non-atomgit gateway must keep the verbatim server
    // error message — user-supplied `sk-...` API keys are not
    // refreshable by us, and developers debugging a bad key need to
    // see what the upstream actually said. The new auth-retry path is
    // only allowed to engage for AtomGit gateway hosts.
    #[tokio::test]
    async fn non_atomgit_gateway_401_keeps_verbatim_error() {
        let server = MockServer::start().await;
        let body = r#"{"error":{"message":"invalid_api_key"}}"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("content-type", "application/json")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let p = provider_pointing_at(&server.uri());
        let events = collect_stream(&p).await;
        let err_msg = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Error(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected StreamEvent::Error, got: {:?}", events));
        assert!(
            err_msg.contains("invalid_api_key"),
            "non-atomgit 401 must include verbatim server message: {}",
            err_msg
        );
        // The i18n-friendly hint is reserved for atomgit-gateway 401s;
        // it must NOT leak into the generic OpenAI / sk-... path.
        let friendly = crate::i18n::t(crate::i18n::Msg::ChatAuthExpired).to_string();
        assert!(
            !err_msg.contains(&friendly),
            "non-atomgit 401 must NOT receive the AtomGit /login hint: got {}",
            err_msg
        );
    }
}

#[cfg(test)]
mod chat_auth_expired_i18n_tests {
    use crate::i18n::{t_with, Locale, Msg};

    #[test]
    fn message_is_non_empty_in_both_locales() {
        // Both must surface SOMETHING — an empty hint defeats the
        // entire point of this branch (point user at /login).
        let zh = t_with(Locale::ZhCn, Msg::ChatAuthExpired).to_string();
        let en = t_with(Locale::En, Msg::ChatAuthExpired).to_string();
        assert!(!zh.is_empty(), "zh message must be populated");
        assert!(!en.is_empty(), "en message must be populated");
    }

    #[test]
    fn message_mentions_login_in_both_locales() {
        // The whole point of this message is to direct the user to
        // re-authenticate. If a future translator drops the `/login`
        // reference the hint becomes useless.
        let zh = t_with(Locale::ZhCn, Msg::ChatAuthExpired).to_string();
        let en = t_with(Locale::En, Msg::ChatAuthExpired).to_string();
        assert!(
            zh.contains("/login"),
            "zh message must mention /login: {}",
            zh
        );
        assert!(
            en.contains("/login"),
            "en message must mention /login: {}",
            en
        );
    }
}

#[cfg(test)]
mod codingplan_signing_tests {
    use super::*;

    #[test]
    fn build_signed_headers_returns_empty_for_non_atomgit_host() {
        let headers = build_codingplan_headers("https://api.openai.com/v1", b"{}", None)
            .expect("non-atomgit host must not error");
        assert!(headers.is_empty(), "got unexpected headers: {:?}", headers);
    }

    #[test]
    fn build_signed_headers_errors_when_atomgit_host_in_open_source_build() {
        // Open-source build: signer() is UnavailableSigner, so an
        // atomgit-bound request must error with the localised hint.
        let err = build_codingplan_headers(
            "https://pre-llm-api-cce.atomgit.com/v1",
            b"{}",
            Some(("dummy-user-id", "dummy-token")),
        )
        .expect_err("open-source build must error out");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("official") || msg.contains("官方"),
            "error message should mention the official-build requirement, got: {msg}",
        );
    }

    #[test]
    fn build_signed_headers_errors_when_atomgit_host_with_empty_auth() {
        let err = build_codingplan_headers(
            "https://pre-llm-api-cce.atomgit.com/v1",
            b"{}",
            Some(("", "")),
        )
        .expect_err("empty auth must error");
        assert!(!format!("{:#}", err).is_empty());
    }

    #[cfg(not(feature = "codingplan-crypto"))]
    #[test]
    fn open_source_build_errors_official_build_required_before_auth_check() {
        // Regression guard: when reordering build_codingplan_headers,
        // the signer-availability check MUST run before any auth lookup
        // so an open-source user with empty auth gets the actionable
        // "need official build" hint instead of a misleading "/login"
        // prompt that would never succeed.
        let err = build_codingplan_headers(
            "https://llm-api.atomgit.com/v1",
            b"{}",
            Some(("", "")), // empty auth
        )
        .expect_err("must error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("official") || msg.contains("官方"),
            "open-source + empty-auth must surface the official-build hint, not the auth hint. got: {msg}",
        );
    }
}
