//! What models an endpoint offers, from its own listing — `GET {base}/models`
//! (OpenAI-shaped and Responses), `{base}/v1/models` (Anthropic), `/api/tags`
//! (Ollama) — so a person adding a model picks it rather than typing its id.
//! Alibaba's Anthropic gateway uses its same-origin OpenAI model listing.
//!
//! One implementation for both front ends that offer it: the web UI's
//! `POST /providers/discover-models` and the terminal's `/provider` panel.
//!
//! The key is sent only to the endpoint it belongs to: redirects are not
//! followed (reqwest drops `Authorization` across hosts, but not Anthropic's
//! `x-api-key`), and a URL carrying credentials is refused.

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[path = "discovery_catalog.rs"]
pub mod catalog;

/// How long a listing may take.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a listing read; a bigger answer is refused rather than buffered.
pub const DISCOVERY_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// The most models kept from one listing.
pub const DISCOVERY_MAX_MODELS: usize = 2_000;

/// One model an endpoint lists, with what it said about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort_levels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Public catalog default shown as a hint, never saved as a request override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub catalog_only: bool,
}

/// Everything in one call: the listing of the endpoint at `base_url`, spoken in
/// `provider_type`'s protocol. `Err` says why there is none — the protocol
/// lists nothing, the URL is unusable, the endpoint refused or did not answer,
/// or the answer was not a listing.
pub async fn discover(
    base_url: &str,
    provider_type: &str,
    transport: &DiscoveryTransport,
) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let result = discover_endpoint(base_url, provider_type, transport).await;
    if matches!(&result, Ok(models) if models.iter().all(|model| !catalog::needs_metadata(model)) && !models.is_empty())
    {
        return result;
    }
    catalog::merge_listing(result, catalog::models(base_url, provider_type).await)
}

async fn discover_endpoint(
    base_url: &str,
    provider_type: &str,
    transport: &DiscoveryTransport,
) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let protocol = discovery_protocol(provider_type).ok_or(DiscoveryError::Unsupported)?;
    let url = discovery_url(base_url, provider_type).map_err(DiscoveryError::BadUrl)?;
    let body = fetch_discovery_body(url, protocol, transport, DISCOVERY_TIMEOUT)
        .await
        .map_err(DiscoveryError::Request)?;
    parse_discovered_models(protocol, &body).map_err(|_| DiscoveryError::NotAListing)
}

