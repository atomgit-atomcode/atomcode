//! Runtime packages: installable toolchains that are NOT shipped in the APK.
//!
//! Android forbids `execve` on files in an app's data directory (targetSdk
//! 29+), so a package can never be a plain executable. What the platform DOES
//! allow is `dlopen` on a `.so` in the data directory — measured on-device, see
//! `docs/android-port.md` §P5.7. So a package is a **shared library plus its
//! data**, and a small launcher shipped in `nativeLibraryDir` (`pluginhost`)
//! `dlopen`s it and calls its entry point.
//!
//! This module owns only the files on disk: fetch, verify, unpack, list,
//! remove. It never loads code — loading belongs to the launcher, which runs
//! in its own process so a crashing interpreter cannot take the daemon down.
//!
//! # Layout
//!
//! ```text
//! <product>/packages/<name>/
//!   package.json        # manifest (see [`Manifest`])
//!   lib/<entry>.so      # the library `pluginhost` dlopens
//!   ...                 # payload the plugin needs (stdlib, data files)
//! ```
//!
//! # Bundle format
//!
//! A package is uploaded/fetched as a `.tar.gz` whose root holds `package.json`.
//! One file to fetch and one hash to verify beats a directory walk.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use axum::extract::Path as UrlPath;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::{json_error, AppState};

/// The manifest every bundle must carry at its root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Package id. Also the directory name, so it is validated as one.
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Path to the shared library, relative to the package root.
    ///
    /// Empty means a **data-only** package: it carries files (C headers, examples,
    /// docs) and owns no command name, so nothing tries to run it.
    #[serde(default)]
    pub entry: String,
    /// The symbol `pluginhost` calls. Defaults to `plugin_main`.
    #[serde(default = "default_symbol")]
    pub symbol: String,
    /// Extra command names that run this package, besides its own name. Each
    /// becomes a symlink in `<product>/bin` pointing at `pluginhost`, so
    /// `cpython script.py` works like any other command.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Product-level directories this package uses **outside** its own tree —
    /// `python-packages`, `.local`, `node_modules`. Recorded at install so that
    /// after the package is removed the same directories can be reported as
    /// leftovers and reclaimed, instead of quietly occupying disk space.
    #[serde(default)]
    pub data_dirs: Vec<String>,
    /// Directory inside the package whose children are exposed to Python —
    /// a wheel's payload (`numpy/`, `numpy-1.26.dist-info/`). Each child is
    /// symlinked into `<product>/python-packages`, the directory the runtime
    /// already puts on `PYTHONPATH`, so `import numpy` works without copying
    /// 30 MB and uninstalling takes the links away again.
    #[serde(default)]
    pub python_packages: String,
    /// 产品目录里的链接：`{"zig": "lib"}` 表示把 `<product>/zig` 指向包内的 `lib/`。
    ///
    /// 数据包经常需要一个**固定路径**：zig 找 `$ZIG_LIB_DIR`、字体引擎找字库目录。
    /// 链接和 python_packages 一样登记所有者，卸载时精确移除，不留死链接。
    #[serde(default)]
    pub links: std::collections::BTreeMap<String, String>,
}

fn default_symbol() -> String {
    "plugin_main".to_string()
}

/// A package as the API reports it — never the payload bytes.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PackageRecord {
    pub name: String,
    pub version: String,
    pub description: String,
    pub entry: String,
    pub symbol: String,
    /// Command names this package answers to (its own name plus aliases).
    pub commands: Vec<String>,
    /// The version kept beside this one for `rollback`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    /// Size of the installed tree, so a person can see what a package costs.
    pub size_bytes: u64,
}

/// One event on the install progress stream.
///
/// A 47 MB bundle takes long enough that a silent button is indistinguishable
/// from a hang, so the install reports where it is. `stage` is the coarse step;
/// `received`/`total` track the bytes moved; `files` counts unpacked entries.
#[derive(Debug, Clone, Serialize)]
pub struct WireProgress {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageRecord>,
}

impl WireProgress {
    fn stage(stage: &'static str) -> Self {
        Self {
            kind: "progress",
            stage: Some(stage),
            received: None,
            total: None,
            files: None,
            message: None,
            package: None,
        }
    }

    fn transfer(stage: &'static str, received: u64, total: Option<u64>) -> Self {
        Self {
            kind: "progress",
            stage: Some(stage),
            received: Some(received),
            total,
            files: None,
            message: None,
            package: None,
        }
    }

    fn unpacked(files: u64, consumed: u64, total: u64) -> Self {
        Self {
            kind: "progress",
            stage: Some("unpack"),
            received: Some(consumed),
            total: Some(total),
            files: Some(files),
            message: None,
            package: None,
        }
    }

    fn done(package: PackageRecord) -> Self {
        Self {
            kind: "done",
            stage: None,
            received: None,
            total: None,
            files: None,
            message: None,
            package: Some(package),
        }
    }

    fn error(message: String) -> Self {
        Self {
            kind: "error",
            stage: None,
            received: None,
            total: None,
            files: None,
            message: Some(message),
            package: None,
        }
    }
}

/// Coarse install steps, so the unpack path can report without knowing about
/// the wire format.
#[derive(Debug, Clone)]
pub enum InstallStage {
    /// Files extracted so far. `consumed`/`total` are compressed bundle bytes,
    /// which is what makes the bar determinate: file counts alone only ever
    /// answer "is it still moving", and a 24 MB runtime is thousands of them.
    Unpacking {
        files: u64,
        consumed: u64,
        total: u64,
    },
    /// Writing the command links.
    Linking,
}

#[derive(Debug, Deserialize)]
pub struct InstallRequest {
    /// `http(s)://…` bundle to download.
    #[serde(default)]
    pub url: Option<String>,
    /// Local bundle path — the sideload/dev route (no network needed).
    #[serde(default)]
    pub path: Option<String>,
    /// Expected SHA-256 of the bundle (hex). Verified when present.
    #[serde(default)]
    pub sha256: Option<String>,
}

