//! 插件市场：从**独立的插件仓库**读市场清单，解析成客户端可直接用的条目。
//!
//! 仓库基址、LFS 媒体基址与缓存 TTL 都来自 `config.toml` 的 `[market]` 段
//! （环境变量可覆盖，测试与 adb 调试都用得上）。清单里每个条目的
//! `artifact.kind` 决定包体怎么取：
//!
//! | kind | 取法 |
//! |---|---|
//! | `repo-plain` | 仓库基址 + 相对路径（普通文件，raw 链接可直接下） |
//! | `repo-lfs` | 媒体基址 + 相对路径（LFS 的 raw 链接只返回指针文本） |
//! | `url` | 用条目自带的绝对地址 |
//!
//! 所以把包体从 Git LFS 换到 Release 附件或对象存储，只需改仓库里的元数据，
//! 客户端不必升级——这正是「弹性适配」的落点。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use tokio::sync::Mutex;

/// 清单多久算过期。仓库更新是低频操作，十分钟足够，也不至于让人等。
const DEFAULT_TTL: Duration = Duration::from_secs(600);

/// 市场下载地址可以用环境变量覆盖：测试、以及 adb 里临时指向本地目录。
const BASE_ENV: &str = "ATOMCODE_MARKET_BASE";
/// LFS 的媒体基址单独一个变量：raw 链接对 LFS 文件只返回指针文本。
const MEDIA_ENV: &str = "ATOMCODE_MARKET_MEDIA_BASE";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Base {
    Http(String),
    Local(PathBuf),
}