/// Why [`discover`] has no listing.
#[derive(Debug)]
pub enum DiscoveryError {
    /// The protocol has no model listing; the model is typed by hand.
    Unsupported,
    BadUrl(String),
    Request(DiscoveryRequestError),
    /// It answered, and not with a listing.
    NotAListing,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelEntry {
    id: String,
    name: Option<String>,
    display_name: Option<String>,
    context_window: Option<usize>,
    context_length: Option<usize>,
    max_tokens: Option<usize>,
    max_output_tokens: Option<usize>,
    #[serde(default, deserialize_with = "optional_metadata")]
    input_modalities: Option<Vec<String>>,
    #[serde(default, deserialize_with = "optional_metadata")]
    effort: Option<ModelEffort>,
}

/// Optional vendor extensions must not invalidate an otherwise usable listing.
/// Unknown shapes are left unknown, just like omitted metadata.
fn optional_metadata<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

#[derive(Debug, Deserialize)]
struct ModelEffort {
    supported_levels: Vec<String>,
    default_level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelsResponse {
    models: Vec<OllamaModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelEntry {
    name: Option<String>,
    model: Option<String>,
}

pub fn discovery_url(base_url: &str, provider_type: &str) -> Result<reqwest::Url, String> {
    // Each wire's own listing, relative to the base the adapter sends chat to:
    // Anthropic's base carries no version (the adapter appends `/v1/messages`).
    let suffix = match discovery_protocol(provider_type) {
        Some("ollama") => "/api/tags",
        Some("anthropic") => "/v1/models",
        _ => "/models",
    };
    let mut url = reqwest::Url::parse(base_url.trim()).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("model discovery supports only http and https URLs".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("model discovery URL must not contain credentials".into());
    }
    // Alibaba's Anthropic-compatible gateway implements Messages but not the
    // Models API. Its paired OpenAI listing uses the same origin and API key:
    // https://help.aliyun.com/zh/model-studio/base-url
    // Keep this vendor-specific; never guess a sibling endpoint on other hosts.
    if suffix == "/v1/models"
        && is_aliyun_model_host(&url)
        && url.path().trim_end_matches('/') == "/apps/anthropic"
    {
        url.set_path("/compatible-mode/v1/models");
        url.set_query(None);
        url.set_fragment(None);
        return Ok(url);
    }
    // These vendors document a single, same-origin model catalog shared by
    // their chat protocols (LongCat APIDocs; Kimi API model-list reference).
    let shared_catalog = matches!(
        (
            url.host_str().unwrap_or_default(),
            url.path().trim_end_matches('/'),
            discovery_protocol(provider_type)
        ),
        ("api.longcat.chat", "/anthropic", Some("anthropic"))
            | ("api.longcat.chat", "/openai/v1", Some("openai"))
            | ("api.moonshot.cn", "/anthropic", Some("anthropic"))
            | ("tokenhub.tencentmaas.com", "", Some("anthropic"))
    );
    if shared_catalog {
        url.set_path("/v1/models");
        url.set_query(None);
        url.set_fragment(None);
        return Ok(url);
    }
    let path = format!("{}{}", url.path().trim_end_matches('/'), suffix);
    url.set_path(&path);
    // Anthropic pages its list (20 by default); one request for all of them.
    url.set_query((suffix == "/v1/models").then_some("limit=1000"));
    url.set_fragment(None);
    Ok(url)
}

fn is_aliyun_model_host(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.ends_with(".maas.aliyuncs.com")
            || matches!(
                host,
                "dashscope.aliyuncs.com"
                    | "dashscope-intl.aliyuncs.com"
                    | "dashscope-us.aliyuncs.com"
                    | "cn-hongkong.dashscope.aliyuncs.com"
            )
    })
}

fn discovery_auth(
    request: reqwest::RequestBuilder,
    url: &reqwest::Url,
    protocol: &str,
    key: Option<&str>,
) -> reqwest::RequestBuilder {
    // TokenHub's shared Models API requires Bearer, while its ordinary
    // Messages API uses x-api-key (unlike its subscription gateway).
    // https://cloud.tencent.com/document/product/1823/130078
    let openai_listing = (is_aliyun_model_host(url) && url.path() == "/compatible-mode/v1/models")
        || (url.host_str() == Some("tokenhub.tencentmaas.com") && url.path() == "/v1/models");
    if protocol == "anthropic" && !openai_listing {
        let request = request.header("anthropic-version", "2023-06-01");
        match key {
            Some(key) => super::anthropic::authenticate(request, url.as_str(), key.trim()),
            None => request,
        }
    } else {
        match key {
            Some(key) => request.bearer_auth(key.trim()),
            None => request,
        }
    }
}

pub fn discovery_protocol(provider_type: &str) -> Option<&'static str> {
    match provider_type.trim().to_ascii_lowercase().as_str() {
        // The Responses adapter targets an OpenAI-shaped endpoint, so model
        // discovery uses the SAME `/models` transport as the chat/completions
        // ("openai") provider — without this arm a Responses account resolves to
        // None and the add-model / discovery flow silently rejects it.
        "openai" | "openai-compat" | "openai_compat" | "responses" => Some("openai"),
        "ollama" => Some("ollama"),
        // `GET /v1/models`, the same `data` array shape as OpenAI's.
        "anthropic" => Some("anthropic"),
        _ => None,
    }
}

/// How to reach the endpoint: its key (never logged), user agent, and TLS
/// policy — the same three a chat request to it uses.
#[derive(Default)]
pub struct DiscoveryTransport {
    pub api_key: Option<String>,
    pub user_agent: Option<String>,
    pub headers: atomcode_config::provider_headers::ProviderHeaders,
    pub skip_tls_verify: bool,
}

impl std::fmt::Debug for DiscoveryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryTransport")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("user_agent", &self.user_agent)
            .field("skip_tls_verify", &self.skip_tls_verify)
            .finish()
    }
}