/// A bundle larger than this is refused instead of filling the disk.
const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// GET /packages
pub(crate) async fn list_packages() -> impl IntoResponse {
    match scan() {
        Ok(packages) => {
            Json(serde_json::json!({ "success": true, "packages": packages })).into_response()
        }
        Err(error) => json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

/// POST /packages/install
pub(crate) async fn install_package(
    State(_state): State<AppState>,
    Json(req): Json<InstallRequest>,
) -> impl IntoResponse {
    let bytes = match read_bundle(&req).await {
        Ok(bytes) => bytes,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error).into_response(),
    };
    if let Some(expected) = req.sha256.as_deref() {
        if let Err(error) = verify_sha256(&bytes, expected) {
            return json_error(StatusCode::BAD_REQUEST, error).into_response();
        }
    }
    match install_bytes(&bytes) {
        Ok(record) => {
            Json(serde_json::json!({ "success": true, "package": record })).into_response()
        }
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// POST /packages/install/stream — the same install, with progress events.
///
/// A 47 MB bundle over a phone link takes long enough that a spinner is
/// indistinguishable from a hang; this variant reports the bytes moved and the
/// unpack count, then ends with exactly one `done` or `error`.
pub(crate) async fn install_package_stream(
    Json(req): Json<InstallRequest>,
) -> axum::response::Response {
    use futures::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WireProgress>();
    tokio::spawn(install_streaming(req, tx));
    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|event| {
        let json = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(axum::response::sse::Event::default().data(json))
    });
    axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

/// Command names the optional helper APK (`com.atomcode.tools`) contributes.
///
/// Kept in step with `atomcode_android::shims::HELPER_TOOLS`. The daemon cannot
/// depend on the Android crate, so the list is repeated here — a wire contract
/// between the two, not a coincidence.
///
/// `zig` is in the list because the helper ships its binary: without it here the
/// panel would report the helper as missing `zig` even though `zig` is on the
/// device and working. `atomcode` is the CLI itself, which a person runs as
/// `atomcode daemon` on the device (see `docs/external-daemon.md`).
const HELPER_COMMANDS: &[&str] = &["make", "cc", "tcc", "atomcode"];

/// GET /packages/helper — whether the optional helper APK is installed.
///
/// Detected from `<product>/bin`: the host writes those links **only** when the
/// helper is present (`shims::link_helper_tools` removes them otherwise), so the
/// links themselves are the state. This is what lets the panel say where `make`
/// and a C compiler come from instead of leaving them unexplained.
pub(crate) async fn helper_status() -> impl IntoResponse {
    let bin = bin_dir();
    let tools: Vec<&str> = HELPER_COMMANDS
        .iter()
        .copied()
        .filter(|name| bin.join(name).exists())
        .collect();
    Json(serde_json::json!({
        "success": true,
        "installed": !tools.is_empty(),
        "tools": tools,
    }))
    .into_response()
}

/// Product-level directories a package declares it uses (manifest `data_dirs`).
///
/// They live outside `packages/<name>` on purpose: `python-packages` holds the
/// libraries a person installed, `.local` holds the interpreter's user site, and
/// both must survive a runtime update. The cost is that removing the package
/// does not remove them, so ownership has to be remembered somewhere: the
/// declaration is recorded at install time, and a directory whose owner is no
/// longer installed is *reported* rather than silently left behind (measured on
/// device: 21.9 MB still on disk after uninstalling cpython).
const DATA_DIRS_REGISTRY: &str = ".data-dirs.json";

fn registry_path() -> std::path::PathBuf {
    packages_root().join(DATA_DIRS_REGISTRY)
}

fn read_data_dirs_registry() -> std::collections::BTreeMap<String, Vec<String>> {
    let Ok(text) = std::fs::read_to_string(registry_path()) else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_data_dirs_registry(
    registry: &std::collections::BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(registry)
        .map_err(|error| format!("cannot encode data-dir registry: {error}"))?;
    std::fs::write(&path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Remember which directories this package owns outside its own tree.
///
/// Best effort: a registry that cannot be written must not fail the install, it
/// only makes the leftovers invisible later.
fn record_data_dirs(manifest: &Manifest) {
    if manifest.data_dirs.is_empty() {
        return;
    }
    let mut registry = read_data_dirs_registry();
    if registry.get(&manifest.name) == Some(&manifest.data_dirs) {
        return;
    }
    registry.insert(manifest.name.clone(), manifest.data_dirs.clone());
    let _ = write_data_dirs_registry(&registry);
}

fn directory_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total += directory_size(&entry.path());
        } else {
            total += metadata.len();
        }
    }
    total
}

/// Names of the packages currently installed under `packages/`.
fn installed_names() -> std::collections::BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(packages_root()) else {
        return std::collections::BTreeSet::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| read_manifest(&entry.path()).ok())
        .map(|manifest| manifest.name)
        .collect()
}

/// A declared data directory whose owning package is gone.
struct Leftover {
    /// The package that created it — the thing a person needs to understand it.
    package: String,
    name: String,
    path: std::path::PathBuf,
    bytes: u64,
}

/// Declared data directories that no installed package owns any more.
///
/// While the owner is installed the same directories are live, so they are not
/// leftovers and cost nothing to report.
fn runtime_leftovers() -> Vec<Leftover> {
    let installed = installed_names();
    let product = packages_root()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut found = Vec::new();
    for (package, directories) in read_data_dirs_registry() {
        if installed.contains(&package) {
            continue;
        }
        for name in directories {
            let path = product.join(&name);
            if !path.is_dir() {
                continue;
            }
            let bytes = directory_size(&path);
            found.push(Leftover {
                package: package.clone(),
                name,
                path,
                bytes,
            });
        }
    }
    found
}

/// GET /packages/leftovers — runtime data still on disk after removal.
pub(crate) async fn list_leftovers() -> impl IntoResponse {
    let leftovers = runtime_leftovers();
    let total: u64 = leftovers.iter().map(|leftover| leftover.bytes).sum();
    let entries: Vec<_> = leftovers
        .into_iter()
        .map(|leftover| {
            serde_json::json!({
                "package": leftover.package,
                "name": leftover.name,
                "path": leftover.path.display().to_string(),
                "bytes": leftover.bytes,
            })
        })
        .collect();
    Json(serde_json::json!({
        "success": true,
        "leftovers": entries,
        "total_bytes": total,
    }))
    .into_response()
}

/// DELETE /packages/leftovers — reclaim the space an uninstalled runtime left.
pub(crate) async fn clean_leftovers() -> impl IntoResponse {
    let leftovers = runtime_leftovers();
    if leftovers.is_empty() {
        return Json(serde_json::json!({ "success": true, "freed_bytes": 0 })).into_response();
    }
    let freed: u64 = leftovers.iter().map(|leftover| leftover.bytes).sum();
    let mut cleaned = std::collections::BTreeSet::new();
    for leftover in leftovers {
        if let Err(error) = std::fs::remove_dir_all(&leftover.path) {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot remove `{}`: {error}", leftover.name),
            )
            .into_response();
        }
        cleaned.insert(leftover.package);
    }
    // Forget the owners we just cleaned, so the report does not keep naming a
    // directory that is gone.
    let mut registry = read_data_dirs_registry();
    for package in cleaned {
        registry.remove(&package);
    }
    let _ = write_data_dirs_registry(&registry);
    Json(serde_json::json!({ "success": true, "freed_bytes": freed })).into_response()
}

/// GET /packages/health — probe every installed package the way a real run
/// would, so the panel can show "broken, and why" without the person having to
/// run it in a shell to find out.
pub(crate) async fn packages_health() -> impl IntoResponse {
    let packages = match scan() {
        Ok(packages) => packages,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };
    let mut report = Vec::with_capacity(packages.len());
    for record in packages {
        // Nothing to run means nothing to probe; "ok" because there is no
        // failure mode to report.
        if record.entry.is_empty() {
            report.push(HealthEntry {
                name: record.name,
                ok: true,
                detail: "data-only package".to_string(),
            });
            continue;
        }
        let name = record.name.clone();
        // Loading a shared object can take a moment (CPython is 25 MB) and a
        // pathological one could hang, so probe off the runtime and cap the wait.
        let probed = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::task::spawn_blocking(move || probe_package(&name)),
        )
        .await;
        let (ok, detail) = match probed {
            Ok(Ok(Ok(detail))) => (true, detail),
            Ok(Ok(Err(detail))) => (false, detail),
            Ok(Err(error)) => (false, format!("probe task failed: {error}")),
            Err(_) => (false, "probe timed out".to_string()),
        };
        report.push(HealthEntry {
            name: record.name,
            ok,
            detail,
        });
    }
    Json(serde_json::json!({ "success": true, "packages": report })).into_response()
}

#[derive(Debug, Serialize)]
struct HealthEntry {
    name: String,
    ok: bool,
    /// What the launcher said: the version when healthy, the loader's complaint
    /// when not.
    detail: String,
}

/// Run `pluginhost --check <name>` and turn its answer into a report.
///
/// Going through the launcher rather than loading the library in-process is
/// deliberate: the question is "would a real run work", and the launcher is
/// what a real run is. It also keeps a broken plugin from taking the daemon
/// down while we are merely inspecting it.
fn probe_package(name: &str) -> Result<String, String> {
    let launcher = bin_dir().join("pluginhost");
    if !launcher.exists() {
        return Err("pluginhost is not installed in this build".to_string());
    }
    let output = std::process::Command::new(&launcher)
        .arg("--check")
        .arg(name)
        .output()
        .map_err(|error| format!("cannot run pluginhost: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        // `ok <name> <version> <entry>` — the version is the interesting part.
        Ok(stdout
            .split_whitespace()
            .nth(2)
            .map(str::to_string)
            .unwrap_or(stdout))
    } else if stderr.is_empty() {
        Err(format!("pluginhost exited with {}", output.status))
    } else {
        Err(stderr)
    }
}

/// Swap the installed version with the one kept beside it.
///
/// An upgrade can be fine to install and wrong to run — a missing library, a
/// regression in the interpreter itself. The previous generation is kept for
/// exactly this, so the fix does not require finding the old bundle again.
///
/// The swap itself keeps the *current* version as the new previous, so a
/// rollback is itself undoable by rolling back again.
pub fn rollback(name: &str) -> Result<PackageRecord, String> {
    if !is_package_name(name) {
        return Err(format!("invalid package name: {name:?}"));
    }
    let root = packages_root();
    let current = root.join(name);
    let previous = root.join(format!(".previous-{name}"));
    if !current.is_dir() {
        return Err(format!("`{name}` is not installed"));
    }
    if !previous.is_dir() {
        return Err(format!("`{name}` has no previous version to roll back to"));
    }
    let current_manifest = read_manifest(&current)?;
    let previous_manifest = read_manifest(&previous)?;

    // Swap through a third name so a failure at any step can be undone.
    let swap = root.join(format!(".swapping-{}", uuid::Uuid::new_v4()));
    std::fs::rename(&current, &swap)
        .map_err(|error| format!("cannot move {} aside: {error}", current.display()))?;
    if let Err(error) = std::fs::rename(&previous, &current) {
        let _ = std::fs::rename(&swap, &current);
        return Err(format!("cannot restore the previous version: {error}"));
    }
    if let Err(error) = std::fs::rename(&swap, &previous) {
        // Undo the first half: nothing has moved as far as the caller can tell.
        let _ = std::fs::rename(&current, &previous);
        let _ = std::fs::rename(&swap, &current);
        return Err(format!("cannot keep the current version: {error}"));
    }

    // Versions may claim different command names, so drop the outgoing set
    // before writing the incoming one.
    remove_alias_links(&current_manifest);
    if let Err(error) = install_alias_links(&previous_manifest) {
        // The disks are already swapped; put them back so the package keeps the
        // command names it had before this call.
        let _ = std::fs::rename(&current, &swap);
        let _ = std::fs::rename(&previous, &current);
        let _ = std::fs::rename(&swap, &previous);
        let _ = install_alias_links(&current_manifest);
        return Err(error);
    }

    record_for(&current, &previous_manifest)
}

/// POST /packages/:name/rollback
pub(crate) async fn rollback_package(UrlPath(name): UrlPath<String>) -> impl IntoResponse {
    // Swapping directories and rewriting links is quick but blocking; keep it
    // off the async runtime.
    match tokio::task::spawn_blocking(move || rollback(&name)).await {
        Ok(Ok(record)) => {
            Json(serde_json::json!({ "success": true, "package": record })).into_response()
        }
        Ok(Err(error)) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("rollback task failed: {error}"),
        )
        .into_response(),
    }
}

