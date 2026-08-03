//! Best-effort models.dev pricing catalog.
//!
//! Pricing is metadata, never a provider-runtime dependency: download/cache failures
//! return no price and must not prevent an agent from starting or sending a request.

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

static REFRESHING: AtomicBool = AtomicBool::new(false);
static PARSED_CACHE: OnceLock<Mutex<Option<ParsedCache>>> = OnceLock::new();

struct RefreshGuard;

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        REFRESHING.store(false, Ordering::Release);
    }
}

struct ParsedCache {
    modified: Option<SystemTime>,
    len: u64,
    catalog: Catalog,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cached_input_per_million: f64,
}

#[derive(Debug, Deserialize)]
struct CatalogProvider {
    id: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: HashMap<String, CatalogModel>,
}

#[derive(Debug, Deserialize)]
struct CatalogModel {
    id: String,
    #[serde(default)]
    cost: Option<CatalogCost>,
    #[serde(default)]
    provider: Option<CatalogModelProvider>,
}

#[derive(Debug, Deserialize)]
struct CatalogModelProvider {
    #[serde(default)]
    api: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CatalogCost {
    input: f64,
    output: f64,
    #[serde(default)]
    cache_read: Option<f64>,
}

type Catalog = HashMap<String, CatalogProvider>;

fn cache_path() -> PathBuf {
    crate::paths::config_dir()
        .join("cache")
        .join("models-dev.json")
}

/// Populate the on-disk catalog when absent or older than 24 hours.
///
/// An existing stale cache remains usable while a failed refresh is ignored. The
/// first run waits at most three seconds, matching OpenCode's initial-populate
/// semantics with a tighter startup bound.
pub async fn ensure_models_dev_catalog() {
    let path = cache_path();
    if cache_is_fresh(&path, SystemTime::now()) {
        return;
    }
    if REFRESHING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let _refresh_guard = RefreshGuard;
    let builder = crate::proxy::apply_async_proxy_policy(reqwest::Client::builder())
        .connect_timeout(FETCH_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .user_agent("atomcode");
    let Ok(client) = builder.build() else {
        return;
    };
    let Ok(response) = client.get(MODELS_DEV_URL).send().await else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let Ok(bytes) = response.bytes().await else {
        return;
    };
    if serde_json::from_slice::<Catalog>(&bytes).is_err() {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(lock) = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
    else {
        return;
    };
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return;
    }
    // Another process may have completed the same refresh while this request
    // was in flight. Keep its fresh cache instead of replacing it again.
    if cache_is_fresh(&path, SystemTime::now()) {
        return;
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if fs::write(&temporary, &bytes).is_ok() {
        replace_cache_file(&temporary, &path);
    }
}

/// Refresh the optional pricing catalog without putting metadata I/O on a
/// caller's startup critical path.
///
/// The task is deliberately detached: callers continue using the existing
/// cache (including a stale cache) while this best-effort refresh runs. The
/// process-wide `REFRESHING` guard inside [`ensure_models_dev_catalog`] keeps
/// concurrent CLI/daemon startup paths from issuing duplicate requests.
pub fn spawn_models_dev_catalog_refresh() {
    tokio::spawn(async {
        ensure_models_dev_catalog().await;
    });
}

/// Resolve a price from the current disk cache.
///
/// Provider ID is preferred. If it does not identify a catalog provider, an
/// official API URL may identify exactly one provider. Arbitrary proxy URLs are
/// deliberately not inferred from the model name.
pub fn resolve_models_dev_pricing(
    provider_id: &str,
    base_url: &str,
    model_id: &str,
) -> Option<CatalogPricing> {
    let path = cache_path();
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .ok()?;
    fs2::FileExt::try_lock_shared(&lock).ok()?;
    let metadata = fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok();
    let len = metadata.len();
    let mut parsed = PARSED_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = parsed.as_ref() {
        if cached.modified == modified && cached.len == len {
            return resolve_from_catalog(&cached.catalog, provider_id, base_url, model_id);
        }
    }
    let bytes = fs::read(path).ok()?;
    let catalog: Catalog = serde_json::from_slice(&bytes).ok()?;
    let pricing = resolve_from_catalog(&catalog, provider_id, base_url, model_id);
    *parsed = Some(ParsedCache {
        modified,
        len,
        catalog,
    });
    pricing
}

fn resolve_from_catalog(
    catalog: &Catalog,
    provider_id: &str,
    base_url: &str,
    model_id: &str,
) -> Option<CatalogPricing> {
    let provider = if !base_url.trim().is_empty() {
        let requested = normalize_api_url(base_url)?;
        let mut matches = catalog.values().filter(|item| {
            let Some(model) = find_model(item, model_id) else {
                return false;
            };
            item.api.as_deref().and_then(normalize_api_url) == Some(requested.clone())
                || model
                    .provider
                    .as_ref()
                    .and_then(|provider| provider.api.as_deref())
                    .and_then(normalize_api_url)
                    == Some(requested.clone())
        });
        let first = matches.next()?;
        matches.next().is_none().then_some(first)?
    } else {
        catalog
            .get(provider_id)
            .or_else(|| catalog.values().find(|item| item.id == provider_id))?
    };
    let model = find_model(provider, model_id)?;
    let cost = model.cost.as_ref()?;
    let pricing = CatalogPricing {
        input_per_million: cost.input,
        output_per_million: cost.output,
        cached_input_per_million: cost.cache_read.unwrap_or(cost.input),
    };
    [
        pricing.input_per_million,
        pricing.output_per_million,
        pricing.cached_input_per_million,
    ]
    .into_iter()
    .all(|value| value.is_finite() && value >= 0.0)
    .then_some(pricing)
}

fn find_model<'a>(provider: &'a CatalogProvider, model_id: &str) -> Option<&'a CatalogModel> {
    provider
        .models
        .get(model_id)
        .or_else(|| provider.models.values().find(|item| item.id == model_id))
}

fn normalize_api_url(raw: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/v1").unwrap_or(path).to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    let mut normalized = url.to_string();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    Some(normalized)
}

fn replace_cache_file(temporary: &Path, destination: &Path) {
    if fs::rename(temporary, destination).is_ok() {
        return;
    }
    // Windows does not replace an existing destination with rename. Readers
    // hold the sibling lock file, so the remove+rename window is not observable
    // by another AtomCode process. Preserve the previous bytes if replacement
    // still fails.
    let previous = fs::read(destination).ok();
    if fs::remove_file(destination).is_ok() && fs::rename(temporary, destination).is_ok() {
        return;
    }
    if let Some(previous) = previous {
        let _ = fs::write(destination, previous);
    }
    let _ = fs::remove_file(temporary);
}

fn cache_is_fresh(path: &Path, now: SystemTime) -> bool {
    let fresh = fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age < CACHE_TTL);
    fresh
        && fs::read(path)
            .ok()
            .is_some_and(|bytes| serde_json::from_slice::<Catalog>(&bytes).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        serde_json::from_str(
            r#"{
              "deepseek": {
                "id": "deepseek",
                "api": "https://api.deepseek.com/v1",
                "models": {
                  "deepseek-chat": {
                    "id": "deepseek-chat",
                    "cost": {"input": 0.27, "output": 1.1, "cache_read": 0.07}
                  }
                }
              },
              "other": {
                "id": "other",
                "api": "https://other.example/v1",
                "models": {}
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_by_catalog_provider_id() {
        let price = resolve_from_catalog(&catalog(), "deepseek", "", "deepseek-chat").unwrap();
        assert_eq!(price.input_per_million, 0.27);
        assert_eq!(price.cached_input_per_million, 0.07);
    }

    #[test]
    fn arbitrary_provider_id_resolves_by_normalized_official_url() {
        let price = resolve_from_catalog(
            &catalog(),
            "my-provider",
            "https://API.DEEPSEEK.com/",
            "deepseek-chat",
        )
        .unwrap();
        assert_eq!(price.output_per_million, 1.1);
    }

    #[test]
    fn provider_id_cannot_override_a_custom_proxy_url() {
        assert!(resolve_from_catalog(
            &catalog(),
            "deepseek",
            "https://proxy.example/v1",
            "deepseek-chat"
        )
        .is_none());
    }

    #[test]
    fn unknown_proxy_and_unknown_model_do_not_guess() {
        assert!(resolve_from_catalog(
            &catalog(),
            "custom",
            "https://proxy.example/v1",
            "deepseek-chat"
        )
        .is_none());
        assert!(resolve_from_catalog(
            &catalog(),
            "deepseek",
            "https://api.deepseek.com",
            "not-a-model"
        )
        .is_none());
    }

    #[test]
    fn url_normalization_only_removes_trailing_v1() {
        assert_eq!(
            normalize_api_url("https://API.DeepSeek.com/v1/"),
            normalize_api_url("https://api.deepseek.com")
        );
        assert_ne!(
            normalize_api_url("https://proxy.example/deepseek/v1"),
            normalize_api_url("https://api.deepseek.com/v1")
        );
    }

    #[test]
    fn fresh_cache_requires_valid_catalog_and_expires_at_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"deepseek":{"id":"deepseek","models":{}}}"#).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert!(cache_is_fresh(&path, modified + Duration::from_secs(1)));
        assert!(!cache_is_fresh(
            &path,
            modified + CACHE_TTL + Duration::from_secs(1)
        ));

        std::fs::write(&path, b"{not-json").unwrap();
        assert!(!cache_is_fresh(&path, SystemTime::now()));
    }

    #[test]
    fn cache_replacement_preserves_valid_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("models.json");
        let temporary = dir.path().join("models.tmp");
        std::fs::write(&destination, b"old").unwrap();
        std::fs::write(&temporary, b"new").unwrap();

        replace_cache_file(&temporary, &destination);

        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        assert!(!temporary.exists());
    }
}