impl Base {
    fn parse(raw: &str) -> Self {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            Base::Http(raw.trim_end_matches('/').to_string())
        } else {
            let trimmed = raw.trim();
            let path = trimmed.strip_prefix("file://").unwrap_or(trimmed);
            Base::Local(PathBuf::from(path))
        }
    }

    fn describe(&self) -> String {
        match self {
            Base::Http(url) => url.clone(),
            Base::Local(path) => path.display().to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct Settings {
    base: Base,
    /// LFS 包体的媒体基址；缺省时退回 `base`（普通文件仓库就是这样）。
    media: Option<Base>,
    ttl: Duration,
}

impl Settings {
    fn from_env() -> Option<Self> {
        let base = std::env::var(BASE_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(config_market_base)?;
        let media = std::env::var(MEDIA_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| Base::parse(&value))
            .or_else(|| {
                config_market_key("media_base")
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| Base::parse(&value))
            });
        Some(Self {
            base: Base::parse(&base),
            media,
            ttl: DEFAULT_TTL,
        })
    }
}

fn config_market_base() -> Option<String> {
    config_market_key("base")
}

/// `config.toml` 的 `[market] <key>`。读失败一律当作没配置：市场是可选能力，
/// 不能因为配置文件缺失就让包管理页打不开。
fn config_market_key(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(atomcode_config::config::Config::default_path()).ok()?;
    // `toml::from_str` 才是“解析整份文档”；`Value::from_str`（`text.parse()`）在
    // toml 1.x 里按单个值解析，`[market]` 会被当成意外内容直接失败。
    let doc: toml::Value = toml::from_str(&text).ok()?;
    let value = doc.get("market")?.get(key)?.as_str()?.to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MarketDoc {
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    storage: String,
    #[serde(default)]
    generated_at: String,
    #[serde(default)]
    base: String,
    #[serde(default)]
    categories: Vec<MarketCategory>,
    #[serde(default)]
    plugins: Vec<MarketPlugin>,
    /// The optional helper APK, published next to the plugins.
    ///
    /// It is **not** a plugin: it is a separate app carrying the binaries that
    /// have to `execve` children (make runs a compiler), which Android forbids
    /// inside our own data directory. Publishing it is what lets the panel offer
    /// a download instead of leaving the person with an `adb install` line.
    ///
    /// Kept for repositories published before there was more than one such app;
    /// [`MarketDoc::providers`] folds it into the same list.
    #[serde(default)]
    helper: Option<MarketHelper>,
    /// Every optional provider APK the repository publishes.
    ///
    /// The helper above carries `make`/`tcc`/the CLI, while zig needs 141 MB of its
    /// own and also comes from another package — and so will Go, clang and the rest:
    /// a **process-model** tool cannot live in a market data package at all
    /// (`execve` from our data directory is refused, measured as exit 126). One APK
    /// per runtime is the delivery route, so the manifest needs a list, not a slot.
    #[serde(default)]
    helpers: Vec<MarketHelper>,
}

impl MarketDoc {
    /// Provider APKs in publication order.
    ///
    /// `helpers` wins when it is present; a repository that still uses the single
    /// `helper` key yields that one entry, so an older market keeps working without
    /// being re-published.
    fn providers(&self) -> Vec<&MarketHelper> {
        if !self.helpers.is_empty() {
            self.helpers.iter().collect()
        } else {
            self.helper.iter().collect()
        }
    }

    /// One provider by name (its own id or its Android package id).
    fn provider(&self, wanted: &str) -> Option<&MarketHelper> {
        let wanted = wanted.trim();
        if wanted.is_empty() {
            return self.providers().into_iter().next();
        }
        self.providers()
            .into_iter()
            .find(|helper| helper.name == wanted || helper.package == wanted)
    }
}

/// The helper APK's own metadata, published by the plugin repository.
#[derive(Debug, Clone, Deserialize)]
struct MarketHelper {
    name: String,
    /// The Android package id of the APK (`com.atomcode.tools.zig`).
    ///
    /// The panel needs it to say *which* provider is installed, and the app uses it
    /// for the `<queries>` lookup. Empty in older manifests, where the name and the
    /// package id were the same thing.
    #[serde(default)]
    package: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    artifact: Artifact,
}

impl MarketHelper {
    /// The Android package id, falling back to the id for old manifests.
    fn package_id(&self) -> &str {
        if self.package.is_empty() {
            &self.name
        } else {
            &self.package
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MarketCategory {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    blurb: String,
    #[serde(default)]
    order: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct MarketPlugin {
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    version: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    index: String,
    #[serde(default)]
    artifact: Artifact,
    /// 全部历史版本，最新的排在最后；老仓库没有这一项时为空。
    #[serde(default)]
    versions: Vec<MarketVersion>,
}

/// 单插件元数据里的一条历史版本。市场清单把它带出来，客户端才能装回旧版
/// （daemon 的 `rollback` 也才有东西可回）。
#[derive(Debug, Clone, Deserialize)]
struct MarketVersion {
    version: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    artifact: Artifact,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Artifact {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    sha256_file: String,
}

/// 一个条目解析后的下载位置：HTTP 给 URL，本地目录给文件路径。
#[derive(Debug, Default, PartialEq, Eq)]
struct Target {
    url: Option<String>,
    path: Option<String>,
    sha256_url: Option<String>,
    sha256_path: Option<String>,
}

fn join_http(base: &str, relative: &str) -> String {
    if relative.is_empty() {
        return base.to_string();
    }
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        relative.trim_start_matches('/')
    )
}

/// 把 `artifact` 解析成可下载的位置。
///
/// 只做“拼接与选择”，不访问网络——纯函数，测试里可以直接断言。
fn resolve(settings: &Settings, artifact: &Artifact) -> Target {
    let kind = if artifact.kind.is_empty() {
        "repo-plain"
    } else {
        artifact.kind.as_str()
    };

    let base = match kind {
        "url" => {
            return Target {
                url: (!artifact.url.is_empty()).then(|| artifact.url.clone()),
                ..Target::default()
            }
        }
        // LFS 的文件必须走媒体基址；没配置媒体基址时退回仓库基址，
        // 客户端下载后会因 sha256 不符而明确失败，比静默装错安全。
        "repo-lfs" => settings.media.as_ref().unwrap_or(&settings.base),
        _ => &settings.base,
    };

    let mut target = Target::default();
    match base {
        Base::Http(prefix) => {
            target.url = Some(join_http(prefix, &artifact.path));
            if !artifact.sha256_file.is_empty() {
                target.sha256_url = Some(join_http(prefix, &artifact.sha256_file));
            }
        }
        Base::Local(dir) => {
            // 本地基址可以是目录，也可以直接指向 market.json。
            let root = if dir.extension().is_some_and(|ext| ext == "json") {
                dir.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                dir.clone()
            };
            if !artifact.path.is_empty() {
                target.path = Some(root.join(&artifact.path).display().to_string());
            }
            if !artifact.sha256_file.is_empty() {
                target.sha256_path = Some(root.join(&artifact.sha256_file).display().to_string());
            }
        }
    }
    target
}

async fn fetch_document(settings: &Settings) -> Result<String, String> {
    match &settings.base {
        Base::Http(prefix) => {
            let url = join_http(prefix, "market.json");
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|error| format!("cannot build HTTP client: {error}"))?;
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|error| format!("cannot fetch {url}: {error}"))?;
            if !response.status().is_success() {
                return Err(format!("{url} returned {}", response.status()));
            }
            response
                .text()
                .await
                .map_err(|error| format!("cannot read {url}: {error}"))
        }
        Base::Local(dir) => {
            let file = if dir.extension().is_some_and(|ext| ext == "json") {
                dir.clone()
            } else {
                dir.join("market.json")
            };
            std::fs::read_to_string(&file)
                .map_err(|error| format!("cannot read {}: {error}", file.display()))
        }
    }
}

struct Cached {
    raw: String,
    doc: MarketDoc,
    fetched_at: Instant,
    base: String,
}

static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();

fn cache_slot() -> &'static Mutex<Option<Cached>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 缓存文件：拉不到网络时还能给出上一次的清单，总比空白页强。
fn cache_file() -> PathBuf {
    atomcode_coding::config::product_dirs_from_env()
        .user()
        .join("cache")
        .join("market.json")
}

fn write_cache(raw: &str) {
    let path = cache_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, raw);
}

fn read_cache() -> Option<String> {
    std::fs::read_to_string(cache_file()).ok()
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct MarketQuery {
    #[serde(default)]
    refresh: Option<String>,
}

fn wants_refresh(value: &Option<String>) -> bool {
    matches!(value.as_deref(), Some("1") | Some("true") | Some("yes"))
}

/// Load the market document, from the cache when it is still fresh.
///
/// Returns the parsed document, the raw text (for the cache file) and whether
/// the answer came from the cache. A network failure falls back to the last
/// good copy; only a missing cache is an error.
async fn load_document(
    settings: &Settings,
    refresh: bool,
) -> Result<(MarketDoc, String, bool), String> {
    let base_repr = settings.base.describe();
    let mut slot = cache_slot().lock().await;

    let fresh = slot
        .as_ref()
        .filter(|cached| {
            !refresh && cached.base == base_repr && cached.fetched_at.elapsed() < settings.ttl
        })
        .map(|cached| (cached.doc.clone(), cached.raw.clone(), true));

    match fresh {
        Some(found) => Ok(found),
        None => match fetch_document(settings).await {
            Ok(raw) => {
                let doc: MarketDoc = serde_json::from_str(&raw)
                    .map_err(|error| format!("market.json 解析失败：{error}"))?;
                write_cache(&raw);
                *slot = Some(Cached {
                    raw: raw.clone(),
                    doc: doc.clone(),
                    fetched_at: Instant::now(),
                    base: base_repr,
                });
                Ok((doc, raw, false))
            }
            Err(error) => match read_cache() {
                Some(raw) => match serde_json::from_str::<MarketDoc>(&raw) {
                    Ok(doc) => Ok((doc, raw, true)),
                    // The cached copy is unreadable too: report why the fetch
                    // failed, which is the actionable half.
                    Err(_) => Err(error),
                },
                None => Err(error),
            },
        },
    }
}

/// GET /packages/market —— 分类 + 插件条目，前端据此渲染侧栏与分页。
pub(crate) async fn market_index(Query(query): Query<MarketQuery>) -> Response {
    let Some(settings) = Settings::from_env() else {
        // 没配置市场源不是错误：包管理页照常可用，只是没有“市场”这一段。
        return Json(serde_json::json!({
            "success": false,
            "error": "market not configured",
            "hint": format!(
                "在 {} 的 [market] base 里填插件仓库地址，或设置环境变量 {}",
                atomcode_config::config::Config::default_path().display(),
                BASE_ENV
            ),
        }))
        .into_response();
    };

    let refresh = wants_refresh(&query.refresh);
    let base_repr = settings.base.describe();

    let (doc, raw, from_cache) = match load_document(&settings, refresh).await {
        Ok(found) => found,
        Err(error) => {
            return Json(serde_json::json!({
                "success": false,
                "error": error,
                "base": base_repr,
            }))
            .into_response()
        }
    };

    let installed: std::collections::BTreeMap<String, String> = crate::packages::scan()
        .unwrap_or_default()
        .into_iter()
        .map(|record| (record.name, record.version))
        .collect();

    let mut categories = doc.categories.clone();
    categories.sort_by(|left, right| {
        left.order
            .cmp(&right.order)
            .then_with(|| left.id.cmp(&right.id))
    });

    let plugins: Vec<_> = doc
        .plugins
        .iter()
        .map(|plugin| {
            let target = resolve(&settings, &plugin.artifact);
            let versions: Vec<_> = plugin
                .versions
                .iter()
                .map(|entry| {
                    let resolved = resolve(&settings, &entry.artifact);
                    serde_json::json!({
                        "version": entry.version,
                        "size_bytes": entry.size_bytes,
                        "sha256": entry.sha256,
                        "download_url": resolved.url,
                        "download_path": resolved.path,
                    })
                })
                .collect();
            let installed_version = installed.get(&plugin.name).cloned();
            serde_json::json!({
                "name": plugin.name,
                "title": plugin.title,
                "summary": plugin.summary,
                "category": plugin.category,
                "tags": plugin.tags,
                "version": plugin.version,
                "size_bytes": plugin.size_bytes,
                "sha256": plugin.sha256,
                "index": plugin.index,
                "artifact_kind": if plugin.artifact.kind.is_empty() { "repo-plain".to_string() } else { plugin.artifact.kind.clone() },
                "download_url": target.url,
                "download_path": target.path,
                "sha256_url": target.sha256_url,
                "sha256_path": target.sha256_path,
                "versions": versions,
                "installed": installed_version.is_some(),
                "installed_version": installed_version,
            })
        })
        .collect();

    let categories: Vec<_> = categories
        .into_iter()
        .map(|category| {
            serde_json::json!({
                "id": category.id,
                "title": category.title,
                "blurb": category.blurb,
            })
        })
        .collect();

    // The provider APKs ride along with the plugins: same base, same storage, so
    // one download path serves both.
    let helpers: Vec<_> = doc
        .providers()
        .into_iter()
        .map(|helper| {
            let target = resolve(&settings, &helper.artifact);
            serde_json::json!({
                "name": helper.name,
                "package": helper.package_id(),
                "title": helper.title,
                "summary": helper.summary,
                "version": helper.version,
                "size_bytes": helper.size_bytes,
                "sha256": helper.sha256,
                "artifact_kind": if helper.artifact.kind.is_empty() { "repo-plain".to_string() } else { helper.artifact.kind.clone() },
                "download_url": target.url,
                "download_path": target.path,
                "sha256_url": target.sha256_url,
                "sha256_path": target.sha256_path,
            })
        })
        .collect();
    // `helper` stays for clients built before there could be more than one provider;
    // it is the first entry, which for those repositories is the only one.
    let helper = helpers.first().cloned();

    let _ = raw; // 原始文本只用于写缓存
    Json(serde_json::json!({
        "success": true,
        "base": base_repr,
        "storage": doc.storage,
        "schema": doc.schema,
        "generated_at": doc.generated_at,
        "declared_base": doc.base,
        "from_cache": from_cache,
        "categories": categories,
        "plugins": plugins,
        "helper": helper,
        "helpers": helpers,
    }))
    .into_response()
}

/// 在 TOML 文本里设置 `[market] base`，**其余内容逐字保留**。
///
/// 没有现成的配置合并库可用（`toml` 的 writer 会重排并丢掉注释），而 config.toml 是
/// 人写的、注释有意义，所以只做最小手术：定位 `[market]` 段，替换或插入一行。
fn with_market_base(text: &str, base: &str) -> String {
    let escaped = base.replace('\\', "\\\\").replace('"', "\\\"");
    let assignment = format!("base = \"{escaped}\"");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let mut header: Option<usize> = None;
    let mut section_is_market = false;
    for (index, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section_is_market = trimmed == "[market]";
            if section_is_market {
                header = Some(index);
            }
        }
    }

    match header {
        Some(start) => {
            let mut end = lines.len();
            for (offset, raw) in lines.iter().enumerate().skip(start + 1) {
                if raw.trim().starts_with('[') {
                    end = offset;
                    break;
                }
            }
            let existing = (start + 1..end).find(|&index| {
                let trimmed = lines[index].trim_start();
                trimmed.starts_with("base") && trimmed["base".len()..].trim_start().starts_with('=')
            });
            match existing {
                Some(index) => lines[index] = assignment,
                None => lines.insert(start + 1, assignment),
            }
        }
        None => {
            // 没有 `[market]` 段：补一个。前面留一个空行，读起来和手写的一样。
            if lines.last().is_some_and(|line| !line.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[market]".to_string());
            lines.push(assignment);
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// 原子写：先写临时文件再改名，避免半截配置让下一次启动读不动。
fn write_atomically(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&temporary, path)
}

#[derive(Debug, Deserialize)]
pub(crate) struct MarketBaseBody {
    #[serde(default)]
    base: String,
}

/// GET /packages/market/helper?name=<provider> —— a provider APK's bytes.
///
/// The panel needs the file itself, not a link it can open: the APK has to be
/// handed to the system installer, which reads it from a file this app owns.
/// So the daemon fetches it (from the same base the plugins come from), checks
/// the hash the repository published, and writes it into the product directory
/// where the app can find it.
///
/// `name` picks the provider (its manifest id or its Android package id); without
/// it the first published provider is used, which is what a client built when there
/// was only one provider asks for.
///
/// The path it returns is inside the app's own storage, so it never leaves the
/// sandbox by accident — the install is still the user's decision.
pub(crate) async fn market_helper_download(Query(query): Query<HelperQuery>) -> Response {
    let Some(settings) = Settings::from_env() else {
        return Json(serde_json::json!({
            "success": false,
            "error": "market not configured",
        }))
        .into_response();
    };

    let (doc, _, _) = match load_document(&settings, false).await {
        Ok(found) => found,
        Err(error) => {
            return Json(serde_json::json!({ "success": false, "error": error })).into_response()
        }
    };

    let Some(helper) = doc.provider(&query.name) else {
        let error = if query.name.trim().is_empty() {
            "the market does not publish a helper app".to_string()
        } else {
            format!(
                "the market does not publish a provider called `{}`",
                query.name.trim()
            )
        };
        return Json(serde_json::json!({ "success": false, "error": error })).into_response();
    };

    let target = resolve(&settings, &helper.artifact);
    let bytes = match fetch_target(&target).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return Json(serde_json::json!({ "success": false, "error": error })).into_response()
        }
    };

    // The repository published a hash; a mismatch means the file is not the one
    // the index describes, and installing it would be installing something
    // nobody reviewed.
    if !helper.sha256.is_empty() {
        if let Err(error) = verify_sha256(&bytes, &helper.sha256) {
            return Json(serde_json::json!({ "success": false, "error": error })).into_response();
        }
    }

    let path = match write_helper_apk(&helper.name, &bytes) {
        Ok(path) => path,
        Err(error) => {
            return Json(serde_json::json!({ "success": false, "error": error })).into_response()
        }
    };

    Json(serde_json::json!({
        "success": true,
        "name": helper.name,
        "package": helper.package_id(),
        "version": helper.version,
        "path": path,
        "size_bytes": bytes.len(),
    }))
    .into_response()
}

/// Which provider to fetch; empty means "the first one the market publishes".
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HelperQuery {
    #[serde(default)]
    name: String,
}

/// Fetch a market artifact from wherever it lives.
async fn fetch_target(target: &Target) -> Result<Vec<u8>, String> {
    if let Some(path) = target.path.as_deref() {
        return std::fs::read(path).map_err(|error| format!("cannot read {path}: {error}"));
    }
    let Some(url) = target.url.as_deref() else {
        return Err("the market entry has no download location".to_string());
    };
    let response = reqwest::get(url)
        .await
        .map_err(|error| format!("download failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("download failed: HTTP {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| format!("download failed: {error}"))
}

/// Write the helper APK into the product directory and return its path.
fn write_helper_apk(name: &str, bytes: &[u8]) -> Result<String, String> {
    let dir = atomcode_coding::config::product_dirs_from_env()
        .user()
        .join("helper");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    // The name is a package id, not a person-supplied string: it comes from the
    // market document. Keep it to a safe file name anyway.
    let file = name.replace(['/', '\\'], "_");
    let path = dir.join(format!("{file}.apk"));
    std::fs::write(&path, bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path.display().to_string())
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let expected = expected.trim().to_ascii_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = format!("{:x}", hasher.finalize());
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "sha256 mismatch: expected {expected}, got {actual}"
        ))
    }
}

/// POST /packages/market/config —— 把市场源写进 `config.toml` 的 `[market] base`。
///
/// 为什么是写文件而不是只存内存：这正是重启后还想生效的东西，而且设备上没有别的
/// 稳定配置面。空字符串表示清掉（回到未配置状态）。
pub(crate) async fn set_market_base(Json(body): Json<MarketBaseBody>) -> Response {
    let base = body.base.trim().to_string();
    let path = atomcode_config::config::Config::default_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = with_market_base(&text, &base);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = write_atomically(&path, &updated) {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("cannot write {}: {error}", path.display()),
        }))
        .into_response();
    }
    // 下一次请求必须去新地址取清单，不能拿旧缓存。
    *cache_slot().lock().await = None;
    Json(serde_json::json!({ "success": true, "base": base })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A repository published before there could be more than one provider APK still
    // says `[helper]`; it must keep working, and it must look like a one-item list.
    #[test]
    fn a_single_helper_becomes_a_one_item_provider_list() {
        let doc: MarketDoc = toml::from_str(
            "[helper]
name = \"com.atomcode.tools\"
",
        )
        .unwrap();
        let providers = doc.providers();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "com.atomcode.tools");
        // Old manifests had no `package` key: the name *was* the package id.
        assert_eq!(providers[0].package_id(), "com.atomcode.tools");
    }

    // The list is what the panel shows: one row per provider APK (tools, zig, …).
    #[test]
    fn a_provider_list_is_read_in_publication_order() {
        let doc: MarketDoc = toml::from_str(
            "[[helpers]]
name = \"tools\"
package = \"com.atomcode.tools\"

             [[helpers]]
name = \"zig\"
package = \"com.atomcode.tools.zig\"
",
        )
        .unwrap();
        let names: Vec<&str> = doc.providers().iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["tools", "zig"]);
    }

    // Lookup has to work by both ids: the panel knows the manifest name, while the
    // install state comes from the Android package id.
    #[test]
    fn a_provider_is_found_by_its_name_or_its_package_id() {
        let doc: MarketDoc = toml::from_str(
            "[[helpers]]
name = \"tools\"
package = \"com.atomcode.tools\"

             [[helpers]]
name = \"zig\"
package = \"com.atomcode.tools.zig\"
",
        )
        .unwrap();
        assert_eq!(
            doc.provider("zig").unwrap().package_id(),
            "com.atomcode.tools.zig"
        );
        assert_eq!(doc.provider("com.atomcode.tools.zig").unwrap().name, "zig");
        // An empty name is the old client's request: the first provider.
        assert_eq!(doc.provider("").unwrap().name, "tools");
        assert!(doc.provider("nope").is_none());
    }

    #[test]
    fn a_missing_market_section_is_appended() {
        let updated = with_market_base("[telemetry]\nenabled = false\n", "https://x.test/p");
        assert!(updated.contains("[market]"));
        assert!(updated.contains("base = \"https://x.test/p\""));
        // 原有内容逐字保留，注释与顺序都不动。
        assert!(updated.starts_with("[telemetry]\nenabled = false\n"));
        assert!(updated.ends_with('\n'));
    }

    #[test]
    fn an_existing_base_is_replaced_in_place() {
        let text = "[market]\nbase = \"/old\"\nmedia_base = \"/media\"\n";
        let updated = with_market_base(text, "/new");
        assert!(updated.contains("base = \"/new\""));
        assert!(!updated.contains("/old"));
        // 同一段里的其它键不动。
        assert!(updated.contains("media_base = \"/media\""));
    }

    #[test]
    fn a_market_section_without_a_base_gets_one() {
        let text = "[market]\nmedia_base = \"/media\"\n\n[other]\nx = 1\n";
        let updated = with_market_base(text, "/new");
        let market = updated.find("[market]").expect("[market]");
        let base = updated.find("base = \"/new\"").expect("base line");
        let other = updated.find("[other]").expect("[other]");
        // 插在 [market] 之后、别的段之前。
        assert!(market < base && base < other);
        assert!(updated.contains("media_base = \"/media\""));
        assert!(updated.contains("[other]\nx = 1"));
    }

    #[test]
    fn quotes_and_backslashes_in_the_base_are_escaped() {
        // TOML 基本字符串：反斜杠与引号必须转义，否则写出来的配置读不回去。
        let updated = with_market_base("", "C:\\repo\\\"x");
        assert!(updated.contains(r#"base = "C:\\repo\\\"x""#), "{updated}");
        // 写出来的东西必须能被 TOML 解析回来。
        let parsed: toml::Value = toml::from_str(&updated).expect("valid toml");
        assert_eq!(parsed["market"]["base"].as_str(), Some("C:\\repo\\\"x"));
    }

    #[test]
    fn an_empty_base_clears_the_value() {
        let updated = with_market_base("[market]\nbase = \"/old\"\n", "");
        assert!(updated.contains("base = \"\""));
    }

    fn settings_http() -> Settings {
        Settings {
            base: Base::parse("https://example.test/plugins"),
            media: Some(Base::parse("https://media.example.test/plugins")),
            ttl: DEFAULT_TTL,
        }
    }

    fn artifact(kind: &str) -> Artifact {
        Artifact {
            kind: kind.to_string(),
            path: "packages/numpy-1.26.2-arm64.tar.gz".to_string(),
            url: String::new(),
            sha256_file: "packages/numpy-1.26.2-arm64.tar.gz.sha256".to_string(),
        }
    }

    #[test]
    fn repo_plain_joins_the_repository_base() {
        let target = resolve(&settings_http(), &artifact("repo-plain"));
        assert_eq!(
            target.url.as_deref(),
            Some("https://example.test/plugins/packages/numpy-1.26.2-arm64.tar.gz")
        );
        assert_eq!(target.path, None);
    }

    #[test]
    fn repo_lfs_prefers_the_media_base() {
        // LFS 的 raw 链接只返回指针文本，所以必须走媒体基址。
        let target = resolve(&settings_http(), &artifact("repo-lfs"));
        assert_eq!(
            target.url.as_deref(),
            Some("https://media.example.test/plugins/packages/numpy-1.26.2-arm64.tar.gz")
        );
        assert_eq!(
            target.sha256_url.as_deref(),
            Some("https://media.example.test/plugins/packages/numpy-1.26.2-arm64.tar.gz.sha256")
        );
    }

    #[test]
    fn repo_lfs_without_media_falls_back_to_the_base() {
        let mut settings = settings_http();
        settings.media = None;
        let target = resolve(&settings, &artifact("repo-lfs"));
        assert_eq!(
            target.url.as_deref(),
            Some("https://example.test/plugins/packages/numpy-1.26.2-arm64.tar.gz")
        );
    }

    #[test]
    fn explicit_url_wins_over_the_base() {
        let mut spec = artifact("url");
        spec.url = "https://objects.example.test/numpy.tar.gz".to_string();
        let target = resolve(&settings_http(), &spec);
        assert_eq!(
            target.url.as_deref(),
            Some("https://objects.example.test/numpy.tar.gz")
        );
    }

    #[test]
    fn a_local_base_resolves_to_file_paths() {
        // 设备上没有网络时，把基址指向本地目录也要能用（验证与离线演示）。
        let settings = Settings {
            base: Base::parse("/sdcard/Download/atomcode-plugins"),
            media: None,
            ttl: DEFAULT_TTL,
        };
        let target = resolve(&settings, &artifact("repo-plain"));
        // 用 PathBuf 比较：Windows 与 Android 的分隔符不同，断言的是位置不是字形。
        let expected = Path::new("/sdcard/Download/atomcode-plugins")
            .join("packages/numpy-1.26.2-arm64.tar.gz");
        assert_eq!(target.path.map(std::path::PathBuf::from), Some(expected));
        assert_eq!(target.url, None);
    }

    #[test]
    fn a_local_base_pointing_at_market_json_uses_its_directory() {
        let settings = Settings {
            base: Base::parse("/tmp/market/market.json"),
            media: None,
            ttl: DEFAULT_TTL,
        };
        let target = resolve(&settings, &artifact("repo-plain"));
        let expected = Path::new("/tmp/market").join("packages/numpy-1.26.2-arm64.tar.gz");
        assert_eq!(target.path.map(std::path::PathBuf::from), Some(expected));
    }

    #[test]
    fn the_market_document_tolerates_unknown_fields() {
        // 仓库以后加字段不该让老客户端整页失败。
        let raw = r#"{
            "schema": 1,
            "storage": "repo-lfs",
            "generated_at": "2026-10-27T00:00:00Z",
            "base": "",
            "brand_new_field": {"whatever": true},
            "categories": [{"id": "python", "title": "Python", "blurb": "解释器", "order": 10}],
            "plugins": [{
                "name": "numpy",
                "title": "NumPy 1.26.2",
                "summary": "科学计算",
                "category": "python",
                "tags": ["scientific"],
                "version": "1.26.2",
                "size_bytes": 9852245,
                "sha256": "ec1da8d0",
                "artifact": {"kind": "repo-lfs", "path": "packages/numpy-1.26.2-arm64.tar.gz"},
                "future_field": "ignored"
            }]
        }"#;
        let doc: MarketDoc = serde_json::from_str(raw).expect("tolerant parse");
        assert_eq!(doc.categories.len(), 1);
        assert_eq!(doc.plugins[0].name, "numpy");
        assert_eq!(doc.plugins[0].artifact.kind, "repo-lfs");
        assert_eq!(doc.plugins[0].size_bytes, 9_852_245);
    }

    #[tokio::test]
    async fn http_fetch_reads_market_json_from_the_base() {
        // 仓库推送之后走的就是这条路：基址是 raw/媒体 链接，客户端 GET
        // `<base>/market.json`。起一个本地服务把它跑通，别等到上机才发现。
        use axum::routing::get;

        let body = r#"{
            "schema": 1,
            "storage": "repo-plain",
            "categories": [{"id": "python", "title": "Python", "order": 10}],
            "plugins": [{
                "name": "numpy",
                "category": "python",
                "version": "1.26.2",
                "artifact": {"kind": "repo-plain", "path": "packages/numpy-1.26.2-arm64.tar.gz"}
            }]
        }"#;
        let app = axum::Router::new().route(
            "/market.json",
            get({
                let body = body.to_string();
                move || async move { body }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let settings = Settings {
            base: Base::Http(format!("http://{address}")),
            media: None,
            ttl: DEFAULT_TTL,
        };
        let raw = fetch_document(&settings).await.expect("应能取到清单");
        let doc: MarketDoc = serde_json::from_str(&raw).expect("清单应可解析");
        assert_eq!(doc.storage, "repo-plain");
        assert_eq!(doc.plugins[0].name, "numpy");

        let target = resolve(&settings, &doc.plugins[0].artifact);
        let expected = format!("http://{address}/packages/numpy-1.26.2-arm64.tar.gz");
        assert_eq!(target.url.as_deref(), Some(expected.as_str()));
    }

    /// 环境变量是进程全局的，测试之间要串行并复原。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn settings_read_the_base_from_config_toml() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 目录名只留数字：Windows 路径不接受 `SystemTime` 的 Debug 输出（带冒号）。
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("atomcode-market-{}-{}", std::process::id(), stamp));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[market]
base = \"/sdcard/Download/atomcode-plugins\"
",
        )
        .unwrap();

        let previous = std::env::var_os("ATOMCODE_HOME");
        std::env::set_var("ATOMCODE_HOME", &dir);
        std::env::remove_var(BASE_ENV);
        std::env::remove_var(MEDIA_ENV);

        // 分步断言：先看路径解析，再看 TOML 解析，失败时能直接看出是哪一环。
        let path = atomcode_config::config::Config::default_path();
        assert!(path.exists(), "config 路径不存在：{path:?}");
        let text = std::fs::read_to_string(&path).expect("config 读不到");
        let doc: toml::Value = toml::from_str(&text).expect("config 不是合法 TOML");
        assert!(
            doc.get("market")
                .and_then(|value| value.get("base"))
                .is_some(),
            "[market] base 没读到：{doc:?}"
        );

        let settings = Settings::from_env().expect("market 配置应被读到");
        assert_eq!(
            settings.base,
            Base::Local(PathBuf::from("/sdcard/Download/atomcode-plugins"))
        );
        assert_eq!(settings.media, None);

        match previous {
            Some(value) => std::env::set_var("ATOMCODE_HOME", value),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_only_accepts_explicit_truthy_values() {
        assert!(wants_refresh(&Some("1".to_string())));
        assert!(wants_refresh(&Some("true".to_string())));
        assert!(!wants_refresh(&Some("0".to_string())));
        assert!(!wants_refresh(&None));
    }
}