/// DELETE /packages/:name
pub(crate) async fn remove_package(UrlPath(name): UrlPath<String>) -> impl IntoResponse {
    match remove(&name) {
        Ok(removed) => {
            Json(serde_json::json!({ "success": true, "removed": removed })).into_response()
        }
        Err(error) => json_error(StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// Where installed packages live: `<product>/packages`.
pub fn packages_root() -> PathBuf {
    atomcode_coding::config::product_dirs_from_env()
        .user()
        .join("packages")
}

async fn read_bundle(req: &InstallRequest) -> Result<Vec<u8>, String> {
    if let Some(path) = req.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot read bundle {path}: {error}"))?;
        if metadata.len() > MAX_BUNDLE_BYTES {
            return Err(format!(
                "bundle is {} bytes, over the {MAX_BUNDLE_BYTES} byte limit",
                metadata.len()
            ));
        }
        return std::fs::read(path).map_err(|error| format!("cannot read bundle {path}: {error}"));
    }
    let Some(url) = req.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
        return Err("either `url` or `path` is required".to_string());
    };
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("bundle URL must be http(s): {url}"));
    }
    let response = reqwest::get(url)
        .await
        .map_err(|error| format!("download failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("download failed: HTTP {}", response.status()));
    }
    if let Some(length) = response.content_length() {
        if length > MAX_BUNDLE_BYTES {
            return Err(format!(
                "bundle is {length} bytes, over the {MAX_BUNDLE_BYTES} byte limit"
            ));
        }
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("download failed: {error}"))?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err("bundle over the size limit".to_string());
    }
    Ok(bytes.to_vec())
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

/// Install a bundle that is already in memory. Split out so tests can drive the
/// whole unpack/validate/move path without a network.
pub fn install_bytes(bundle: &[u8]) -> Result<PackageRecord, String> {
    install_bytes_with(bundle, &mut |_| true)
}

/// [`install_bytes`], reporting coarse progress for the streaming endpoint.
///
/// The callback returns `false` to abort — the streaming client closing its
/// connection is a cancel, and the work must stop there rather than finish an
/// install nobody is waiting for. An aborted install leaves no staging
/// directory behind: the error path below removes it.
pub fn install_bytes_with(
    bundle: &[u8],
    on_stage: &mut dyn FnMut(InstallStage) -> bool,
) -> Result<PackageRecord, String> {
    let root = packages_root();
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;

    // Unpack into a staging directory first: a bundle that fails validation
    // must leave no half-installed package behind.
    let staging = root.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&staging)
        .map_err(|error| format!("cannot create staging dir: {error}"))?;

    let unpacked = unpack_tar_gz(bundle, &staging, on_stage).and_then(|()| {
        let manifest = read_manifest(&staging)?;
        validate_manifest(&manifest)?;
        // A data-only package declares no entry, so there is no file to check.
        if !manifest.entry.is_empty() {
            validate_entry(&staging, &manifest.entry)?;
        }
        // Check the command names before anything is moved into place: a
        // package whose alias is already taken must not install at all.
        check_aliases(&manifest)?;
        Ok(manifest)
    });

    let manifest = match unpacked {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    let destination = root.join(&manifest.name);
    // Replace-in-place, but keep ONE generation: see `rollback`. An upgrade
    // that turns out to be broken must be undoable without finding the old
    // bundle again.
    let previous = root.join(format!(".previous-{}", manifest.name));
    let had_previous = destination.exists();
    let outgoing = if had_previous {
        read_manifest(&destination).ok()
    } else {
        None
    };
    if had_previous {
        // One step deep is the contract, so an older previous is dropped now.
        let _ = std::fs::remove_dir_all(&previous);
        if let Err(error) = std::fs::rename(&destination, &previous) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!("cannot replace {}: {error}", destination.display()));
        }
    }
    if let Err(error) = std::fs::rename(&staging, &destination) {
        // Put the old one back so a failed upgrade is not a failed uninstall.
        if had_previous {
            let _ = std::fs::rename(&previous, &destination);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "cannot install into {}: {error}",
            destination.display()
        ));
    }

    // Now make it callable. If the links cannot be written the package is
    // useless, so drop it rather than leave an installed thing nobody can run.
    on_stage(InstallStage::Linking);
    // A new version may have dropped a command name. Its link still points at
    // `pluginhost` but the package no longer claims it, which would make the
    // command fail with "no package provides …" — take it away.
    if let Some(outgoing) = &outgoing {
        let claimed = command_names(&manifest);
        for name in command_names(outgoing) {
            if !claimed.contains(&name) {
                remove_alias_link(&name, &manifest.name);
            }
        }
    }
    if let Err(error) = install_alias_links(&manifest) {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(error);
    }
    if let Err(error) = install_python_links(&manifest, &destination) {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(error);
    }
    if let Err(error) = install_data_links(&manifest, &destination) {
        remove_python_links(&manifest);
        let _ = std::fs::remove_dir_all(&destination);
        return Err(error);
    }
    // Remember what this package claims outside its own tree *before* the
    // install is reported done, so an uninstall later can still name it.
    record_data_dirs(&manifest);
    // Packages that ship native libraries (numpy) must be importable straight
    // after install, without restarting the app.
    refresh_native_library_path();
    record_for(&destination, &manifest)
}

/// Where a payload's children get linked: the directory the CPython runtime
/// already puts on `PYTHONPATH` (`<product>/python-packages`).
fn python_packages_dir() -> std::path::PathBuf {
    packages_root()
        .parent()
        .map(|product| product.join("python-packages"))
        .unwrap_or_else(|| std::path::PathBuf::from("python-packages"))
}

/// Link name → owning package, so an uninstall removes exactly its own links.
const PYTHON_LINKS_REGISTRY: &str = ".python-links.json";

fn python_links_path() -> std::path::PathBuf {
    packages_root().join(PYTHON_LINKS_REGISTRY)
}

fn read_python_links() -> std::collections::BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(python_links_path()) else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_python_links(registry: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    let path = python_links_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(registry)
        .map_err(|error| format!("cannot encode python link registry: {error}"))?;
    std::fs::write(&path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Expose a wheel payload to Python without duplicating it.
///
/// Copying a 30 MB numpy tree would double the disk cost and, worse, leave a
/// full copy behind after removal — exactly the dead-file problem the leftovers
/// report exists to avoid. Symlinks keep one copy and make the uninstall exact,
/// because each link is recorded with its owner.
fn install_python_links(manifest: &Manifest, package_dir: &Path) -> Result<(), String> {
    if manifest.python_packages.is_empty() {
        return Ok(());
    }
    let source = package_dir.join(&manifest.python_packages);
    if !source.is_dir() {
        return Err(format!(
            "python_packages directory is missing in the bundle: {}",
            manifest.python_packages
        ));
    }
    let target_root = python_packages_dir();
    std::fs::create_dir_all(&target_root)
        .map_err(|error| format!("cannot create {}: {error}", target_root.display()))?;
    let mut registry = read_python_links();
    let mut created: Vec<String> = Vec::new();

    let drop_created = |created: &[String], root: &Path| {
        for name in created {
            let _ = std::fs::remove_file(root.join(name));
        }
    };

    let entries = std::fs::read_dir(&source)
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }
        if let Some(owner) = registry.get(&name) {
            if owner != &manifest.name {
                drop_created(&created, &target_root);
                return Err(format!("`{name}` is already provided by package `{owner}`"));
            }
        }
        let link = target_root.join(&name);
        // A real file or directory that no package claims: refuse rather than
        // overwrite something the person put there.
        if registry.get(&name).is_none() && link.symlink_metadata().is_ok() {
            drop_created(&created, &target_root);
            return Err(format!("`{name}` already exists in python-packages"));
        }
        let _ = std::fs::remove_file(&link);
        if let Err(error) = write_symlink(&entry.path(), &link) {
            drop_created(&created, &target_root);
            return Err(format!("cannot link `{name}`: {error}"));
        }
        registry.insert(name.clone(), manifest.name.clone());
        created.push(name);
    }
    write_python_links(&registry)
}

/// Directories inside installed packages that hold native libraries.
///
/// numpy's extension modules link against `libopenblas.so` / `libc++_shared.so`,
/// which the wheel carries under `lib/python/.libs` — next to nothing the loader
/// searches by default, so `import numpy` failed with "library libopenblas.so
/// not found" even though the file was installed and the package was listed as
/// present. The CPython shim cannot fix this alone: the libraries belong to a
/// *different* package (numpy, not cpython), and nobody numbers them in the
/// shim's preload list.
///
/// The loader does honour `LD_LIBRARY_PATH` for app-private paths (measured on
/// device), so the directories are collected there whenever packages change and
/// once at start-up.
pub fn refresh_native_library_path() {
    let mut dirs: Vec<String> = Vec::new();
    // The installer keeps `<product>/python-packages/.libs` pointing at the
    // package that provides it; a plain `.libs` next to the interpreter's
    // packages covers the single-provider case without a scan.
    let shared = python_packages_dir().join(".libs");
    if shared.is_dir() {
        dirs.push(shared.display().to_string());
    }
    if let Ok(entries) = std::fs::read_dir(packages_root()) {
        for entry in entries.flatten() {
            let libs = entry.path().join("lib").join("python").join(".libs");
            if libs.is_dir() {
                dirs.push(libs.display().to_string());
            }
        }
    }
    if dirs.is_empty() {
        return;
    }
    // Keep whatever the host already set (the Android shim exports nothing here
    // today, but a packaged toolchain may).
    if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
        let existing = existing.to_string_lossy().to_string();
        if !existing.is_empty() {
            dirs.push(existing);
        }
    }
    dirs.dedup();
    std::env::set_var("LD_LIBRARY_PATH", dirs.join(":"));
}

/// Remove the links this package created, and forget them.
fn remove_python_links(manifest: &Manifest) {
    let mut registry = read_python_links();
    let mine: Vec<String> = registry
        .iter()
        .filter(|(_, owner)| *owner == &manifest.name)
        .map(|(name, _)| name.clone())
        .collect();
    if mine.is_empty() {
        return;
    }
    let root = python_packages_dir();
    for name in &mine {
        let _ = std::fs::remove_file(root.join(name));
    }
    for name in mine {
        registry.remove(&name);
    }
    let _ = write_python_links(&registry);
}