pub fn parse_discovered_models(
    provider_type: &str,
    body: &[u8],
) -> Result<Vec<DiscoveredModel>, serde_json::Error> {
    let models = if provider_type == "ollama" {
        serde_json::from_slice::<OllamaModelsResponse>(body)?
            .models
            .into_iter()
            .filter_map(|entry| entry.model.or(entry.name))
            .map(|id| DiscoveredModel {
                id,
                name: None,
                context_window: None,
                ..Default::default()
            })
            .collect()
    } else {
        serde_json::from_slice::<OpenAiModelsResponse>(body)?
            .data
            .into_iter()
            .map(|entry| DiscoveredModel {
                id: entry.id,
                name: entry.name.or(entry.display_name),
                context_window: entry
                    .context_window
                    .or(entry.context_length)
                    .filter(|n| *n > 0),
                max_tokens: entry
                    .max_output_tokens
                    .or(entry.max_tokens)
                    .filter(|n| *n > 0),
                supports_vision: entry
                    .input_modalities
                    .as_ref()
                    .map(|m| m.iter().any(|m| m == "image")),
                reasoning_effort: entry.effort.as_ref().and_then(|e| {
                    e.default_level
                        .clone()
                        .filter(|v| e.supported_levels.contains(v))
                }),
                reasoning_effort_levels: entry.effort.map(|e| e.supported_levels),
                catalog_default_effort: None,
                catalog_only: false,
            })
            .collect()
    };
    Ok(normalize_discovered_models(models))
}

#[derive(Debug)]
enum DiscoveryReadError {
    ResponseTooLarge,
    Transport(reqwest::Error),
}

