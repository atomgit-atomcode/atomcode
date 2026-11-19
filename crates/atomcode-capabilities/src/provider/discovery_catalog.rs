//! Public model metadata. Provider credentials never cross this boundary.

use super::{discovery_client, read_bounded_response, DiscoveredModel, DiscoveryTransport};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const URL: &str = "https://models.opencode.ai/experimental/v2.0/api.json?type=chat";
const TIMEOUT: Duration = Duration::from_secs(4);
const LIMIT: usize = 32 * 1024 * 1024;
type Catalog = BTreeMap<String, Provider>;

#[derive(Deserialize)]
struct Provider {
    models: BTreeMap<String, Model>,
}

#[derive(Deserialize)]
struct Model {
    name: Option<String>,
    status: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    limit: Option<Limits>,
    modalities: Option<Modalities>,
    capabilities: Option<Capabilities>,
    #[serde(default)]
    api: BTreeMap<String, Route>,
}

#[derive(Deserialize)]
struct Route {
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct Limits {
    context: Option<usize>,
    output: Option<usize>,
}

#[derive(Deserialize)]
struct Modalities {
    input: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Capabilities {
    reasoning: Option<Reasoning>,
}

#[derive(Deserialize)]
struct Reasoning {
    effort: Option<Vec<String>>,
    default: Option<ReasoningDefault>,
}

#[derive(Deserialize)]
struct ReasoningDefault {
    #[serde(rename = "type")]
    kind: String,
    value: serde_json::Value,
}

#[derive(Default)]
struct Cache {
    catalog: Option<Arc<Catalog>>,
    retry_at: Option<Instant>,
}

async fn download() -> Option<Catalog> {
    let proxy = super::super::auto_proxy::for_request().await.ok()?;
    let client = discovery_client(
        &DiscoveryTransport::default(),
        TIMEOUT,
        true,
        proxy.as_ref(),
    )
    .ok()?;
    let response = client.get(URL).send().await.ok()?.error_for_status().ok()?;
    let bytes = read_bounded_response(response, LIMIT).await.ok()?;
    let catalog: Catalog = tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes))
        .await
        .ok()?
        .ok()?;
    (!catalog.is_empty()).then_some(catalog)
}

async fn load() -> Option<Arc<Catalog>> {
    static CACHE: tokio::sync::Mutex<Cache> = tokio::sync::Mutex::const_new(Cache {
        catalog: None,
        retry_at: None,
    });
    let mut cache = CACHE.lock().await;
    if cache.retry_at.is_none_or(|when| Instant::now() >= when) {
        let loaded = tokio::time::timeout(TIMEOUT, download())
            .await
            .ok()
            .flatten();
        let interval = if let Some(catalog) = loaded {
            cache.catalog = Some(Arc::new(catalog));
            Duration::from_secs(60 * 60)
        } else {
            Duration::from_secs(60)
        };
        cache.retry_at = Some(Instant::now() + interval);
    }
    cache.catalog.clone()
}

fn route(wire: &str) -> Option<&'static str> {
    match wire {
        "openai" | "openai-compat" | "openai_compat" => Some("chat-completions"),
        "responses" => Some("responses"),
        "anthropic" | "deepseek-messages" => Some("messages"),
        _ => None,
    }
}

fn endpoint(base: &str, route: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(base.trim()).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    let path = if route == "messages" || url.host_str() == Some("api.deepseek.com") {
        path.strip_suffix("/v1").unwrap_or(path)
    } else {
        path
    };
    url.set_path(&path.to_owned());
    Some(url)
}