/// Emit at most once per this many bytes: the UI shows a percentage, so more
/// frequent events would only cost bandwidth on a fast link.
const PROGRESS_STEP: u64 = 4 * 1024 * 1024;
/// How much of the compressed bundle to consume between unpack reports.
const UNPACK_STEP: u64 = 4 * 1024 * 1024;

/// 产品目录链接的登记表：链接名 → 拥有它的包。卸载时只删自己的。
const DATA_LINKS_REGISTRY: &str = ".data-links.json";

fn data_links_path() -> std::path::PathBuf {
    packages_root().join(DATA_LINKS_REGISTRY)
}

fn read_data_links() -> std::collections::BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(data_links_path()) else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_data_links(registry: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    let path = data_links_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(registry)
        .map_err(|error| format!("cannot encode data link registry: {error}"))?;
    std::fs::write(&path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// 建立清单里声明的那几条产品目录链接（`{"zig": "lib"}` → `<product>/zig`）。
///
/// 数据包要的是**固定路径**：zig 只认 `$ZIG_LIB_DIR`，字体引擎只认自己的目录名。链接
/// 和 python_packages 同一套所有权登记，卸载时精确摘除。
fn install_data_links(manifest: &Manifest, package_dir: &Path) -> Result<(), String> {
    if manifest.links.is_empty() {
        return Ok(());
    }
    let product = packages_root()
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "packages root has no parent".to_string())?;
    let mut registry = read_data_links();
    let mut created: Vec<String> = Vec::new();

    for (name, relative) in &manifest.links {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." {
            return Err(format!("invalid link name: {name:?}"));
        }
        let source = package_dir.join(relative);
        if !source.is_dir() {
            return Err(format!(
                "link target is missing in the bundle: {relative} (for `{name}`)"
            ));
        }
        if let Some(owner) = registry.get(name) {
            if owner != &manifest.name {
                return Err(format!("`{name}` is already linked by package `{owner}`"));
            }
        }
        let link = product.join(name);
        // 真实文件/目录（不是链接）说明有人真放了东西在那儿：拒绝，别覆盖。
        if registry.get(name).is_none() && link.symlink_metadata().is_ok() {
            return Err(format!("`{name}` already exists in the product directory"));
        }
        let _ = std::fs::remove_file(&link);
        write_symlink(&source, &link).map_err(|error| format!("cannot link `{name}`: {error}"))?;
        registry.insert(name.clone(), manifest.name.clone());
        created.push(name.clone());
    }
    write_data_links(&registry)?;

    // zig 只认环境变量；装完立即生效（daemon 的进程环境会传给 bash 工具起的子进程），
    // 不用等下一次重启。
    if manifest.links.contains_key("zig") {
        let lib = linked_library_dir(&product, "zig");
        if lib.is_dir() {
            std::env::set_var("ZIG_LIB_DIR", &lib);
        }
    }
    let _ = created;
    Ok(())
}

/// The directory a `links` entry exposes to a tool that reads its library
/// through a fixed path (zig through `$ZIG_LIB_DIR`).
///
/// A data package declares `{"zig": "lib"}`: `<product>/zig` becomes a link to
/// the package's own `lib/`, and that link **is** the library directory — zig
/// looks for `std/` *under* it. So the variable names the link itself. Naming a
/// child of it would point at a directory that does not exist, the variable
/// would go unset, and zig would report a missing standard library.
fn linked_library_dir(product: &Path, link_name: &str) -> PathBuf {
    product.join(link_name)
}

/// 摘掉本包建立的目录链接，并忘掉它们。
fn remove_data_links(manifest: &Manifest) {
    if manifest.links.is_empty() {
        return;
    }
    let Ok(product) = packages_root()
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "packages root has no parent".to_string())
    else {
        return;
    };
    let mut registry = read_data_links();
    let mine: Vec<String> = registry
        .iter()
        .filter(|(_, owner)| *owner == &manifest.name)
        .map(|(name, _)| name.clone())
        .collect();
    for name in &mine {
        let _ = std::fs::remove_file(product.join(name));
        registry.remove(name);
    }
    let _ = write_data_links(&registry);
}

/// Read a local bundle in chunks, reporting progress as it goes.
///
/// The non-streaming endpoint reads the file in one call, which is fine when
/// nobody is watching; this one exists so a person installing a 47 MB bundle
/// can see it moving.
pub fn read_path_streaming(
    path: &str,
    tx: &UnboundedSender<WireProgress>,
) -> Result<Vec<u8>, String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("cannot read bundle {path}: {error}"))?;
    let total = file.metadata().ok().map(|metadata| metadata.len());
    if let Some(total) = total {
        if total > MAX_BUNDLE_BYTES {
            return Err(format!(
                "bundle is {total} bytes, over the {MAX_BUNDLE_BYTES} byte limit"
            ));
        }
    }
    let mut out = Vec::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut next_report = PROGRESS_STEP;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read bundle {path}: {error}"))?;
        if read == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..read]);
        if out.len() as u64 >= next_report {
            // A failed send means the client's stream is gone: stop reading
            // instead of pulling the rest of a bundle nobody asked for.
            if tx
                .send(WireProgress::transfer("download", out.len() as u64, total))
                .is_err()
            {
                return Err(CANCELLED.to_string());
            }
            next_report = out.len() as u64 + PROGRESS_STEP;
        }
        if out.len() as u64 > MAX_BUNDLE_BYTES {
            return Err("bundle over the size limit".to_string());
        }
    }
    let _ = tx.send(WireProgress::transfer("download", out.len() as u64, total));
    Ok(out)
}

/// Download a bundle, reporting bytes as they arrive.
pub async fn download_streaming(
    url: &str,
    tx: &UnboundedSender<WireProgress>,
) -> Result<Vec<u8>, String> {
    use futures::StreamExt;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("bundle URL must be http(s): {url}"));
    }
    let response = reqwest::get(url)
        .await
        .map_err(|error| format!("download failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("download failed: HTTP {}", response.status()));
    }
    let total = response.content_length();
    if let Some(total) = total {
        if total > MAX_BUNDLE_BYTES {
            return Err(format!(
                "bundle is {total} bytes, over the {MAX_BUNDLE_BYTES} byte limit"
            ));
        }
    }
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    let mut next_report = PROGRESS_STEP;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("download failed: {error}"))?;
        out.extend_from_slice(&chunk);
        if out.len() as u64 >= next_report {
            // Same as the local read: a failed send is the client leaving.
            if tx
                .send(WireProgress::transfer("download", out.len() as u64, total))
                .is_err()
            {
                return Err(CANCELLED.to_string());
            }
            next_report = out.len() as u64 + PROGRESS_STEP;
        }
        if out.len() as u64 > MAX_BUNDLE_BYTES {
            return Err("bundle over the size limit".to_string());
        }
    }
    let _ = tx.send(WireProgress::transfer("download", out.len() as u64, total));
    Ok(out)
}

/// Install with a progress stream. Always ends with exactly one `done` or
/// `error` event, so a client never has to guess whether the stream just
/// stopped.
pub async fn install_streaming(req: InstallRequest, tx: UnboundedSender<WireProgress>) {
    let result = run_streaming(req, &tx).await;
    let _ = match result {
        Ok(record) => tx.send(WireProgress::done(record)),
        Err(message) => tx.send(WireProgress::error(message)),
    };
}

async fn run_streaming(
    req: InstallRequest,
    tx: &UnboundedSender<WireProgress>,
) -> Result<PackageRecord, String> {
    let bundle = if let Some(path) = req.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        let path = path.to_string();
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || read_path_streaming(&path, &tx))
            .await
            .map_err(|error| format!("read task failed: {error}"))??
    } else {
        let Some(url) = req.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
            return Err("either `url` or `path` is required".to_string());
        };
        download_streaming(url, tx).await?
    };

    let _ = tx.send(WireProgress::stage("verify"));
    if let Some(expected) = req.sha256.as_deref() {
        verify_sha256(&bundle, expected)?;
    }

    let stage_tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        // `false` propagates a cancel: once the client's stream is gone the
        // send fails, and there is no point unpacking 47 MB for nobody.
        install_bytes_with(&bundle, &mut |stage| {
            let event = match stage {
                InstallStage::Unpacking {
                    files,
                    consumed,
                    total,
                } => WireProgress::unpacked(files, consumed, total),
                InstallStage::Linking => WireProgress::stage("link"),
            };
            stage_tx.send(event).is_ok()
        })
    })
    .await
    .map_err(|error| format!("install task failed: {error}"))?
}