async fn read_bounded_response(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, DiscoveryReadError> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(DiscoveryReadError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DiscoveryReadError::Transport)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(DiscoveryReadError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Why a listing could not be had.
#[derive(Debug)]
pub enum DiscoveryRequestError {
    Timeout,
    /// The Ollama endpoint refused a connection or did not answer in time.
    OllamaUnavailable {
        endpoint: String,
        local: bool,
    },
    ResponseTooLarge,
    UpstreamStatus(u16),
    Transport,
}

impl DiscoveryRequestError {
    /// Actionable guidance shared by drivers, without changing the HTTP schema.
    pub fn ollama_guidance(&self) -> Option<String> {
        let Self::OllamaUnavailable { endpoint, local } = self else {
            return None;
        };
        Some(if *local {
            format!("Could not connect to Ollama at {endpoint}. Run `ollama serve` on the machine running atomcode, then retry model discovery. The default port is 11434; change the address if your daemon uses another port.")
        } else {
            format!("Could not connect to Ollama at {endpoint}. Check that Ollama is running on the target machine (`ollama serve`) and that its address, port and network access are correct, then retry model discovery.")
        })
    }
}

fn ollama_connection_failure(
    url: &reqwest::Url,
    protocol: &str,
    error: &reqwest::Error,
    proxy_may_apply: bool,
) -> Option<DiscoveryRequestError> {
    use std::error::Error;
    // HTTP proxy refusal has the same IO error as a direct endpoint refusal.
    // Use request policy, not error text (which also contains the target URL).
    if protocol != "ollama" || proxy_may_apply {
        return None;
    }
    let mut cause: Option<&(dyn Error + 'static)> = Some(error);
    let mut refused = false;
    while let Some(current) = cause {
        refused |= current
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused);
        cause = current.source();
    }
    if !(error.is_timeout() || refused) {
        return None;
    }
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    // The URL was validated before the request; no credentials or query are shown.
    let mut endpoint = url.clone();
    let path = endpoint
        .path()
        .strip_suffix("/api/tags")
        .unwrap_or(endpoint.path())
        .to_string();
    endpoint.set_path(&path);
    Some(DiscoveryRequestError::OllamaUnavailable {
        endpoint: endpoint.to_string(),
        local,
    })
}

/// Conservatively withhold startup diagnostics when a proxy could be the
/// failed peer. Exact/domain bypasses are known direct; unhandled CIDR bypasses
/// retain the generic transport error rather than guessing which peer failed.
fn discovery_proxy_may_apply(
    url: &reqwest::Url,
    policy: Option<&atomcode_config::proxy::ProxyConfig>,
) -> bool {
    use atomcode_config::proxy::{ProxyConfig, ProxyMode, MODE_ENV};
    let captured;
    let policy = match policy {
        Some(policy) => policy,
        None => {
            if std::env::var(MODE_ENV).ok().as_deref() == Some("no_proxy") {
                return false;
            }
            // The installed policy may have added loopback bypasses or restored
            // pinned proxy settings since startup; read the current process route.
            let env = |names: &[&str]| {
                names.iter().find_map(|name| {
                    std::env::var(name)
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                })
            };
            captured = ProxyConfig {
                mode: ProxyMode::DefaultProxy,
                http: env(&["HTTP_PROXY", "http_proxy"]),
                https: env(&["HTTPS_PROXY", "https_proxy"]),
                all: env(&["ALL_PROXY", "all_proxy"]),
                no_proxy: env(&["NO_PROXY", "no_proxy"]),
            };
            &captured
        }
    };
    if policy.mode == ProxyMode::NoProxy {
        return false;
    }
    let scheme_proxy = if url.scheme() == "https" {
        &policy.https
    } else {
        &policy.http
    };
    if scheme_proxy.as_ref().or(policy.all.as_ref()).is_none() {
        return false;
    }
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    !policy
        .no_proxy
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .any(|entry| {
            let entry = entry
                .trim()
                .trim_start_matches('.')
                .trim_matches(['[', ']']);
            !entry.is_empty()
                && (entry == "*"
                    || host.eq_ignore_ascii_case(entry)
                    || (entry.parse::<std::net::IpAddr>().is_err()
                        && host
                            .to_ascii_lowercase()
                            .ends_with(&format!(".{}", entry.to_ascii_lowercase()))))
        })
}

/// The chat adapters' network policy — the `/proxy` setting and the OS /
/// `SSL_CERT_FILE` roots — so a listing reaches every endpoint a chat does.
/// `trust_os_roots = false` is the same webpki-only backstop the chat client
/// falls back to when a bad root would abort the build.
fn discovery_client(
    transport: &DiscoveryTransport,
    timeout: Duration,
    trust_os_roots: bool,
    proxy: Option<&atomcode_config::proxy::ProxyConfig>,
) -> Result<reqwest::Client, atomcode_kernel::stream::ProviderError> {
    let mut client = super::auto_proxy::apply(
        crate::proxy::apply_async_proxy_policy(reqwest::Client::builder()),
        proxy,
    )?
    .timeout(timeout)
    .redirect(reqwest::redirect::Policy::none())
    .danger_accept_invalid_certs(transport.skip_tls_verify);
    if trust_os_roots && !cfg!(target_os = "windows") {
        client = super::openai_compat::add_trusted_roots(client);
    }
    if let Some(user_agent) = transport.user_agent.as_deref() {
        client = client.user_agent(user_agent);
    }
    client
        .build()
        .map_err(|_| atomcode_kernel::stream::ProviderError {
            message: "discovery client build failed".into(),
            ..Default::default()
        })
}

pub async fn fetch_discovery_body(
    url: reqwest::Url,
    protocol: &str,
    transport: &DiscoveryTransport,
    timeout: Duration,
) -> Result<Vec<u8>, DiscoveryRequestError> {
    // No redirects: a key is bound to the endpoint it was saved for, and a 30x
    // to another host would carry it there — reqwest strips `Authorization`
    // across hosts, but not Anthropic's `x-api-key`.
    let proxy = super::auto_proxy::for_request()
        .await
        .map_err(|_| DiscoveryRequestError::Transport)?;
    let client = discovery_client(transport, timeout, true, proxy.as_ref())
        .or_else(|_| discovery_client(transport, timeout, false, proxy.as_ref()))
        .map_err(|_| DiscoveryRequestError::Transport)?;
    let request = discovery_auth(
        client.get(url.clone()).header("accept", "application/json"),
        &url,
        protocol,
        transport.api_key.as_deref(),
    );
    let request = super::apply_account_headers(request, &transport.headers)
        .map_err(|_| DiscoveryRequestError::Transport)?;
    let proxy_may_apply = protocol == "ollama" && discovery_proxy_may_apply(&url, proxy.as_ref());
    let response = request.send().await.map_err(|error| {
        ollama_connection_failure(&url, protocol, &error, proxy_may_apply).unwrap_or_else(|| {
            if error.is_timeout() {
                DiscoveryRequestError::Timeout
            } else {
                DiscoveryRequestError::Transport
            }
        })
    })?;
    if !response.status().is_success() {
        return Err(DiscoveryRequestError::UpstreamStatus(
            response.status().as_u16(),
        ));
    }
    read_bounded_response(response, DISCOVERY_MAX_RESPONSE_BYTES)
        .await
        .map_err(|error| match error {
            DiscoveryReadError::ResponseTooLarge => DiscoveryRequestError::ResponseTooLarge,
            DiscoveryReadError::Transport(error) if error.is_timeout() => {
                DiscoveryRequestError::Timeout
            }
            DiscoveryReadError::Transport(_) => DiscoveryRequestError::Transport,
        })
}

pub fn normalize_discovered_models(mut models: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    models.retain(|model| !model.id.trim().is_empty());
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models.truncate(DISCOVERY_MAX_MODELS);
    models
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn aliyun_anthropic_lists_models_on_its_same_origin_openai_endpoint() {
        let client = reqwest::Client::new();
        for host in [
            "token-plan.cn-beijing.maas.aliyuncs.com",
            "workspace.ap-southeast-1.maas.aliyuncs.com",
            "dashscope.aliyuncs.com",
            "dashscope-intl.aliyuncs.com",
            "dashscope-us.aliyuncs.com",
            "cn-hongkong.dashscope.aliyuncs.com",
        ] {
            let base = format!("https://{host}:8443/apps/anthropic/?old=query#fragment");
            let url = discovery_url(&base, "anthropic").unwrap();
            assert_eq!(
                url.as_str(),
                format!("https://{host}:8443/compatible-mode/v1/models")
            );
            let request = discovery_auth(
                client.get(url.clone()),
                &url,
                "anthropic",
                Some(" test-key "),
            )
            .build()
            .unwrap();
            assert_eq!(request.headers()["authorization"], "Bearer test-key");
            assert!(!request.headers().contains_key("x-api-key"));
            assert!(!request.headers().contains_key("anthropic-version"));
        }
    }

    #[test]
    fn tokenhub_listing_uses_bearer_but_messages_keep_api_key_auth() {
        let client = reqwest::Client::new();
        for (base, protocol) in [
            ("https://tokenhub.tencentmaas.com/", "anthropic"),
            ("https://tokenhub.tencentmaas.com/v1", "openai"),
        ] {
            let url = discovery_url(base, protocol).unwrap();
            assert_eq!(url.as_str(), "https://tokenhub.tencentmaas.com/v1/models");
            let request =
                discovery_auth(client.get(url.clone()), &url, protocol, Some(" test-key "))
                    .build()
                    .unwrap();
            assert_eq!(request.headers()["authorization"], "Bearer test-key");
            assert!(!request.headers().contains_key("x-api-key"));
            assert!(!request.headers().contains_key("anthropic-version"));
        }
        for url in [
            "https://tokenhub.tencentmaas.com/v1/messages",
            "https://tokenhub.tencentmaas.com.example.test/v1/models",
            "https://tokenhub.tencentmaas.com/custom/v1/models",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            let request = discovery_auth(client.get(url), &parsed, "anthropic", Some("test-key"))
                .build()
                .unwrap();
            assert_eq!(request.headers()["x-api-key"], "test-key");
            assert!(!request.headers().contains_key("authorization"));
        }
    }

    #[test]
    fn compatible_vendors_use_their_documented_catalog_and_auth() {
        let client = reqwest::Client::new();
        for (base, path) in [
            ("https://api.longcat.chat/anthropic", "/v1/models"),
            ("https://api.moonshot.cn/anthropic", "/v1/models"),
            ("https://api.siliconflow.cn", "/v1/models"),
            ("https://openrouter.ai/api", "/api/v1/models"),
            (
                "https://api.lkeap.cloud.tencent.com/coding/anthropic",
                "/coding/anthropic/v1/models",
            ),
            (
                "https://tokenhub.tencentmaas.com/plan/anthropic",
                "/plan/anthropic/v1/models",
            ),
            (
                "https://ark.cn-beijing.volces.com/api/coding",
                "/api/coding/v1/models",
            ),
        ] {
            let url = discovery_url(base, "anthropic").unwrap();
            assert_eq!(url.path(), path);
            assert_eq!(
                url.host_str(),
                reqwest::Url::parse(base).unwrap().host_str()
            );
            let request =
                discovery_auth(client.get(url.clone()), &url, "anthropic", Some("test-key"))
                    .build()
                    .unwrap();
            assert_eq!(request.headers()["authorization"], "Bearer test-key");
            assert!(!request.headers().contains_key("x-api-key"));
            let message_url = format!("{base}/v1/messages");
            let chat = super::super::anthropic::authenticate(
                client.post(&message_url),
                &message_url,
                "test-key",
            )
            .build()
            .unwrap();
            assert_eq!(chat.headers()["authorization"], "Bearer test-key");
            assert!(!chat.headers().contains_key("x-api-key"));
        }
        assert_eq!(
            discovery_url("https://api.longcat.chat/openai/v1", "openai")
                .unwrap()
                .as_str(),
            "https://api.longcat.chat/v1/models"
        );
        for url in [
            "https://api.anthropic.com/v1/messages",
            "https://api.longcat.chat.evil.test/anthropic/v1/messages",
            "https://evil.test/api.siliconflow.cn/v1/messages",
            "https://api.longcat.chat/custom/v1/messages",
            "https://api.minimax.cn/anthropic/v1/messages",
            "https://api.kimi.com/coding/v1/messages",
            "https://api.lkeap.cloud.tencent.com.evil.test/coding/anthropic/v1/messages",
            "https://tokenhub.tencentmaas.com/custom/plan/anthropic/v1/messages",
        ] {
            let request = super::super::anthropic::authenticate(client.post(url), url, "test-key")
                .build()
                .unwrap();
            assert_eq!(request.headers()["x-api-key"], "test-key");
            assert!(!request.headers().contains_key("authorization"));
        }
    }

    #[test]
    fn other_anthropic_endpoints_keep_their_path_and_authentication() {
        let client = reqwest::Client::new();
        for base in [
            "https://api.anthropic.com",
            "https://example.test/apps/anthropic",
            "https://dashscope.aliyuncs.com.example.test/apps/anthropic",
            "https://notmaas.aliyuncs.com/apps/anthropic",
            "https://dashscope.aliyuncs.com/custom/apps/anthropic",
        ] {
            let url = discovery_url(base, "anthropic").unwrap();
            assert_eq!(url.as_str(), format!("{base}/v1/models?limit=1000"));
            let request =
                discovery_auth(client.get(url.clone()), &url, "anthropic", Some("test-key"))
                    .build()
                    .unwrap();
            assert_eq!(request.headers()["x-api-key"], "test-key");
            assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
            assert!(!request.headers().contains_key("authorization"));
        }
        assert!(discovery_url(
            "https://user:secret@dashscope.aliyuncs.com/apps/anthropic",
            "anthropic"
        )
        .is_err());
    }

    #[tokio::test]
    async fn a_refused_ollama_connection_offers_startup_guidance_only_for_ollama() {
        // Use a recently released ephemeral port rather than an assumed service port.
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let url = discovery_url(&format!("http://{address}/proxy/gateway"), "ollama").unwrap();
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url.clone())
            .send()
            .await
            .unwrap_err();
        let diagnostic = ollama_connection_failure(&url, "ollama", &error, false).unwrap();
        assert!(
            matches!(&diagnostic, DiscoveryRequestError::OllamaUnavailable { local: true, endpoint } if endpoint == &format!("http://{address}/proxy/gateway"))
        );
        let message = diagnostic.ollama_guidance().unwrap();
        assert!(message.contains("ollama serve"));
        assert!(message.contains("11434"));
        assert!(message.contains("retry"));
        assert!(ollama_connection_failure(&url, "openai", &error, false).is_none());
        for host in ["localhost", "127.0.0.2", "[::1]"] {
            let target = discovery_url(&format!("http://{host}:11434"), "ollama").unwrap();
            assert!(matches!(
                ollama_connection_failure(&target, "ollama", &error, false),
                Some(DiscoveryRequestError::OllamaUnavailable { local: true, .. })
            ));
        }
        let remote = discovery_url("http://inference.example:1234", "ollama").unwrap();
        let diagnostic = ollama_connection_failure(&remote, "ollama", &error, false).unwrap();
        assert!(matches!(
            &diagnostic,
            DiscoveryRequestError::OllamaUnavailable { local: false, .. }
        ));
        assert!(diagnostic
            .ollama_guidance()
            .unwrap()
            .contains("target machine"));
        assert!(DiscoveryRequestError::UpstreamStatus(401)
            .ollama_guidance()
            .is_none());
    }

    #[test]
    fn proxy_policy_distinguishes_loopback_bypass_and_remote_routing() {
        let mut policy = atomcode_config::proxy::ProxyConfig {
            http: Some("http://127.0.0.1:9000".into()),
            no_proxy: Some("localhost,127.0.0.1,::1,.direct.example".into()),
            ..Default::default()
        };
        for endpoint in [
            "http://localhost:11434",
            "http://127.0.0.1:11434",
            "http://[::1]:11434",
            "http://host.direct.example",
        ] {
            assert!(!discovery_proxy_may_apply(
                &discovery_url(endpoint, "ollama").unwrap(),
                Some(&policy)
            ));
        }
        let remote = discovery_url("http://remote.example", "ollama").unwrap();
        assert!(discovery_proxy_may_apply(&remote, Some(&policy)));
        policy.no_proxy = Some("*".into());
        assert!(!discovery_proxy_may_apply(&remote, Some(&policy)));
    }

    #[tokio::test]
    async fn a_refused_proxy_is_not_an_ollama_startup_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all(format!("http://{address}")).unwrap())
            .build()
            .unwrap();
        for scheme in ["http", "https"] {
            let url =
                discovery_url(&format!("{scheme}://inference.example:11434"), "ollama").unwrap();
            let error = client.get(url.clone()).send().await.unwrap_err();
            let policy = atomcode_config::proxy::ProxyConfig {
                http: Some(format!("http://{address}")),
                https: Some(format!("http://{address}")),
                ..Default::default()
            };
            let proxied = discovery_proxy_may_apply(&url, Some(&policy));
            assert!(proxied);
            assert!(ollama_connection_failure(&url, "ollama", &error, proxied).is_none());
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn an_ollama_that_does_not_answer_offers_retry_guidance() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = discovery_url(
            &format!("http://{}", listener.local_addr().unwrap()),
            "ollama",
        )
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let target = url.clone();
        // Keep virtual time still while the OS establishes the local connection.
        let clock_guard = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let request = tokio::spawn(async move { client.get(target).send().await });
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        let error = request.await.unwrap().unwrap_err();
        clock_guard.abort();
        assert!(error.is_timeout());
        let diagnostic = ollama_connection_failure(&url, "ollama", &error, false).unwrap();
        assert!(diagnostic
            .ollama_guidance()
            .unwrap()
            .contains("ollama serve"));
    }

    #[test]
    fn a_rich_listing_declares_capabilities_without_model_name_heuristics() {
        let models = parse_discovered_models(
            "openai",
            br#"{"data":[
            {"id":"vendor/arbitrary-name","name":"Flash","context_window":1048576,
             "max_output_tokens":393216,"input_modalities":["text","image"],
             "effort":{"supported_levels":["low","high","max"],"default_level":"high"}},
            {"id":"text-only","input_modalities":["text"]},
            {"id":"unknown"}
        ]}"#,
        )
        .unwrap();
        let rich = models
            .iter()
            .find(|m| m.id == "vendor/arbitrary-name")
            .unwrap();
        assert_eq!(rich.context_window, Some(1048576));
        assert_eq!(rich.max_tokens, Some(393216));
        assert_eq!(rich.supports_vision, Some(true));
        assert_eq!(rich.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            rich.reasoning_effort_levels.as_deref(),
            Some(["low".into(), "high".into(), "max".into()].as_slice())
        );
        assert_eq!(
            models
                .iter()
                .find(|m| m.id == "text-only")
                .unwrap()
                .supports_vision,
            Some(false)
        );
        let unknown = models.iter().find(|m| m.id == "unknown").unwrap();
        assert_eq!(unknown.supports_vision, None);
        assert_eq!(unknown.reasoning_effort_levels, None);
    }

    #[test]
    fn unrecognised_optional_metadata_does_not_hide_the_model_list() {
        let models = parse_discovered_models(
            "openai",
            br#"{"data":[
            {"id":"a","input_modalities":"text","effort":"high"},
            {"id":"b","input_modalities":["image"],"effort":{"default_level":"high"}},
            {"id":"c","effort":{"supported_levels":["low"],"default_level":"low"}}
        ]}"#,
        )
        .unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].supports_vision, None);
        assert_eq!(models[0].reasoning_effort_levels, None);
        assert_eq!(models[1].supports_vision, Some(true));
        assert_eq!(models[1].reasoning_effort, None);
        assert_eq!(models[2].reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn an_invalid_default_is_not_advertised_as_a_supported_effort() {
        let models = parse_discovered_models(
            "openai",
            br#"{"data":[{"id":"x",
            "effort":{"supported_levels":["low"],"default_level":"max"}}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].reasoning_effort, None);
    }
}