fn candidates(catalog: &Catalog, base: &reqwest::Url, route: &str) -> Vec<DiscoveredModel> {
    let matching = |model: &Model| {
        model
            .api
            .get(route)
            .and_then(|r| r.base_url.as_deref())
            .and_then(|url| endpoint(url, route))
            .as_ref()
            == Some(base)
    };
    let mut providers = catalog
        .values()
        .filter(|provider| provider.models.values().any(matching));
    let Some(provider) = providers.next() else {
        return Vec::new();
    };
    // Two providers can use the same gateway. An ambiguous endpoint must not
    // silently fill limits from a different plan.
    if providers.next().is_some() {
        return Vec::new();
    }
    provider
        .models
        .iter()
        .filter(|(_, model)| {
            matching(model)
                && model.kind.as_deref().is_none_or(|kind| kind == "chat")
                && model.status.as_deref() != Some("deprecated")
        })
        .take(super::DISCOVERY_MAX_MODELS)
        .map(|(id, model)| {
            let reasoning = model
                .capabilities
                .as_ref()
                .and_then(|c| c.reasoning.as_ref());
            let levels = reasoning
                .and_then(|r| r.effort.as_ref())
                .map(|levels| {
                    atomcode_config::config::REASONING_EFFORT_LEVELS
                        .into_iter()
                        .filter(|level| levels.iter().any(|known| known == level))
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|levels| !levels.is_empty());
            let default = reasoning
                .and_then(|r| r.default.as_ref())
                .filter(|setting| setting.kind == "effort")
                .and_then(|setting| setting.value.as_str())
                .filter(|value| {
                    levels
                        .as_ref()
                        .is_some_and(|levels| levels.iter().any(|level| level == value))
                })
                .map(str::to_owned);
            DiscoveredModel {
                id: id.clone(),
                name: model.name.clone(),
                context_window: model
                    .limit
                    .as_ref()
                    .and_then(|limit| positive(limit.context)),
                max_tokens: model
                    .limit
                    .as_ref()
                    .and_then(|limit| positive(limit.output)),
                supports_vision: model
                    .modalities
                    .as_ref()
                    .and_then(|m| m.input.as_ref())
                    .map(|input| {
                        input
                            .iter()
                            .any(|m| m == "image" || m.starts_with("image/"))
                    }),
                reasoning_effort_levels: levels,
                reasoning_effort: None,
                catalog_default_effort: default,
                catalog_only: true,
            }
        })
        .collect()
}

fn positive(value: Option<usize>) -> Option<usize> {
    value.filter(|n| *n > 0 && *n <= u32::MAX as usize)
}

pub async fn models(base_url: &str, wire: &str) -> Vec<DiscoveredModel> {
    let Some(route) = route(wire) else {
        return Vec::new();
    };
    let Some(base) = endpoint(base_url, route) else {
        return Vec::new();
    };
    let Some(catalog) = load().await else {
        return Vec::new();
    };
    candidates(&catalog, &base, route)
}

pub fn supports_lookup(base_url: &str, wire: &str) -> bool {
    route(wire)
        .and_then(|wire| endpoint(base_url, wire))
        .is_some()
}

pub fn needs_metadata(model: &DiscoveredModel) -> bool {
    model.name.is_none()
        || model.context_window.is_none()
        || model.max_tokens.is_none()
        || model.supports_vision.is_none()
        || model.reasoning_effort_levels.is_none()
        || model.reasoning_effort.is_none()
}

pub fn fill_missing(model: &mut DiscoveredModel, catalog: &DiscoveredModel) {
    if model.id != catalog.id {
        return;
    }
    model.name = model.name.take().or_else(|| catalog.name.clone());
    model.context_window = model.context_window.or(catalog.context_window);
    model.max_tokens = model.max_tokens.or(catalog.max_tokens);
    model.supports_vision = model.supports_vision.or(catalog.supports_vision);
    if model.reasoning_effort_levels.is_none()
        && model.reasoning_effort.as_ref().is_none_or(|effort| {
            catalog
                .reasoning_effort_levels
                .as_ref()
                .is_some_and(|levels| levels.contains(effort))
        })
    {
        model.reasoning_effort_levels = catalog.reasoning_effort_levels.clone();
    }
    if model.reasoning_effort.is_none() && model.catalog_default_effort.is_none() {
        model.catalog_default_effort = catalog.catalog_default_effort.clone().filter(|effort| {
            model
                .reasoning_effort_levels
                .as_ref()
                .is_some_and(|levels| levels.contains(effort))
        });
    }
}

pub fn merge_listing<E>(
    result: Result<Vec<DiscoveredModel>, E>,
    catalog: Vec<DiscoveredModel>,
) -> Result<Vec<DiscoveredModel>, E> {
    if catalog.is_empty() {
        return result;
    }
    match result {
        Ok(mut models) if !models.is_empty() => {
            let indexed: BTreeMap<_, _> = catalog
                .iter()
                .map(|model| (model.id.as_str(), model))
                .collect();
            for model in &mut models {
                if let Some(entry) = indexed.get(model.id.as_str()) {
                    fill_missing(model, entry);
                }
            }
            Ok(models)
        }
        _ => Ok(catalog),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plan_route_limits_catalog_metadata_to_the_exact_model_and_protocol() {
        let data = json!({
            "alibaba-token-plan-cn": {"models": {
                "qwen-demo": {
                    "name": "Qwen Demo",
                    "limit": {"context": 1000000, "output": 131072},
                    "modalities": {"input": ["text", "image/jpeg"]},
                    "capabilities": {"reasoning": {
                        "effort": ["high", "xhigh", "future"],
                        "default": {"type": "effort", "value": "high"}
                    }},
                    "api": {"messages": {"base_url": "https://plan.example/apps/anthropic/v1"}}
                },
                "other-wire": {"api": {"chat-completions": {"base_url": "https://plan.example/v1"}}},
                "deprecated": {
                    "status": "deprecated",
                    "api": {"messages": {"base_url": "https://plan.example/apps/anthropic/v1"}}
                }
            }},
            "alibaba-standard": {"models": {
                "qwen-demo": {
                    "limit": {"context": 4000},
                    "api": {"messages": {"base_url": "https://standard.example/v1"}}
                }
            }}
        });
        let catalog: Catalog = serde_json::from_value(data).unwrap();
        let base = endpoint("https://plan.example/apps/anthropic", "messages").unwrap();
        let entries = candidates(&catalog, &base, "messages");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].context_window, Some(1_000_000));
        assert_eq!(entries[0].supports_vision, Some(true));
        assert_eq!(
            entries[0].reasoning_effort_levels.as_deref(),
            Some(&["high".to_string(), "xhigh".to_string()][..])
        );
        let verified = DiscoveredModel {
            id: "qwen-demo".into(),
            context_window: Some(128_000),
            supports_vision: Some(false),
            ..Default::default()
        };
        let other = DiscoveredModel {
            id: "new-not-in-catalog".into(),
            ..Default::default()
        };
        let live = merge_listing::<()>(Ok(vec![verified, other.clone()]), entries.clone()).unwrap();
        assert_eq!(live[0].context_window, Some(128_000));
        assert_eq!(live[0].supports_vision, Some(false));
        assert_eq!(live[0].max_tokens, Some(131_072));
        assert_eq!(live[0].catalog_default_effort.as_deref(), Some("high"));
        assert_eq!(live[0].reasoning_effort, None);
        let mut disabled = DiscoveredModel {
            id: "qwen-demo".into(),
            reasoning_effort_levels: Some(vec![]),
            ..Default::default()
        };
        fill_missing(&mut disabled, &entries[0]);
        assert_eq!(disabled.reasoning_effort_levels, Some(vec![]));
        assert_eq!(disabled.catalog_default_effort, None);
        assert_eq!(disabled.reasoning_effort, None);
        assert!(!live[0].catalog_only);
        assert_eq!(live[1], other);
        assert!(merge_listing(Err(()), entries.clone()).unwrap()[0].catalog_only);
        assert!(merge_listing::<()>(Ok(vec![]), entries).unwrap()[0].catalog_only);
        assert_eq!(
            merge_listing(Err("unavailable"), vec![]),
            Err("unavailable")
        );
    }

    #[test]
    fn an_ambiguous_gateway_does_not_supply_another_plans_limits() {
        let record = json!({"models": {"demo": {"api": {
            "chat-completions": {"base_url": "https://shared.example/v1"}
        }}}});
        let catalog: Catalog =
            serde_json::from_value(json!({"plan-a": record, "plan-b": record})).unwrap();
        let shared = endpoint("https://shared.example/v1", "chat-completions").unwrap();
        assert!(candidates(&catalog, &shared, "chat-completions").is_empty());
        assert!(!supports_lookup("http://localhost:11434", "ollama"));
        assert!(!supports_lookup(
            "https://user:secret@example.com",
            "openai"
        ));
    }
}