fn unpack_tar_gz(
    bundle: &[u8],
    destination: &Path,
    on_stage: &mut dyn FnMut(InstallStage) -> bool,
) -> Result<(), String> {
    // Unpacking reports progress in *compressed* bytes consumed, because that is
    // the only total known up front. The counter is shared (`Rc<Cell>`) rather
    // than read back from the decoder: `entries()` borrows the archive mutably
    // for the whole loop, so `get_ref()` is out of reach while iterating.
    let consumed = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let reader = CountingReader {
        inner: bundle,
        consumed: std::rc::Rc::clone(&consumed),
    };
    let total = bundle.len() as u64;
    let decoder = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("bundle is not a valid .tar.gz: {error}"))?;
    let mut files = 0u64;
    let mut next_report = UNPACK_STEP;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("corrupt bundle entry: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("corrupt bundle path: {error}"))?
            .into_owned();
        // Refuse anything that could escape the package directory. tar itself
        // warns about `..`, but a crafted bundle can still try. An entry for the
        // archive root (`.` — what `tar -C dir -cf - .` writes first) carries no
        // path of its own and is skipped rather than treated as an attack.
        let relative = match classify(&path) {
            Relative::Inside(relative) => relative,
            Relative::RootDir => continue,
            Relative::Escape => {
                return Err(format!(
                    "bundle entry escapes the package root: {}",
                    path.display()
                ))
            }
        };
        let target = destination.join(&relative);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let mut body = Vec::new();
        entry
            .read_to_end(&mut body)
            .map_err(|error| format!("cannot read bundle entry: {error}"))?;
        std::fs::write(&target, &body)
            .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
        files += 1;
        // Report on whichever comes first: 200 files, or 4 MB of bundle
        // consumed. File counts alone cannot drive a percentage, and a single
        // huge file (a 20 MB shared library) would otherwise look frozen. A
        // `false` answer means the caller went away — stop unpacking rather
        // than fill the disk for nobody.
        let written = consumed.get();
        if files % 200 == 0 || written >= next_report {
            next_report = written + UNPACK_STEP;
            let stage = InstallStage::Unpacking {
                files,
                consumed: written,
                total,
            };
            if !on_stage(stage) {
                return Err(CANCELLED.to_string());
            }
        }
    }
    let stage = InstallStage::Unpacking {
        files,
        consumed: consumed.get(),
        total,
    };
    if !on_stage(stage) {
        return Err(CANCELLED.to_string());
    }
    Ok(())
}

/// Counts the bytes a reader has handed out, so a tar loop can report how far
/// through the compressed bundle it is.
struct CountingReader<'a> {
    inner: &'a [u8],
    consumed: std::rc::Rc<std::cell::Cell<u64>>,
}

impl std::io::Read for CountingReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = std::io::Read::read(&mut self.inner, buffer)?;
        self.consumed.set(self.consumed.get() + read as u64);
        Ok(read)
    }
}

/// The error text a cancelled install produces. Callers report it as a normal
/// failure; the streaming endpoint turns it into an `error` event, which is
/// what a client that cancelled will simply never read.
const CANCELLED: &str = "install cancelled";

/// How an archive path classifies when it is rooted inside the package dir.
enum Relative {
    /// A normal path below the root: write it here.
    Inside(PathBuf),
    /// The archive's own root (`.` / `./`): nothing to write, not an attack.
    RootDir,
    /// Absolute, drive-prefixed or containing `..`: refuse the whole bundle.
    Escape,
}

/// Classify a path made only of normal components — no root, no `..`, no prefix.
fn classify(path: &Path) -> Relative {
    let mut out = PathBuf::new();
    let mut any = false;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                out.push(part);
                any = true;
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Relative::Escape;
            }
        }
    }
    if any {
        Relative::Inside(out)
    } else {
        Relative::RootDir
    }
}

fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join("package.json");
    let body = std::fs::read_to_string(&path)
        .map_err(|error| format!("bundle has no readable package.json: {error}"))?;
    serde_json::from_str(&body).map_err(|error| format!("package.json is invalid: {error}"))
}

pub(crate) fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if !is_package_name(&manifest.name) {
        return Err(format!(
            "package name must be [A-Za-z0-9][A-Za-z0-9._-]*, got {:?}",
            manifest.name
        ));
    }
    if manifest.version.trim().is_empty() {
        return Err("package version is empty".to_string());
    }
    if !is_symbol(&manifest.symbol) {
        return Err(format!("invalid symbol name: {:?}", manifest.symbol));
    }
    for alias in command_names(manifest) {
        if !is_package_name(&alias) {
            return Err(format!(
                "alias must be [A-Za-z0-9][A-Za-z0-9._-]*, got {alias:?}"
            ));
        }
    }
    // A data dir is joined onto the product directory, so it must stay a single
    // component: anything with a separator or a `..` could point anywhere.
    for directory in &manifest.data_dirs {
        let safe = !directory.is_empty()
            && directory != "."
            && directory != ".."
            && !directory.contains('/')
            && !directory.contains('\\');
        if !safe {
            return Err(format!(
                "data dir must be a single directory name, got {directory:?}"
            ));
        }
    }
    if !manifest.python_packages.is_empty() {
        match classify(Path::new(&manifest.python_packages)) {
            Relative::Inside(_) => {}
            Relative::RootDir | Relative::Escape => {
                return Err(format!(
                    "python_packages must be a relative path inside the package: {:?}",
                    manifest.python_packages
                ))
            }
        }
    }
    Ok(())
}

/// Every command name a package answers to: its own name plus its aliases,
/// without duplicates.
fn command_names(manifest: &Manifest) -> Vec<String> {
    // A data-only package has nothing to run. Giving it a bin link would only
    // produce a command that fails inside `pluginhost` at exec time.
    if manifest.entry.is_empty() {
        return Vec::new();
    }
    let mut names = vec![manifest.name.clone()];
    for alias in &manifest.aliases {
        if !names.contains(alias) {
            names.push(alias.clone());
        }
    }
    names
}

fn validate_entry(dir: &Path, entry: &str) -> Result<(), String> {
    let relative = match classify(Path::new(entry)) {
        Relative::Inside(relative) => relative,
        Relative::RootDir | Relative::Escape => {
            return Err(format!(
                "entry must be a relative path inside the package: {entry}"
            ))
        }
    };
    let target = dir.join(&relative);
    if !target.is_file() {
        return Err(format!("entry does not exist in the bundle: {entry}"));
    }
    Ok(())
}

/// A package id is also a directory name, so it may not contain separators.
pub fn is_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        && !name.contains("..")
}

fn is_symbol(symbol: &str) -> bool {
    !symbol.is_empty()
        && symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Where the command shims live (`<product>/bin`) — the same directory the host
/// fills with `git`, `jq`, `python3` and `pluginhost`.
fn bin_dir() -> PathBuf {
    packages_root()
        .parent()
        .map(|parent| parent.join("bin"))
        .unwrap_or_else(|| PathBuf::from("bin"))
}

/// A package's own name and its aliases become commands only when the launcher
/// is actually installed; on a desktop build there is nothing to point at, and
/// writing dead links would be worse than doing nothing.
fn launcher_available() -> bool {
    bin_dir().join("pluginhost").exists()
}

/// Refuse only what cannot be restored.
///
/// A package may take over a command name the host already provides — that is
/// how a CPython package becomes `python3`. What it may not do is replace a real
/// file on disk, because there would be nothing to put back.
fn check_aliases(manifest: &Manifest) -> Result<(), String> {
    if !launcher_available() {
        return Ok(());
    }
    let bin = bin_dir();
    for name in command_names(manifest) {
        // A name another package already took over stays theirs. Both packages
        // would otherwise point the same link at `pluginhost`, and the one that
        // was uninstalled first would take the other's command down with it.
        if let Some(holder) = holding_package(&name, &manifest.name) {
            if holder != manifest.name {
                return Err(format!(
                    "`{name}` is already provided by package `{holder}`"
                ));
            }
        }
        let link = bin.join(&name);
        let Ok(metadata) = std::fs::symlink_metadata(&link) else {
            continue;
        };
        if !metadata.file_type().is_symlink() {
            return Err(format!(
                "`{name}` already exists in {} and is not a link, so it cannot be taken over",
                bin.display()
            ));
        }
    }
    Ok(())
}

/// Create a symlink, on the platforms that have them.
///
/// Wrapped so the callers do not each need a `cfg` branch: desktop builds run
/// interpreters directly and never write command links anyway.
#[cfg(unix)]
fn write_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn write_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks are a unix feature",
    ))
}

/// A link this module wrote: it points at `pluginhost` in the same directory.
fn is_launcher_link(target: &Path) -> bool {
    target
        .file_name()
        .map(|name| name == "pluginhost")
        .unwrap_or(false)
}

/// Where a displaced host link is remembered: `<product>/.bin-overrides/<name>`.
///
/// Installing a package may take over a command name the host already provides
/// (`python3` → the bundled RustPython is the motivating case). The name is
/// restorable only if we know what it pointed at, so that is written down.
fn overrides_dir() -> PathBuf {
    packages_root()
        .parent()
        .map(|parent| parent.join(".bin-overrides"))
        .unwrap_or_else(|| PathBuf::from(".bin-overrides"))
}

/// What we displaced for `name`: the *basename* of the old target.
///
/// The basename is deliberate. Host links point into the APK's
/// `nativeLibraryDir`, whose absolute path changes on every app update
/// (`/data/app/~~hash/…`), so a recorded full path would be stale exactly when
/// we need it. The directory is re-derived at restore time instead.
/// What a takeover displaced, and who holds the name now.
struct OverrideRecord {
    /// Basename of the host target we displaced (e.g. `libpython3.so`).
    base: String,
    /// Package that took the name over. `None` for records written before the
    /// owner was tracked — those behave like the old single-owner world.
    owner: Option<String>,
}

/// Read the record for `name`, accepting both formats.
///
/// Current format is two lines (`base`, then owner); the first shipped format
/// was the basename alone. Old records stay readable so an upgrade does not
/// strand a name in a taken-over state.
fn recorded_override(name: &str) -> Option<OverrideRecord> {
    let body = std::fs::read_to_string(overrides_dir().join(name)).ok()?;
    let mut lines = body.lines().map(str::trim).filter(|line| !line.is_empty());
    let base = lines.next()?.to_string();
    Some(OverrideRecord {
        base,
        owner: lines.next().map(str::to_string),
    })
}

fn remember_override(name: &str, target: &Path, owner: &str) {
    let Some(base) = target
        .file_name()
        .map(|part| part.to_string_lossy().to_string())
    else {
        return;
    };
    heal_override(name, &base, owner);
}

/// (Re)write a record in the current two-line format.
fn heal_override(name: &str, base: &str, owner: &str) {
    let dir = overrides_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(name), format!("{base}\n{owner}\n"));
    }
}

/// Which installed package other than `except` claims this command name.
///
/// Only needed for records written before the owner was tracked: the file says
/// what was displaced but not who holds it now, and the installed manifests are
/// the only place that answer exists. Every install re-heals the record, so the
/// scan runs rarely.
fn current_holder(name: &str, except: &str) -> Option<String> {
    let entries = std::fs::read_dir(packages_root()).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(manifest) = read_manifest(&dir) else {
            continue;
        };
        if manifest.name == except {
            continue;
        }
        if command_names(&manifest)
            .iter()
            .any(|candidate| candidate == name)
        {
            return Some(manifest.name);
        }
    }
    None
}

/// The package that holds `name` right now, from the record or, for a legacy
/// record, from the installed manifests.
fn holding_package(name: &str, except: &str) -> Option<String> {
    match recorded_override(name) {
        Some(record) => match record.owner {
            Some(owner) => Some(owner),
            None => {
                let holder = current_holder(name, except);
                if let Some(owner) = &holder {
                    // Adopt the old note so a later uninstall respects it.
                    heal_override(name, &record.base, owner);
                }
                holder
            }
        },
        None => None,
    }
}

fn forget_override(name: &str) {
    let _ = std::fs::remove_file(overrides_dir().join(name));
}

/// The app's current native library directory.
///
/// Derived from a host link rather than stored: the host rewrites those links at
/// every start-up, so whichever one still resolves tells us where "there" is
/// now — even after an app update moved it.
fn current_lib_dir() -> Option<PathBuf> {
    let entries = std::fs::read_dir(bin_dir()).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(target) = std::fs::read_link(&path) else {
            continue;
        };
        if !target.is_absolute() {
            continue;
        }
        if let Some(parent) = target.parent() {
            if parent.is_dir() {
                return Some(parent.to_path_buf());
            }
        }
    }
    None
}

#[cfg(unix)]
fn install_alias_links(manifest: &Manifest) -> Result<(), String> {
    if !launcher_available() {
        return Ok(());
    }
    let bin = bin_dir();
    for name in command_names(manifest) {
        let link = bin.join(&name);
        // Taking over a host-provided name (`python3` → the bundled RustPython)
        // is allowed, but only reversibly: write down what we are displacing.
        // A link that already points at `pluginhost` is ours and needs no note.
        if recorded_override(&name).is_none() {
            if let Ok(target) = std::fs::read_link(&link) {
                if !is_launcher_link(&target) {
                    remember_override(&name, &target, &manifest.name);
                }
            }
        }
        let _ = std::fs::remove_file(&link);
        // Relative target: `bin/` moves only if the whole tree does, and the
        // app's data directory is stable across updates.
        write_symlink(Path::new("pluginhost"), &link)
            .map_err(|error| format!("cannot link `{name}`: {error}"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_alias_links(_manifest: &Manifest) -> Result<(), String> {
    // Desktop builds run the interpreter directly; no shims to write.
    Ok(())
}

fn remove_alias_links(manifest: &Manifest) {
    for name in command_names(manifest) {
        remove_alias_link(&name, &manifest.name);
    }
}

/// Drop one command link, and put back whatever it displaced.
///
/// A package that took over a host name (`python3`) leaves the bundled tool
/// unreachable only until it is removed; the recorded basename is resolved
/// against the *current* library directory, so the restore survives an app
/// update that moved `/data/app/~~hash/…`.
///
/// `owner` guards the other direction: a link only points at `pluginhost`, so
/// "is it ours?" cannot tell two packages apart. Without the owner check,
/// uninstalling one package would delete a link another package created and
/// take that command down with it.
fn remove_alias_link(name: &str, owner: &str) {
    if !launcher_available() {
        return;
    }
    let record = recorded_override(name);
    // A link only points at `pluginhost`, so "is it ours?" cannot tell two
    // packages apart. Ask who holds the name instead — the record says it, or
    // (for a legacy record) the installed manifests do.
    if let Some(holder) = holding_package(name, owner) {
        if holder != owner {
            // Somebody else provides this name now; leaving it alone is the
            // only thing that does not break their command.
            return;
        }
    }
    let link = bin_dir().join(name);
    let ours = matches!(std::fs::read_link(&link), Ok(target) if is_launcher_link(&target));
    if ours {
        let _ = std::fs::remove_file(&link);
    }

    let Some(record) = record else {
        return;
    };
    let base = record.base;
    // Only restore over a link we own (or one that is now gone). A real file
    // sitting there is somebody else's business.
    let occupied_by_other = std::fs::symlink_metadata(&link)
        .map(|metadata| !metadata.file_type().is_symlink() || !ours)
        .unwrap_or(false);
    if !occupied_by_other {
        if let Some(dir) = current_lib_dir() {
            let _ = std::fs::remove_file(&link);
            let _ = write_symlink(&dir.join(base), &link);
        }
    }
    forget_override(name);
}

fn record_for(dir: &Path, manifest: &Manifest) -> Result<PackageRecord, String> {
    // The kept generation lives beside the installed one, named after it.
    let previous_version = dir
        .parent()
        .map(|root| root.join(format!(".previous-{}", manifest.name)))
        .and_then(|previous| read_manifest(&previous).ok())
        .map(|previous| previous.version);
    Ok(PackageRecord {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        entry: manifest.entry.clone(),
        symbol: manifest.symbol.clone(),
        commands: command_names(manifest),
        previous_version,
        size_bytes: tree_size(dir),
    })
}

fn tree_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => total += tree_size(&path),
            Ok(metadata) => total += metadata.len(),
            Err(_) => {}
        }
    }
    total
}

/// Every installed package, sorted by name.
pub fn scan() -> Result<Vec<PackageRecord>, String> {
    let root = packages_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut packages = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Staging / retired dirs are ours, not packages.
        if name.starts_with('.') {
            continue;
        }
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(manifest) = read_manifest(&dir) else {
            continue;
        };
        if let Ok(record) = record_for(&dir, &manifest) {
            packages.push(record);
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
}

fn remove(name: &str) -> Result<bool, String> {
    if !is_package_name(name) {
        return Err(format!("invalid package name: {name:?}"));
    }
    let root = packages_root();
    let dir = root.join(name);
    if !dir.exists() {
        return Ok(false);
    }
    // Take the command names away first: a link left pointing at a deleted
    // package would fail at exec time with a confusing loader error.
    if let Ok(manifest) = read_manifest(&dir) {
        remove_alias_links(&manifest);
        remove_python_links(&manifest);
        remove_data_links(&manifest);
    }
    // A removed package's native libraries must stop being searched.
    refresh_native_library_path();
    // The kept generation is part of this package; uninstalling means both
    // versions go. Its command names may differ from the installed one's.
    let previous = root.join(format!(".previous-{name}"));
    if let Ok(manifest) = read_manifest(&previous) {
        remove_alias_links(&manifest);
    }
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::remove_dir_all(&dir)
        .map_err(|error| format!("cannot remove {}: {error}", dir.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_with(manifest: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut add = |path: &str, body: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, body).unwrap();
        };
        add("package.json", manifest.as_bytes());
        for (path, body) in files {
            add(path, body);
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// `packages_root()` reads the product dir from the environment, which is
    /// process-global. Tests that point it at a temp tree must not overlap, or
    /// one test's tree is silently used by another.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn aliases_appear_in_the_record() {
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"cpython","version":"3.12","entry":"lib/demo.so","aliases":["pycp"]}"#,
                &[("lib/demo.so", b"\x7fELF")],
            );
            let record = install_bytes(&bundle).unwrap();
            assert_eq!(record.commands, vec!["cpython", "pycp"]);
        });
    }

    #[test]
    fn a_data_only_package_installs_without_owning_a_command() {
        // C headers, examples, docs: files with nothing to run. An empty `entry`
        // is how a bundle says so, and it must not produce a bin link — a command
        // whose only possible outcome is failing inside pluginhost is worse than
        // no command at all.
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"cheaders","version":"1.0.0"}"#,
                &[("include/stdio.h", b"/* header */")],
            );
            let record = install_bytes(&bundle).unwrap();
            assert!(record.commands.is_empty(), "{:?}", record.commands);
            assert!(record.entry.is_empty(), "{:?}", record.entry);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_python_payload_is_linked_into_python_packages_and_unlinked_with_it() {
        // A wheel's payload is exposed by symlink, not by copy: one copy on disk,
        // and removing the package removes exactly its own names. Unix-only
        // because the whole mechanism is symlinks.
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"numpy","version":"1.26.2","python_packages":"lib/python"}"#,
                &[
                    ("lib/python/numpy/__init__.py", b"# numpy"),
                    ("lib/python/numpy-1.26.2.dist-info/METADATA", b"Name: numpy"),
                ],
            );
            let record = install_bytes(&bundle).unwrap();
            // Data-like: no command name, but it does put something on PYTHONPATH.
            assert!(record.commands.is_empty(), "{:?}", record.commands);

            let product = packages_root().parent().unwrap().to_path_buf();
            let linked = product.join("python-packages").join("numpy");
            assert!(
                linked.exists(),
                "numpy must be reachable from python-packages"
            );

            remove("numpy").unwrap();
            assert!(!linked.exists(), "the link must go with its package");
        });
    }

    #[test]
    fn declared_data_dirs_are_leftovers_only_after_the_owner_is_removed() {
        // A runtime keeps `python-packages` outside its own tree so libraries
        // survive an update. The flip side: removing the runtime leaves them
        // behind, and nothing on disk then says who they belonged to. The
        // registry written at install time is what makes them reportable.
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"cpython","version":"3.12","entry":"lib/demo.so","aliases":["python3"],"data_dirs":["python-packages"]}"#,
                &[("lib/demo.so", b"\x7fELF")],
            );
            install_bytes(&bundle).unwrap();

            let product = packages_root().parent().unwrap().to_path_buf();
            let shared = product.join("python-packages");
            std::fs::create_dir_all(&shared).unwrap();
            std::fs::write(shared.join("lib.py"), b"payload").unwrap();

            // Owner installed: the directory is live, not a leftover.
            assert!(runtime_leftovers().is_empty());

            remove("cpython").unwrap();
            let leftovers = runtime_leftovers();
            assert_eq!(leftovers.len(), 1);
            assert_eq!(leftovers[0].package, "cpython");
            assert_eq!(leftovers[0].name, "python-packages");
            assert!(leftovers[0].bytes > 0);
        });
    }

    #[test]
    fn an_escaping_data_dir_is_refused() {
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"demo","version":"1","entry":"lib/demo.so","data_dirs":["../escape"]}"#,
                &[("lib/demo.so", b"\x7fELF")],
            );
            let error = install_bytes(&bundle).unwrap_err();
            assert!(error.contains("data dir must be"), "{error}");
        });
    }

    #[test]
    fn an_invalid_alias_is_refused() {
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"demo","version":"1","entry":"lib/demo.so","aliases":["../evil"]}"#,
                &[("lib/demo.so", b"\x7fELF")],
            );
            let error = install_bytes(&bundle).unwrap_err();
            assert!(error.contains("alias must be"), "{error}");
        });
    }

    // A package may claim a command name, but never one the host already
    // provides — installing would otherwise shadow `git` with an interpreter.
    #[test]
    fn a_command_name_already_taken_is_refused() {
        with_temp_root(|| {
            let bin = bin_dir();
            std::fs::create_dir_all(&bin).unwrap();
            // The launcher must look present for name checking to run at all.
            std::fs::write(bin.join("pluginhost"), b"stub").unwrap();
            std::fs::write(bin.join("git"), b"stub").unwrap();

            let bundle = bundle_with(
                r#"{"name":"git","version":"1","entry":"lib/demo.so"}"#,
                &[("lib/demo.so", b"\x7fELF")],
            );
            let error = install_bytes(&bundle).unwrap_err();
            assert!(error.contains("already exists"), "{error}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn installing_links_the_command_and_removing_takes_it_away() {
        with_temp_root(|| {
            let bin = bin_dir();
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("pluginhost"), b"stub").unwrap();

            let bundle = bundle_with(
                r#"{"name":"cpython","version":"3","entry":"lib/demo.so","aliases":["pycp"]}"#,
                &[("lib/demo.so", b"\x7fELF")],
            );
            install_bytes(&bundle).unwrap();

            for name in ["cpython", "pycp"] {
                let link = bin.join(name);
                let target = std::fs::read_link(&link).unwrap();
                assert_eq!(
                    target,
                    Path::new("pluginhost"),
                    "`{name}` runs the launcher"
                );
            }

            assert!(remove("cpython").unwrap());
            for name in ["cpython", "pycp"] {
                assert!(
                    std::fs::symlink_metadata(bin.join(name)).is_err(),
                    "`{name}` must be gone after uninstall"
                );
            }
        });
    }

    // A client that walks away cancels the install. Nothing may be left behind:
    // a half-unpacked staging directory is worse than no install at all.
    #[test]
    fn cancelling_mid_install_leaves_nothing_behind() {
        with_temp_root(|| {
            let files: Vec<(&str, &[u8])> =
                (0..401).map(|_| ("lib/demo.so", b"x" as &[u8])).collect();
            // One entry is enough to reach the first stage report (every 200).
            let bundle = bundle_with(MANIFEST, &files[..1]);
            let error = install_bytes_with(&bundle, &mut |_| false).unwrap_err();
            assert!(error.contains("cancelled"), "{error}");

            let root = packages_root();
            let leftovers: Vec<_> = std::fs::read_dir(&root)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            assert!(
                leftovers.iter().all(|name| !name.starts_with(".staging-")),
                "staging left behind: {leftovers:?}"
            );
            assert!(scan().unwrap().is_empty(), "nothing may be installed");
        });
    }

    fn bundle_v1() -> Vec<u8> {
        bundle_with(
            r#"{"name":"demo","version":"1.0","entry":"lib/demo.so"}"#,
            &[("lib/demo.so", b"one")],
        )
    }

    fn bundle_v2() -> Vec<u8> {
        bundle_with(
            r#"{"name":"demo","version":"2.0","entry":"lib/demo.so"}"#,
            &[("lib/demo.so", b"two")],
        )
    }

    // An upgrade must stay undoable: the whole point of keeping one generation
    // is that a version which installs fine can still turn out to be wrong.
    #[test]
    fn an_upgrade_keeps_the_previous_version_and_rollback_restores_it() {
        with_temp_root(|| {
            install_bytes(&bundle_v1()).unwrap();
            let record = install_bytes(&bundle_v2()).unwrap();
            assert_eq!(record.version, "2.0");
            assert_eq!(
                record.previous_version.as_deref(),
                Some("1.0"),
                "the replaced version is kept"
            );
            assert_eq!(
                std::fs::read(packages_root().join("demo/lib/demo.so")).unwrap(),
                b"two"
            );

            let rolled = rollback("demo").unwrap();
            assert_eq!(rolled.version, "1.0");
            assert_eq!(
                rolled.previous_version.as_deref(),
                Some("2.0"),
                "a rollback is itself undoable"
            );
            assert_eq!(
                std::fs::read(packages_root().join("demo/lib/demo.so")).unwrap(),
                b"one",
                "the old library really is back on disk"
            );
            // Still exactly one installed package: the swap must not look like
            // two entries to `scan`.
            assert_eq!(scan().unwrap().len(), 1);
        });
    }

    #[test]
    fn rollback_without_a_previous_version_is_refused() {
        with_temp_root(|| {
            install_bytes(&bundle_v1()).unwrap();
            let error = rollback("demo").unwrap_err();
            assert!(error.contains("no previous version"), "{error}");
        });
    }

    #[test]
    fn rollback_of_something_not_installed_is_refused() {
        with_temp_root(|| {
            let error = rollback("demo").unwrap_err();
            assert!(error.contains("not installed"), "{error}");
        });
    }

    #[test]
    fn uninstalling_removes_both_generations() {
        with_temp_root(|| {
            install_bytes(&bundle_v1()).unwrap();
            install_bytes(&bundle_v2()).unwrap();
            assert!(remove("demo").unwrap());
            let leftovers: Vec<String> = std::fs::read_dir(packages_root())
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
        });
    }

    // A new version may stop claiming a command name; its link has to go, or
    // the command survives as a link that resolves to nothing.
    #[cfg(unix)]
    #[test]
    fn an_upgrade_drops_a_command_name_it_no_longer_claims() {
        with_temp_root(|| {
            let bin = bin_dir();
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("pluginhost"), b"stub").unwrap();

            let with_alias = bundle_with(
                r#"{"name":"demo","version":"1.0","entry":"lib/demo.so","aliases":["demo-old"]}"#,
                &[("lib/demo.so", b"one")],
            );
            install_bytes(&with_alias).unwrap();
            assert!(std::fs::symlink_metadata(bin.join("demo-old")).is_ok());

            // 2.0 drops the alias.
            install_bytes(&bundle_v2()).unwrap();
            assert!(
                std::fs::symlink_metadata(bin.join("demo-old")).is_err(),
                "the dropped command name must not linger"
            );
            assert!(std::fs::symlink_metadata(bin.join("demo")).is_ok());
        });
    }

    // The motivating case: a CPython package declares `python3`, takes the name
    // over from the bundled interpreter, and gives it back when removed.
    #[cfg(unix)]
    #[test]
    fn taking_over_a_host_command_restores_it_on_uninstall() {
        with_temp_root(|| {
            let bin = bin_dir();
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("pluginhost"), b"stub").unwrap();
            // Stand in for the APK's nativeLibraryDir: the host's own `python3`
            // points into a library directory, not at `pluginhost`.
            let lib = bin.parent().unwrap().join("fakelib");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(lib.join("libpython3.so"), b"stub").unwrap();
            std::os::unix::fs::symlink(lib.join("libpython3.so"), bin.join("python3")).unwrap();

            let bundle = bundle_with(
                r#"{"name":"cpython","version":"3","entry":"lib/demo.so","aliases":["python3"]}"#,
                &[("lib/demo.so", b"elf")],
            );
            install_bytes(&bundle).unwrap();

            assert_eq!(
                std::fs::read_link(bin.join("python3")).unwrap(),
                Path::new("pluginhost"),
                "the package owns the name now"
            );
            let record = recorded_override("python3").expect("the takeover is written down");
            assert_eq!(
                record.base, "libpython3.so",
                "what it displaced is recorded"
            );
            assert_eq!(
                record.owner.as_deref(),
                Some("cpython"),
                "and who took it over"
            );

            assert!(remove("cpython").unwrap());
            assert_eq!(
                std::fs::read_link(bin.join("python3")).unwrap(),
                lib.join("libpython3.so"),
                "the host link comes back"
            );
            assert!(
                recorded_override("python3").is_none(),
                "the note is dropped once it has been used"
            );
        });
    }

    // An app update moves the library directory; the restore must resolve the
    // recorded basename against wherever the host's links point NOW.
    #[cfg(unix)]
    #[test]
    fn a_restore_follows_the_library_directory_after_a_move() {
        with_temp_root(|| {
            let bin = bin_dir();
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("pluginhost"), b"stub").unwrap();
            let old_lib = bin.parent().unwrap().join("lib-old");
            std::fs::create_dir_all(&old_lib).unwrap();
            std::fs::write(old_lib.join("libpython3.so"), b"stub").unwrap();
            std::os::unix::fs::symlink(old_lib.join("libpython3.so"), bin.join("python3")).unwrap();

            let bundle = bundle_with(
                r#"{"name":"cpython","version":"3","entry":"lib/demo.so","aliases":["python3"]}"#,
                &[("lib/demo.so", b"elf")],
            );
            install_bytes(&bundle).unwrap();

            // The app updates: another host link now points at a new directory.
            let new_lib = bin.parent().unwrap().join("lib-new");
            std::fs::create_dir_all(&new_lib).unwrap();
            std::fs::write(new_lib.join("libpython3.so"), b"stub").unwrap();
            std::fs::write(new_lib.join("libgit.so"), b"stub").unwrap();
            let _ = std::fs::remove_file(bin.join("git"));
            std::os::unix::fs::symlink(new_lib.join("libgit.so"), bin.join("git")).unwrap();

            assert!(remove("cpython").unwrap());
            assert_eq!(
                std::fs::read_link(bin.join("python3")).unwrap(),
                new_lib.join("libpython3.so"),
                "the stale absolute path must not be reused"
            );
        });
    }

    // Two packages claiming the same command name would share one link, and the
    // first uninstall would take the other's command down. The second install is
    // refused instead.
    #[cfg(unix)]
    #[test]
    fn a_name_another_package_took_over_is_refused() {
        with_temp_root(|| {
            let bin = bin_dir();
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("pluginhost"), b"stub").unwrap();
            let lib = bin.parent().unwrap().join("fakelib");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(lib.join("libpython3.so"), b"stub").unwrap();
            std::os::unix::fs::symlink(lib.join("libpython3.so"), bin.join("python3")).unwrap();

            let first = bundle_with(
                r#"{"name":"cpython","version":"3","entry":"lib/a.so","aliases":["python3"]}"#,
                &[("lib/a.so", b"elf")],
            );
            install_bytes(&first).unwrap();

            let second = bundle_with(
                r#"{"name":"other","version":"1","entry":"lib/b.so","aliases":["python3"]}"#,
                &[("lib/b.so", b"elf")],
            );
            let error = install_bytes(&second).unwrap_err();
            assert!(
                error.contains("cpython"),
                "the refusal must name the package that holds it, got: {error}"
            );
            assert_eq!(
                std::fs::read_link(bin.join("python3")).unwrap(),
                Path::new("pluginhost"),
                "the first package keeps its command"
            );
        });
    }

    fn with_temp_root<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        // The package root comes from the product dirs, which come from the
        // environment. Point it at a throwaway tree for the duration.
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ATOMCODE_HOME");
        std::env::set_var("ATOMCODE_HOME", dir.path());
        let result = f();
        match previous {
            Some(value) => std::env::set_var("ATOMCODE_HOME", value),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
        result
    }

    const MANIFEST: &str =
        r#"{"name":"demo","version":"1.0","description":"d","entry":"lib/demo.so"}"#;

    #[test]
    fn a_bundle_installs_lists_and_removes() {
        with_temp_root(|| {
            let bundle = bundle_with(MANIFEST, &[("lib/demo.so", b"\x7fELF")]);
            let record = install_bytes(&bundle).expect("installs");
            assert_eq!(record.name, "demo");
            assert_eq!(record.symbol, "plugin_main", "symbol defaults");
            assert!(record.size_bytes > 0);

            let listed = scan().unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].name, "demo");

            assert!(remove("demo").unwrap());
            assert!(scan().unwrap().is_empty());
            assert!(!remove("demo").unwrap(), "removing twice reports absent");
        });
    }

    #[test]
    fn installing_the_same_name_replaces_it() {
        with_temp_root(|| {
            let first = bundle_with(MANIFEST, &[("lib/demo.so", b"one")]);
            install_bytes(&first).unwrap();
            let second = bundle_with(
                r#"{"name":"demo","version":"2.0","entry":"lib/demo.so"}"#,
                &[("lib/demo.so", b"two")],
            );
            let record = install_bytes(&second).unwrap();
            assert_eq!(record.version, "2.0");
            assert_eq!(scan().unwrap().len(), 1, "still exactly one demo");
        });
    }

    #[test]
    fn a_bundle_without_a_manifest_is_refused() {
        with_temp_root(|| {
            let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            ));
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "lib/demo.so", &b"hi"[..])
                .unwrap();
            let bundle = builder.into_inner().unwrap().finish().unwrap();
            let error = install_bytes(&bundle).unwrap_err();
            assert!(error.contains("package.json"), "{error}");
        });
    }

    // A bundle that tries to write outside its directory must be refused. The
    // `tar` crate already declines to *write* such an entry, so the bundle
    // cannot even be built through its safe API — the guard that matters is the
    // unpack-side classification, tested here directly.
    #[test]
    fn paths_that_escape_are_classified_as_escapes() {
        use std::path::Path;
        for escaped in ["../escape.txt", "a/../../b", "/etc/passwd"] {
            assert!(
                matches!(classify(Path::new(escaped)), Relative::Escape),
                "{escaped} must be refused"
            );
        }
        // The archive's own root is not an escape, and normal paths stay inside.
        assert!(matches!(classify(Path::new("./")), Relative::RootDir));
        assert!(matches!(classify(Path::new(".")), Relative::RootDir));
        match classify(Path::new("./lib/demo.so")) {
            Relative::Inside(path) => assert_eq!(path, Path::new("lib/demo.so")),
            _ => panic!("a normal path must classify as Inside"),
        }
    }

    #[test]
    fn a_manifest_entry_outside_the_package_is_refused() {
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"demo","version":"1","entry":"../outside.so"}"#,
                &[("lib/demo.so", b"x")],
            );
            let error = install_bytes(&bundle).unwrap_err();
            assert!(error.contains("relative path inside"), "{error}");
        });
    }

    #[test]
    fn a_missing_entry_library_is_refused() {
        with_temp_root(|| {
            let bundle = bundle_with(MANIFEST, &[]);
            let error = install_bytes(&bundle).unwrap_err();
            assert!(error.contains("does not exist"), "{error}");
        });
    }

    #[test]
    fn a_linked_library_dir_names_the_link_itself() {
        // The rule is platform-independent even though symlinks are not: zig
        // reads `std/` *under* `$ZIG_LIB_DIR`, and the link **is** the library
        // directory. Naming a child of it points at a directory that does not
        // exist, so the variable goes unset and zig reports a missing stdlib.
        let product = Path::new("/product");
        assert_eq!(
            linked_library_dir(product, "zig"),
            Path::new("/product/zig"),
            "the variable names the link, not a child of it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_data_package_links_its_directory_and_points_zig_at_it() {
        with_temp_root(|| {
            let bundle = bundle_with(
                r#"{"name":"zig-lib","version":"0.13.0","entry":"","links":{"zig":"lib"}}"#,
                &[("lib/std/std.zig", b"pub const x = 1;"), ("lib/c.zig", b"")],
            );
            install_bytes(&bundle).expect("a data-only package installs");

            let product = packages_root().parent().unwrap().to_path_buf();
            let link = product.join("zig");
            assert!(
                link.is_dir(),
                "the package links `<product>/zig` at its own library directory"
            );
            assert!(
                link.join("std").join("std.zig").is_file(),
                "the link resolves to the standard library tree, not to a parent of it"
            );

            let zig_lib = std::env::var("ZIG_LIB_DIR").expect("zig gets its library dir");
            assert_eq!(
                Path::new(&zig_lib),
                link.as_path(),
                "ZIG_LIB_DIR names the library directory itself"
            );
            assert!(
                Path::new(&zig_lib).join("std").is_dir(),
                "zig finds std/ under the directory it was given"
            );
        });
    }

    #[test]
    fn package_names_cannot_escape_their_directory() {
        assert!(is_package_name("cpython"));
        assert!(is_package_name("python3.12"));
        assert!(is_package_name("my-pkg_1"));
        assert!(!is_package_name("../evil"));
        assert!(!is_package_name("a/b"));
        assert!(!is_package_name("a\\b"));
        assert!(!is_package_name(".."));
        assert!(!is_package_name(""));
        assert!(!is_package_name("-leading"));
    }

    #[test]
    fn the_archive_root_entry_is_skipped_not_refused() {
        with_temp_root(|| {
            // `tar -C dir -cf - .` writes a `./` entry first. Treating that as
            // an escape made every normally-packed bundle uninstallable.
            let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::fast(),
            ));
            let mut add = |path: &str, body: &[u8]| {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, body).unwrap();
            };
            add("./", b"");
            add("./package.json", MANIFEST.as_bytes());
            add("./lib/demo.so", b"\x7fELF");
            let bundle = builder.into_inner().unwrap().finish().unwrap();
            let record = install_bytes(&bundle).expect("a root entry is not an attack");
            assert_eq!(record.name, "demo");
        });
    }

    #[test]
    fn a_bad_sha256_is_refused() {
        let bytes = b"hello";
        assert!(verify_sha256(
            bytes,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        )
        .is_ok());
        let error = verify_sha256(bytes, "00").unwrap_err();
        assert!(error.contains("mismatch"), "{error}");
    }
}
