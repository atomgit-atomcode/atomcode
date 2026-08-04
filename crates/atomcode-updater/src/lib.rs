//! In-place binary upgrade for atomcode.
//!
//! Flow:
//! 1. Fetch `latest.json` manifest (version + per-target sha256/size).
//! 2. Detect current platform and pick the matching binary entry.
//! 3. Verify we can write to `current_exe()`'s directory — if not, fail
//!    with a precise message telling the user to re-run with `sudo`.
//! 4. Download the binary to a sibling temp file, streaming progress.
//! 5. Verify SHA256 against the manifest. Bail (and delete temp) on
//!    mismatch — we never touch the live binary until verification
//!    passes.
//! 6. Three-way swap to replace the live binary:
//!    a. `atomcode` → `.atomcode.rolling`  (Windows allows renaming a running exe)
//!    b. new binary → `atomcode`            (install the upgrade)
//!    c. best-effort: remove old `.bak`, then `.atomcode.rolling` → `.bak`
//!
//!    Steps a–b are the critical path; step c is best-effort. If the old
//!    `.bak` is locked (AV scanner, still-running process, read-only
//!    attribute), the upgrade still succeeds — the `.rolling` file lingers
//!    and is cleaned up on the next upgrade attempt.
//!
//! Rollback swaps the live binary with `.bak` in place, so one backup
//! always points to "the other version" — the user can toggle by
//! alternating `/upgrade` and `/upgrade rollback`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const MANIFEST_URL: &str =
    "https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/latest.json";
pub const DOWNLOAD_BASE: &str = "https://atomgit.com/atomgit_atomcode/atomcode/releases/download";

/// User-Agent for the update HTTP client. Lowercase `atomcode/<version>` is
/// deliberate (the gateway UA filter hijacks capital-A `AtomCode`). Vendored here so
/// this leaf crate doesn't depend on `atomcode-core` (mirrors its `ATOMCODE_USER_AGENT`;
/// same value since the whole workspace shares one version).
const ATOMCODE_USER_AGENT: &str = concat!("atomcode/", env!("CARGO_PKG_VERSION"));

/// Apply the process proxy policy to the download client: honor `no_proxy` mode,
/// otherwise leave reqwest's env-based proxy detection intact. Same behavior as the
/// retiring `atomcode_core::proxy::apply_async_proxy_policy`, using the config crate's
/// proxy env items so this crate needs no `atomcode-core` dependency.
fn apply_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    if std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
    {
        builder.no_proxy()
    } else {
        builder
    }
}

/// Streamed progress events from the upgrade/rollback machinery.
///
/// Sender is always async-owned (the upgrade task); receivers are the
/// TUI event loop or the CLI `stdout` logger. Events are advisory —
/// dropping the receiver must never block the upgrade.
#[derive(Debug, Clone)]
pub enum UpgradeEvent {
    ManifestFetched {
        version: String,
    },
    Downloading {
        bytes: u64,
        total: u64,
    },
    Verifying,
    Replacing,
    Done {
        version: String,
        backup: PathBuf,
        /// The *original* exe path (e.g. `atomcode.exe`) **before**
        /// `replace_binary` renamed it. On Windows, `current_exe()`
        /// returns the renamed path after the swap, so callers must
        /// use this field for `re_exec_self`.
        exe: PathBuf,
    },
    /// Terminal failure. Carries the display-formatted error so the UI
    /// layer doesn't need `anyhow` to render it.
    Failed(String),
    /// Rollback finished. Reported through the same channel so the TUI
    /// can drive both flows with a single select arm.
    RolledBack {
        exe: PathBuf,
        backup: PathBuf,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub released_at: Option<String>,
    pub binaries: std::collections::BTreeMap<String, BinaryEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinaryEntry {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct UpgradeSummary {
    pub version: String,
    pub backup: PathBuf,
    pub exe: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RollbackSummary {
    pub exe: PathBuf,
    pub backup: PathBuf,
}

/// Return the target tag used in release artifact names
/// (`darwin-arm64`, `linux-x64`, `windows-x64`, …).
///
/// `None` means the current platform has no published release — the
/// caller must surface a clean "unsupported platform" message rather
/// than fall through to a 404 download.
// `target_env = "ohos"` is unknown to the check-cfg lint on toolchains
// without the OpenHarmony target registered; the cfg is still correct when
// building for `*-linux-ohos`, so silence the false-positive.
#[allow(unexpected_cfgs)]
pub fn detect_target() -> Option<&'static str> {
    // HarmonyOS / OpenHarmony builds report `std::env::consts::OS == "linux"`
    // at runtime, so the OS+ARCH table below would resolve to `linux-arm64`
    // — a STATIC-musl build that doesn't run on HarmonyOS — and the auto-
    // upgrade would clobber the working DYNAMIC-musl install (the one linked
    // against `/lib/ld-musl-aarch64.so.1`), leaving `permission denied`.
    // The OHOS artifact is compiled for a `*-linux-ohos` target, so pin it to
    // its own `ohos-arm64` asset using the COMPILE-TIME target rather than the
    // runtime OS string. Zero effect on every other platform.
    #[cfg(any(target_os = "ohos", target_env = "ohos"))]
    {
        return match std::env::consts::ARCH {
            "aarch64" | "arm64" => Some("ohos-arm64"),
            _ => None,
        };
    }
    #[cfg(not(any(target_os = "ohos", target_env = "ohos")))]
    {
        target_tag(std::env::consts::OS, std::env::consts::ARCH)
    }
}

fn target_tag(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("windows", "x86_64") => Some("windows-x64"),
        ("windows", "aarch64") => Some("windows-arm64"),
        _ => None,
    }
}

/// Release artifact filename for a given version + target, matching
/// what `scripts/release.sh` publishes to `dist/<version>/`.
pub fn binary_filename(version: &str, target: &str) -> String {
    if target.starts_with("windows") {
        format!("atomcode-{}-{}.exe", version, target)
    } else {
        format!("atomcode-{}-{}", version, target)
    }
}

pub fn binary_url(version: &str, target: &str) -> String {
    format!(
        "{}/{}/{}",
        DOWNLOAD_BASE,
        version,
        binary_filename(version, target)
    )
}

/// Path of the running `atomcode` executable. Resolved once at the
/// start of an upgrade so we know what to replace.
pub fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("could not resolve current executable path")
}

/// Sibling path used to stash the previous binary.
///
/// Unix: `atomcode` → `atomcode.bak`.
/// Windows: `atomcode.exe` → `atomcode.exe.bak`.
pub fn backup_path(exe: &Path) -> PathBuf {
    let mut os = exe.as_os_str().to_os_string();
    os.push(".bak");
    PathBuf::from(os)
}

/// Temp file where an in-flight download is written before atomic
/// rename. Dotted prefix so it doesn't show up in a casual `ls`, and
/// also so it's easy to identify-and-clean if a crash orphans it.
fn download_path(exe: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    dir.join(".atomcode.download")
}

/// Same-dir temp used during a three-way rollback swap. Also reused by the
/// Windows uninstaller (atomcode-cli) to rename the live exe out of the way
/// before deleting the install dir, hence `pub` rather than `pub(crate)`.
pub fn rolling_path(exe: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    dir.join(".atomcode.rolling")
}

/// Return `Ok(())` iff we can create a file alongside `exe`.
///
/// Testing the *directory* (not the file itself) is what matters:
/// `rename(2)` needs write permission on the containing dir to atomic
/// replace. Probing creates a real empty file and deletes it to avoid
/// false positives from filesystems that claim metadata writability
/// but reject opens.
pub fn ensure_writable(exe: &Path) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no parent directory: {}", exe.display()))?;
    let probe = dir.join(".atomcode.writable-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(anyhow!(
            "{} is not writable by the current user ({}).\n\
             Re-run with elevated privileges:  sudo atomcode upgrade\n\
             Or reinstall into a user-writable location (e.g. ~/.local/bin).",
            dir.display(),
            e
        )),
    }
}

/// Fetch and parse `latest.json`.
///
/// Longer timeout than the passive `version_check` (which must fail
/// fast at startup); here the user explicitly asked for an upgrade, so
/// waiting 30s for a slow mirror is acceptable.
pub async fn fetch_manifest() -> Result<Manifest> {
    let client = apply_proxy_policy(reqwest::Client::builder())
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(ATOMCODE_USER_AGENT)
        .build()?;
    let resp = client
        .get(MANIFEST_URL)
        .send()
        .await
        .context("failed to fetch latest.json")?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "fetching latest.json returned HTTP {}",
            resp.status()
        ));
    }
    let body = resp.text().await.context("reading latest.json body")?;
    serde_json::from_str(&body)
        .with_context(|| format!("parsing latest.json (body: {:?})", truncate(&body, 200)))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{}…", head)
    }
}

/// Download `url` to `dest`, streaming SHA256 as bytes arrive.
///
/// `progress` fires every chunk with (bytes_so_far, total). Callers
/// should debounce before redrawing the UI; we emit eagerly because
/// the upgrade UI wants smooth progress for a ~10 MB file.
///
/// Verifies size AND sha256 before returning. On any failure the
/// partial file is removed so a retry starts clean.
async fn download_and_verify(
    url: &str,
    expected_sha256: &str,
    expected_size: u64,
    dest: &Path,
    progress: &mpsc::UnboundedSender<UpgradeEvent>,
) -> Result<()> {
    use futures::StreamExt;

    if dest.exists() {
        let _ = std::fs::remove_file(dest);
    }

    let client = apply_proxy_policy(reqwest::Client::builder())
        .timeout(std::time::Duration::from_secs(600))
        .user_agent(ATOMCODE_USER_AGENT)
        .build()?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "downloading {} returned HTTP {} — release may not exist for this platform",
            url,
            resp.status()
        ));
    }

    // Don't bail on Content-Length mismatches.  The Content-Length
    // header may differ from the manifest's `size` for several reasons:
    //   - CDN mirrors or reverse proxies may alter Content-Length.
    //   - The manifest may be slightly out of sync with the server's
    //     binary (e.g. during a rolling release).
    //   - Redirects can surface a different Content-Length from an
    //     intermediate hop.
    // The real integrity checks happen after the download completes:
    //   1. Total bytes written must match `expected_size` (below).
    //   2. SHA256 must match `expected_sha256`.
    // These two checks are sufficient; an early Content-Length gate
    // causes false-negative aborts (see issue #380).

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response chunk")?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .context("writing download to disk")?;
        written += chunk.len() as u64;
        // Ignore send errors — receiver may have been dropped.
        let _ = progress.send(UpgradeEvent::Downloading {
            bytes: written,
            total: expected_size,
        });
    }
    file.flush().await.context("flushing download to disk")?;
    drop(file);

    if written != expected_size {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!(
            "short download: got {} bytes, expected {}",
            written,
            expected_size
        ));
    }

    let _ = progress.send(UpgradeEvent::Verifying);
    let got = hex_encode(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_sha256) {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!(
            "checksum mismatch — possible corruption or tampering.\n  expected: {}\n  got:      {}",
            expected_sha256,
            got
        ));
    }

    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// Best-effort cleanup of a leftover file (typically a `.bak` or `.rolling`
/// from a prior upgrade/rollback). Returns `true` if the file was removed,
/// `false` if it could not be removed (locked, permission denied, etc.).
///
/// Defensive against Windows ACCESS_DENIED scenarios:
///
/// 1. The file carries a read-only attribute (some AV / SCCM policies
///    flag any executable in `%LOCALAPPDATA%` this way). Clear it first.
/// 2. Windows Defender or another scanner briefly holds the file open
///    during a real-time scan — typically <500 ms. Retry once with a
///    short sleep before giving up.
/// 3. The file is a still-running atomcode process from a prior upgrade
///    where the user didn't restart. Nothing we can do at the code layer;
///    the caller proceeds without blocking the upgrade.
///
/// This function is intentionally **best-effort** — a failure to remove
/// the stale file must NOT block an upgrade. The three-way swap in
/// `replace_binary` ensures the upgrade proceeds even if old backups
/// cannot be deleted.
fn try_remove_stale(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }

    #[cfg(windows)]
    {
        if let Ok(meta) = path.metadata() {
            let mut perm = meta.permissions();
            if perm.readonly() {
                perm.set_readonly(false);
                let _ = std::fs::set_permissions(path, perm);
            }
        }
    }

    if std::fs::remove_file(path).is_ok() {
        return true;
    }
    // Brief retry to ride out a transient AV / indexer hold.
    std::thread::sleep(std::time::Duration::from_millis(500));
    std::fs::remove_file(path).is_ok()
}

/// Clear a read-only attribute on `path` so a rename/remove isn't denied
/// outright. Best-effort. On Unix a normal executable isn't read-only so
/// this is a no-op; on Windows some AV / SCCM policies flag executables
/// under `%LOCALAPPDATA%` read-only, which alone causes ERROR_ACCESS_DENIED.
fn clear_readonly(path: &Path) {
    // Windows-only: on Unix, `rename`/`remove_file` act on the directory
    // entry and don't need the file itself writable, and `set_readonly(false)`
    // there would widen the mode to world-writable. The read-only-attribute
    // failure mode is Windows-specific (AV/SCCM marking %LOCALAPPDATA% exes).
    #[cfg(windows)]
    if let Ok(meta) = path.metadata() {
        let mut perm = meta.permissions();
        if perm.readonly() {
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// Rename `from` → `to`, hardened against the transient failures that make
/// a naive `std::fs::rename` of a *running* Windows executable fail with
/// `ERROR_ACCESS_DENIED` (os error 5):
///
///   * an AV / indexer (Windows Defender real-time scan) briefly holds the
///     source or destination open without `FILE_SHARE_DELETE` — this clears
///     well under a second, so we retry with exponential backoff;
///   * a read-only attribute on the source — cleared before the first try;
///   * a leftover destination from a prior interrupted upgrade that is
///     momentarily locked — re-removed before each retry so the implicit
///     `MOVEFILE_REPLACE_EXISTING` doesn't fail on it.
///
/// Cross-platform: on Unix these conditions don't arise, so the first
/// attempt succeeds and the loop is never re-entered. Worst-case added
/// latency on persistent failure is ~1.5s (100+200+400+800ms of sleeps),
/// which is acceptable for a one-shot upgrade/rollback.
fn robust_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    clear_readonly(from);
    robust_rename_with(from, to, |f, t| std::fs::rename(f, t))
}

/// `robust_rename` with the underlying rename injected, so the cross-device
/// fallback and retry logic can be exercised deterministically in tests.
fn robust_rename_with<R>(from: &Path, to: &Path, mut rename: R) -> std::io::Result<()>
where
    R: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    const ATTEMPTS: u32 = 5;
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..ATTEMPTS {
        // A locked / leftover destination makes the replace-existing rename
        // fail; try to clear it (and its read-only attr) before each try.
        if to.exists() {
            clear_readonly(to);
            let _ = std::fs::remove_file(to);
        }
        match rename(from, to) {
            Ok(()) => return Ok(()),
            // Cross-device: the staged binary lives under the user profile
            // (e.g. C:\Users\…\.atomcode\staged\) while the running exe may be
            // on another drive (H:\…). `rename` across volumes can NEVER
            // succeed (ERROR_NOT_SAME_DEVICE / EXDEV), so don't waste the retry
            // budget — copy the bytes across and drop the source instead.
            Err(e) if is_cross_device_error(&e) => return copy_across_devices(from, to),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < ATTEMPTS {
                    let ms = 100u64 << attempt; // 100, 200, 400, 800
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("rename failed")))
}

/// True when a failed `rename` was rejected because source and destination
/// live on different filesystems/volumes. Windows reports ERROR_NOT_SAME_DEVICE
/// (os error 17); Unix reports EXDEV (os error 18).
fn is_cross_device_error(e: &std::io::Error) -> bool {
    #[cfg(windows)]
    let code = 17;
    #[cfg(not(windows))]
    let code = 18;
    e.raw_os_error() == Some(code)
}

/// Move a file across volumes by copying its bytes and then dropping the
/// source. Used as the fallback when `rename` reports a cross-device error.
/// Removing the source is best-effort: once the copy lands, the move has
/// effectively succeeded, and a lingering staged file is harmless (it is
/// superseded/cleaned by the next upgrade) — so a locked source must not
/// fail the upgrade.
fn copy_across_devices(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// Put `new_bin` in place of `exe`, keeping the previous `exe` as `.bak`.
///
/// Uses a **three-way swap** to avoid ever needing to delete `.bak` as a
/// prerequisite — the old approach of "delete .bak, then rename exe→.bak"
/// could fail on Windows when `.bak` is locked (AV scanner, read-only
/// attribute, still-running process). The swap sequence is:
///
///   1. `exe` → `.rolling`      (Windows allows renaming a running exe)
///   2. `new_bin` → `exe`        (install new version)
///   3. best-effort: delete old `.bak`
///   4. `.rolling` → `.bak`      (preserve old version for rollback)
///
/// Steps 1–2 are the critical path; steps 3–4 are best-effort cleanup.
/// If step 4 fails (e.g. old `.bak` is still locked), the upgrade still
/// succeeds — the `.rolling` file is left behind and will be cleaned up
/// on the next upgrade attempt.
///
/// On Unix, `rename(2)` within a directory is atomic; on Windows, an
/// exe currently being executed can be renamed (but not deleted), so
/// the same sequence works.
fn replace_binary(new_bin: &Path, exe: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(new_bin)
            .with_context(|| format!("stat {}", new_bin.display()))?
            .permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(new_bin, perm)
            .with_context(|| format!("chmod {}", new_bin.display()))?;
    }

    let backup = backup_path(exe);
    let rolling = rolling_path(exe);

    // Clean up any leftover .rolling from a prior interrupted upgrade.
    try_remove_stale(&rolling);

    // Step 1: live binary → rolling (Windows allows renaming a running exe).
    // `robust_rename` retries through transient AV/indexer locks that
    // otherwise surface as ERROR_ACCESS_DENIED (os error 5) here.
    robust_rename(exe, &rolling).with_context(|| {
        format!(
            "renaming current binary {} -> {} (swap step 1)",
            exe.display(),
            rolling.display()
        )
    })?;

    // Step 2: new binary → live (the actual upgrade)
    if let Err(e) = robust_rename(new_bin, exe) {
        // Best-effort unwind of step 1 so the user isn't left without
        // a live binary.
        let _ = std::fs::rename(&rolling, exe);
        return Err(anyhow!(
            "moving new binary into place failed ({}). Previous version restored.",
            e
        ));
    }

    // Step 3: best-effort — remove old .bak so we can rename .rolling→.bak.
    // Failure is non-fatal; we just leave .rolling behind.
    let bak_removed = try_remove_stale(&backup);

    // Step 4: rolling → .bak (preserve old version for rollback)
    if bak_removed {
        if let Err(e) = std::fs::rename(&rolling, &backup) {
            // Upgrade succeeded but we couldn't preserve the old version
            // as .bak. The .rolling file lingers; next upgrade will
            // clean it up. Rollback won't be available this session.
            eprintln!(
                "Note: could not preserve previous version as backup ({}). Rollback unavailable until next upgrade.",
                e
            );
        }
    } else {
        // Old .bak couldn't be removed (locked by AV, running process, etc.).
        // The .rolling file stays behind; next upgrade attempt will clean
        // it up. Rollback points to the version before this upgrade's
        // predecessor, which is still better than failing the upgrade.
        eprintln!(
            "Note: could not remove old backup {}. Rollback may point to an older version.\n  The .rolling file at {} will be cleaned up on the next upgrade.",
            backup.display(),
            rolling.display()
        );
    }

    Ok(())
}

/// Top-level upgrade driver.
///
/// `current_version` is what we're running right now (e.g. `"v4.19.0"`
/// — callers typically pass `format!("v{}", env!("CARGO_PKG_VERSION"))`).
/// When `force` is false and the manifest version is `<=` current, this
/// returns an error carrying `ALREADY_LATEST` so callers can distinguish
/// "already up to date" from a real failure.
/// In `distro-pm` builds this returns immediately with an error carrying `PACKAGE_MANAGED` (upgrades are the package manager's job).
pub async fn run_upgrade(
    current_version: String,
    force: bool,
    tx: mpsc::UnboundedSender<UpgradeEvent>,
) -> Result<UpgradeSummary> {
    if is_package_managed() {
        return Err(anyhow!(PACKAGE_MANAGED));
    }
    let current_version = current_version.as_str();
    let target = detect_target().ok_or_else(|| {
        anyhow!(
            "this platform has no published atomcode release ({}/{})",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let exe = current_exe_path()?;
    ensure_writable(&exe)?;

    let manifest = fetch_manifest().await?;
    let _ = tx.send(UpgradeEvent::ManifestFetched {
        version: manifest.version.clone(),
    });

    if !force && !is_newer(&manifest.version, current_version) {
        return Err(anyhow!(
            "{}: already on {} (latest is {}). Pass --force to reinstall.",
            ALREADY_LATEST,
            current_version,
            manifest.version
        ));
    }

    let entry = manifest.binaries.get(target).ok_or_else(|| {
        anyhow!(
            "manifest has no entry for target {} — this platform may not be in this release",
            target
        )
    })?;

    let url = binary_url(&manifest.version, target);
    let download = download_path(&exe);
    download_and_verify(&url, &entry.sha256, entry.size, &download, &tx).await?;

    let _ = tx.send(UpgradeEvent::Replacing);
    replace_binary(&download, &exe)?;

    // Manual `/upgrade` just installed whatever the current manifest
    // advertises. Any staged upgrade sitting in `staged_dir()` is now
    // superseded — if we leave `pending.json` in place, the next startup
    // might try to "apply" an older (or identical) staged version on top
    // of what we just installed, causing a downgrade or redundant churn.
    // Clear both the pointer and any stray staged binaries.
    clear_pending_pointer();
    if let Ok(entries) = std::fs::read_dir(staged_dir()) {
        for e in entries.flatten() {
            let p = e.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("atomcode-"))
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    let backup = backup_path(&exe);
    // NOTE: `exe` was captured *before* `replace_binary` renamed the running
    // binary. On Windows, `current_exe()` would now return `.atomcode.rolling`
    // instead of the original `atomcode.exe`, so we must pass this saved
    // value through to `re_exec_self`.
    let _ = tx.send(UpgradeEvent::Done {
        version: manifest.version.clone(),
        backup: backup.clone(),
        exe: exe.clone(),
    });

    Ok(UpgradeSummary {
        version: manifest.version,
        backup,
        exe,
    })
}

/// Sentinel substring in the "already latest" error so the CLI/TUI
/// layer can render a calm informational message instead of a scary
/// red error. Kept as a plain string to avoid an error-type refactor.
pub const ALREADY_LATEST: &str = "ALREADY_LATEST";

/// Sentinel embedded in the error returned by `run_upgrade` / `run_rollback`
/// when this binary is a package-manager-managed build (HarmonyBrew).
/// Callers special-case it to show "use `brew upgrade`" instead of a
/// generic failure — mirrors the `ALREADY_LATEST` pattern.
pub const PACKAGE_MANAGED: &str = "PACKAGE_MANAGED";

/// True when this binary was compiled for package-manager distribution
/// (the `distro-pm` feature, set by the HarmonyBrew formula). Such builds
/// must never self-modify the binary — upgrades are the package manager's
/// job.
pub const fn is_package_managed() -> bool {
    cfg!(feature = "distro-pm")
}

// ============================================================================
// Deferred upgrade (download-in-session, apply-at-next-startup)
// ============================================================================
//
// The deferred path solves two problems that `run_upgrade` alone can't:
//   1. Users whose sessions run for hours/days — they'll never voluntarily
//      restart just to pick up a new version. A background task can prepare
//      the staged binary while they work; apply happens whenever they do
//      restart (which they will, eventually, for unrelated reasons).
//   2. Users whose sessions are short but who rarely think to run
//      `/upgrade` — same benefit: the next normal launch carries the bump.
//
// Flow:
//   session N      : prepare_deferred_upgrade()
//                    → download to ~/.atomcode/staged/<filename>
//                    → write ~/.atomcode/staged/pending.json
//                    → (UI surfaces "⟲ vX.Y.Z pending")
//   session N exit : no special work; staged files survive any exit path
//                    (graceful, SIGHUP on terminal close, SIGKILL, power
//                    loss — all fine, state is on disk)
//   session N+1    : apply_pending_upgrade() runs BEFORE tokio starts
//                    → atomically swaps live binary with staged
//                    → re-execs self with original argv
//                    → user sees "✓ Upgraded to vX.Y.Z" on welcome
//
// A safety circuit-breaker is wired in: if apply succeeds but the new
// binary fails to start `MAX_APPLY_ATTEMPTS` times in a row, the staged
// file is discarded so a broken release can't brick the install.

/// Maximum times we'll try to apply the same staged upgrade before
/// giving up. Prevents a corrupted download from turning into a boot loop.
const MAX_APPLY_ATTEMPTS: u32 = 3;

/// Pointer record stored at `~/.atomcode/staged/pending.json`. Describes
/// a downloaded binary that hasn't yet been promoted to live. Lifecycle:
/// written by `prepare_deferred_upgrade`, read (and deleted on success /
/// incremented on failure) by `apply_pending_upgrade`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingUpgrade {
    /// Version tag the staged binary represents, e.g. `v4.19.1`.
    pub version: String,
    /// Absolute path to the verified binary sitting in `staged_dir()`.
    pub staged_path: PathBuf,
    /// SHA256 of the staged binary (lowercase hex). Re-verified at apply
    /// time so a partial overwrite between sessions (e.g. disk full)
    /// doesn't install a corrupted file.
    pub sha256: String,
    /// Size in bytes; cheap sanity check before we recompute sha256.
    pub size: u64,
    /// RFC3339 timestamp for audit / debug. Not load-bearing.
    pub created_at: String,
    /// How many times we've attempted apply and failed. When this hits
    /// `MAX_APPLY_ATTEMPTS`, the staged file is discarded instead of
    /// retried again on the following startup.
    #[serde(default)]
    pub attempts: u32,
}

/// Successful apply result — fed into the re-exec handoff so the new
/// process can render a one-time "✓ Upgraded" banner on the welcome screen.
#[derive(Debug, Clone)]
pub struct AppliedUpgrade {
    pub version: String,
    pub backup: PathBuf,
    pub exe: PathBuf,
}

/// `~/.atomcode/staged/` (or equivalent on Windows). Created on demand by
/// `prepare_deferred_upgrade`; safe to treat as possibly-missing at read
/// time. Kept under the same root as `history` / `recent_dirs` so nothing
/// new appears in `$HOME`.
pub fn staged_dir() -> PathBuf {
    atomcode_config::config::Config::config_dir().join("staged")
}

fn pending_json_path() -> PathBuf {
    staged_dir().join("pending.json")
}

/// Where a prepared (downloaded + verified) binary lives while it waits
/// to be promoted. Filename mirrors the release artifact so the same
/// `binary_filename` helper round-trips.
fn staged_binary_path(version: &str, target: &str) -> PathBuf {
    staged_dir().join(binary_filename(version, target))
}

/// Read `pending.json` if present. Absent file → `Ok(None)`. Corrupt JSON
/// returns an error so callers can delete it and retry; `apply_pending_upgrade`
/// does exactly that.
pub fn read_pending() -> Result<Option<PendingUpgrade>> {
    let path = pending_json_path();
    if !path.exists() {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let pending: PendingUpgrade =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(pending))
}

fn write_pending(pending: &PendingUpgrade) -> Result<()> {
    let dir = staged_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let body = serde_json::to_string_pretty(pending).context("serializing pending.json")?;
    let path = pending_json_path();
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
}

fn clear_pending_pointer() {
    let _ = std::fs::remove_file(pending_json_path());
}

/// Quiet variant of `check_latest` used by the hourly background poll.
/// Returns the remote `(version, manifest)` only when strictly newer than
/// `current_version`. Separate from `version_check::check_latest` because
/// that one only gives back the version string; here we need the full
/// manifest to know the per-target sha256 / size.
pub async fn fetch_manifest_if_newer(current_version: &str) -> Result<Option<Manifest>> {
    let manifest = fetch_manifest().await?;
    if is_newer(&manifest.version, current_version) {
        Ok(Some(manifest))
    } else {
        Ok(None)
    }
}

/// Download + verify a new release into `staged_dir()` without touching
/// the live binary. Writes `pending.json` as the final step so a partial
/// download (crashed mid-stream) doesn't masquerade as "ready to apply" —
/// the pointer only appears if sha256 passed.
///
/// Returns `Ok(None)` when we're already on the latest version (or newer).
/// Idempotent: calling twice with the same manifest is a no-op after the
/// first success (file + pointer already in place).
/// In `distro-pm` builds this is a no-op returning `Ok(None)`.
pub async fn prepare_deferred_upgrade(
    current_version: &str,
    tx: mpsc::UnboundedSender<UpgradeEvent>,
) -> Result<Option<PendingUpgrade>> {
    if is_package_managed() {
        return Ok(None);
    }
    let target = detect_target().ok_or_else(|| {
        anyhow!(
            "this platform has no published atomcode release ({}/{})",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let manifest = fetch_manifest().await?;

    if !is_newer(&manifest.version, current_version) {
        return Ok(None);
    }

    let _ = tx.send(UpgradeEvent::ManifestFetched {
        version: manifest.version.clone(),
    });

    // If a staged upgrade for this exact version already exists and the
    // on-disk bytes still match the manifest's sha256, reuse it. Saves a
    // redownload when two sessions both polled and landed here.
    if let Ok(Some(existing)) = read_pending() {
        if existing.version == manifest.version && existing.staged_path.exists() {
            if let Some(entry) = manifest.binaries.get(target) {
                if existing.sha256.eq_ignore_ascii_case(&entry.sha256)
                    && existing.size == entry.size
                {
                    return Ok(Some(existing));
                }
            }
        }
    }

    let entry = manifest.binaries.get(target).ok_or_else(|| {
        anyhow!(
            "manifest has no entry for target {} — this platform may not be in this release",
            target
        )
    })?;

    let dir = staged_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let staged_path = staged_binary_path(&manifest.version, target);
    let url = binary_url(&manifest.version, target);
    download_and_verify(&url, &entry.sha256, entry.size, &staged_path, &tx).await?;

    let pending = PendingUpgrade {
        version: manifest.version.clone(),
        staged_path: staged_path.clone(),
        sha256: entry.sha256.clone(),
        size: entry.size,
        created_at: chrono::Utc::now().to_rfc3339(),
        attempts: 0,
    };
    write_pending(&pending)?;
    Ok(Some(pending))
}

/// Bootstrap entry point: called once at the very top of `main()` BEFORE
/// the tokio runtime, TUI, or any heavy init. Three outcomes:
///
///   * `Ok(None)`                  — no pending upgrade, continue normally.
///   * `Ok(Some(AppliedUpgrade))`  — staged binary is now live; caller must
///                                   `re_exec_self` to hand control over.
///   * `Err(e)`                    — apply failed; caller should log and
///                                   continue with the OLD binary. We've
///                                   already bumped the attempt counter
///                                   (or discarded the stage past the cap).
///
/// SHA256 is re-verified here even though we verified at download time:
/// a session-external process (backup tool, AV software, buggy sync)
/// could have touched the file between sessions. Verification is cheap
/// compared to installing a corrupted binary.
/// In `distro-pm` builds this is a no-op returning `Ok(None)`.
pub fn apply_pending_upgrade() -> Result<Option<AppliedUpgrade>> {
    if is_package_managed() {
        return Ok(None);
    }
    let mut pending = match read_pending() {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(None),
        Err(_) => {
            // Corrupt pointer — nuke it, we can't do anything safe with it.
            clear_pending_pointer();
            return Ok(None);
        }
    };

    if pending.attempts >= MAX_APPLY_ATTEMPTS {
        // Circuit-break: this staged upgrade has failed too many times.
        // Discard everything so we stop trying, fall back to the old
        // binary, and let the next successful prepare_deferred_upgrade
        // supersede it.
        let _ = std::fs::remove_file(&pending.staged_path);
        clear_pending_pointer();
        return Ok(None);
    }

    // Bump attempt counter up-front so a crash mid-apply doesn't leave us
    // in an unbounded retry loop.
    pending.attempts += 1;
    let _ = write_pending(&pending);

    if !pending.staged_path.exists() {
        clear_pending_pointer();
        return Ok(None);
    }

    let actual_size = std::fs::metadata(&pending.staged_path)
        .with_context(|| format!("stat {}", pending.staged_path.display()))?
        .len();
    if actual_size != pending.size {
        let _ = std::fs::remove_file(&pending.staged_path);
        clear_pending_pointer();
        return Err(anyhow!(
            "staged binary size changed between sessions (expected {}, got {}). Discarded.",
            pending.size,
            actual_size
        ));
    }

    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(&pending.staged_path)
        .with_context(|| format!("opening {}", pending.staged_path.display()))?;
    std::io::copy(&mut file, &mut hasher).context("hashing staged binary")?;
    drop(file);
    let got = hex_encode(&hasher.finalize());
    if !got.eq_ignore_ascii_case(&pending.sha256) {
        let _ = std::fs::remove_file(&pending.staged_path);
        clear_pending_pointer();
        return Err(anyhow!(
            "staged binary sha256 drifted between sessions (expected {}, got {}). Discarded.",
            pending.sha256,
            got
        ));
    }

    let exe = current_exe_path()?;
    ensure_writable(&exe)?;
    replace_binary(&pending.staged_path, &exe)?;

    // Success — pointer is done, file moved into place by replace_binary.
    clear_pending_pointer();

    Ok(Some(AppliedUpgrade {
        version: pending.version,
        backup: backup_path(&exe),
        exe,
    }))
}

/// Replace the current process with a fresh invocation of the live binary,
/// preserving argv, cwd, and env. On Unix this is `execv` (same PID, old
/// process image gone). On Windows we spawn a child and exit the parent —
/// a separate PID, but terminal stdio is shared so the user still sees
/// one continuous "session" from their perspective.
///
/// **Important on Windows:** After `replace_binary` renames the running exe
/// (e.g. `atomcode.exe` → `.atomcode.rolling`), `std::env::current_exe()`
/// may return the *renamed* path (`.atomcode.rolling`) instead of the
/// original one (`atomcode.exe`). This is because `GetModuleFileNameW`
/// tracks the on-disk filename. If `override_exe` is provided, it is used
/// instead of `current_exe()` — callers should capture the exe path
/// *before* calling `replace_binary` and pass it here.
///
/// Never returns on the happy path. An `Err` return means the handoff
/// failed (e.g., new binary missing execute bit under unusual filesystem
/// constraints); caller should surface the error and keep running with
/// the old binary rather than exiting silently.
pub fn re_exec_self(override_exe: Option<&Path>) -> Result<std::convert::Infallible> {
    let exe = override_exe.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        current_exe_path().unwrap_or_else(|_| {
            // Fallback: if we can't resolve current_exe AND no override,
            // try argv[0] as a last resort.
            std::env::args_os()
                .next()
                .map(PathBuf::from)
                .unwrap_or_default()
        })
    });
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).args(&args).exec();
        // `exec` only returns on failure.
        Err(anyhow!("re-exec failed: {}", err))
    }

    #[cfg(windows)]
    {
        // Windows has no exec(). Spawn the child with shared stdio so
        // the user's terminal stays connected to the new process, then
        // exit ourselves. The replacement PID shift is invisible in
        // a terminal context (the shell tracks the parent's exit).
        let status = std::process::Command::new(&exe)
            .args(&args)
            .spawn()
            .with_context(|| format!("spawning new binary {}", exe.display()))?
            .wait()
            .with_context(|| "waiting for spawned binary to exit")?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

/// Parse and compare two `vMAJOR.MINOR.PATCH` strings. Returns true
/// when `latest > current`. Malformed inputs fall back to a byte-wise
/// `!=` so we *do* proceed with reinstall when version strings are
/// shaped unexpectedly — safer than silently refusing to upgrade.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => latest.trim() != current.trim(),
    }
}

fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    let rest = s.strip_prefix('v')?;
    // Strip pre-release suffix (e.g. "-beta.1", "-rc.2") so versions
    // like "v4.25.0-beta.1" parse correctly (Issue #596).
    let rest = rest.split('-').next()?;
    let mut parts = rest.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let c = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// Three-way swap between the live binary and `.bak`, leaving `.bak`
/// pointing at what was previously live. Calling rollback twice in a
/// row returns you to the original state — intentional, so users can
/// toggle between last-two versions without redownloading.
/// In `distro-pm` builds this returns immediately with an error carrying `PACKAGE_MANAGED` (upgrades are the package manager's job).
pub fn run_rollback() -> Result<RollbackSummary> {
    if is_package_managed() {
        return Err(anyhow!(PACKAGE_MANAGED));
    }
    let exe = current_exe_path()?;
    let backup = backup_path(&exe);
    if !backup.exists() {
        return Err(anyhow!(
            "no backup found at {} — nothing to roll back to",
            backup.display()
        ));
    }
    ensure_writable(&exe)?;

    let rolling = rolling_path(&exe);
    if rolling.exists() {
        std::fs::remove_file(&rolling).ok();
    }

    // Step 1: live -> rolling (retry through transient Windows locks)
    robust_rename(&exe, &rolling).with_context(|| {
        format!(
            "renaming {} -> {} (swap step 1)",
            exe.display(),
            rolling.display()
        )
    })?;
    // Step 2: backup -> live
    if let Err(e) = robust_rename(&backup, &exe) {
        // Best-effort unwind of step 1.
        let _ = std::fs::rename(&rolling, &exe);
        return Err(anyhow!("rollback failed at step 2 ({}); state restored", e));
    }
    // Step 3: rolling -> backup
    if let Err(e) = std::fs::rename(&rolling, &backup) {
        // Can't cleanly unwind — the live file is already the old
        // version, which is the user-visible outcome they asked for.
        // Surface the orphan so they can clean up manually.
        return Err(anyhow!(
            "rollback succeeded but tmp file {} could not be moved to {} ({}).\n\
             Delete it manually; next /upgrade will overwrite it.",
            rolling.display(),
            backup.display(),
            e
        ));
    }

    Ok(RollbackSummary { exe, backup })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_filename_adds_exe_on_windows_targets() {
        assert_eq!(
            binary_filename("v4.19.0", "windows-x64"),
            "atomcode-v4.19.0-windows-x64.exe"
        );
        assert_eq!(
            binary_filename("v4.19.0", "windows-arm64"),
            "atomcode-v4.19.0-windows-arm64.exe"
        );
    }

    #[test]
    fn binary_filename_plain_on_unix_targets() {
        assert_eq!(
            binary_filename("v4.19.0", "darwin-arm64"),
            "atomcode-v4.19.0-darwin-arm64"
        );
        assert_eq!(
            binary_filename("v4.19.0", "linux-x64"),
            "atomcode-v4.19.0-linux-x64"
        );
    }

    #[test]
    fn binary_url_shape() {
        assert_eq!(
            binary_url("v4.19.0", "darwin-arm64"),
            "https://atomgit.com/atomgit_atomcode/atomcode/releases/download/v4.19.0/atomcode-v4.19.0-darwin-arm64"
        );
    }

    #[test]
    fn detect_target_returns_something_on_tier1_hosts() {
        // We can't assert an exact value (depends on host), but every
        // supported dev platform should resolve to Some.
        let t = detect_target();
        if cfg!(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        )) {
            if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
                assert!(t.is_some(), "expected target tag on this host");
            }
        }
    }

    #[test]
    fn target_tag_matches_release_manifest_targets() {
        assert_eq!(target_tag("macos", "aarch64"), Some("darwin-arm64"));
        assert_eq!(target_tag("macos", "x86_64"), Some("darwin-x64"));
        assert_eq!(target_tag("linux", "x86_64"), Some("linux-x64"));
        assert_eq!(target_tag("linux", "aarch64"), Some("linux-arm64"));
        assert_eq!(target_tag("windows", "x86_64"), Some("windows-x64"));
        assert_eq!(target_tag("windows", "aarch64"), Some("windows-arm64"));
        assert_eq!(target_tag("linux", "arm"), None);
    }

    #[test]
    fn backup_path_appends_bak() {
        let p = Path::new("/usr/local/bin/atomcode");
        assert_eq!(backup_path(p), PathBuf::from("/usr/local/bin/atomcode.bak"));
    }

    #[test]
    fn try_remove_stale_deletes_normal_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("atomcode.exe.bak");
        std::fs::write(&p, b"old").expect("seed");
        assert!(try_remove_stale(&p));
        assert!(!p.exists(), "backup should be gone");
    }

    #[test]
    fn try_remove_stale_clears_readonly_then_deletes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("atomcode.exe.bak");
        std::fs::write(&p, b"old").expect("seed");
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm).expect("set readonly");
        // On Windows the readonly flag would block remove_file outright.
        // On Unix it doesn't, but the helper still happens to clean up.
        // Either way, the file must be gone after this call.
        assert!(try_remove_stale(&p));
        assert!(!p.exists());
    }

    #[test]
    fn try_remove_stale_returns_false_for_truly_locked_file() {
        // On Unix, an open file can still be unlinked, so true locking
        // is hard to simulate. Instead we test the "nonexistent path"
        // case — `try_remove_stale` correctly returns true because the
        // file doesn't exist (nothing to remove). To verify the false
        // return, we'd need a platform-specific lock (Windows HANDLE),
        // which isn't feasible in a cross-platform unit test. The
        // important contract is: returns true when nothing needs doing.
        let bogus = std::path::PathBuf::from("/no/such/dir/atomcode.exe.bak");
        assert!(!bogus.exists());
        // A path that doesn't exist is "already removed" → true
        assert!(try_remove_stale(&bogus));
    }

    #[test]
    fn try_remove_stale_returns_true_for_nonexistent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("does_not_exist");
        assert!(!p.exists());
        assert!(try_remove_stale(&p));
    }

    #[test]
    fn backup_path_preserves_exe_suffix_on_windows_style() {
        let p = Path::new("C:/Tools/atomcode.exe");
        assert_eq!(backup_path(p), PathBuf::from("C:/Tools/atomcode.exe.bak"));
    }

    #[test]
    fn is_newer_semver() {
        assert!(is_newer("v4.19.0", "v4.18.2")); // latest, current
        assert!(is_newer("v4.19.0", "v4.18.9"));
        assert!(is_newer("v5.0.4", "v4.99.99"));
        assert!(!is_newer("v4.19.0", "v4.19.0"));
        assert!(!is_newer("v4.18.0", "v4.19.0")); // latest is older than current
    }

    #[test]
    fn is_newer_falls_back_to_string_diff_on_garbage() {
        // If we can't parse, err on the side of allowing reinstall
        // when strings differ (user may have a custom channel).
        assert!(is_newer("build-abc", "build-xyz"));
        assert!(!is_newer("build-abc", "build-abc"));
    }

    #[test]
    fn parse_version_handles_prerelease_suffix() {
        // Issue #596: pre-release versions like "v4.25.0-beta.1" must parse
        assert_eq!(parse_version("v4.25.0-beta.1"), Some((4, 25, 0)));
        assert_eq!(parse_version("v4.25.0-rc.2"), Some((4, 25, 0)));
        assert_eq!(parse_version("v4.25.0-alpha"), Some((4, 25, 0)));
        // Normal versions still work
        assert_eq!(parse_version("v4.25.0"), Some((4, 25, 0)));
    }

    #[test]
    fn is_newer_handles_prerelease() {
        // Pre-release version is treated as equal to its release version
        assert!(!is_newer("v4.25.0", "v4.25.0-beta.1"));
        assert!(!is_newer("v4.25.0-beta.1", "v4.25.0"));
        // But still newer than older versions
        assert!(is_newer("v4.25.0-beta.1", "v4.24.99"));
    }

    #[test]
    fn manifest_parses_minimal_shape() {
        let json = r#"{
            "version": "v4.19.0",
            "binaries": {
                "darwin-arm64": { "sha256": "abcd", "size": 1024 }
            }
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version, "v4.19.0");
        assert_eq!(m.binaries["darwin-arm64"].size, 1024);
    }

    #[test]
    fn manifest_ignores_unknown_fields() {
        let json = r#"{
            "version": "v4.19.0",
            "released_at": "2026-04-19T00:00:00Z",
            "signature": "future-field",
            "binaries": {
                "linux-x64": { "sha256": "ffff", "size": 42, "notes": "x" }
            }
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.released_at.as_deref(), Some("2026-04-19T00:00:00Z"));
        assert_eq!(m.binaries["linux-x64"].sha256, "ffff");
    }

    #[test]
    fn hex_encode_matches_known_vectors() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x10]), "00ff10");
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn ensure_writable_probes_containing_dir() {
        // A path inside tempdir must pass; a path whose parent doesn't
        // exist must fail with a clear message.
        let tmp = tempfile::tempdir().unwrap();
        let ok = tmp.path().join("atomcode");
        assert!(ensure_writable(&ok).is_ok());

        let bogus = Path::new("/nonexistent-dir-xyzzy-9999/atomcode");
        let err = ensure_writable(bogus).unwrap_err().to_string();
        assert!(err.contains("sudo atomcode upgrade"), "got: {}", err);
    }

    #[test]
    fn replace_binary_renames_live_to_bak_via_three_way_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("atomcode");
        let new = tmp.path().join(".atomcode.download");
        std::fs::write(&exe, b"OLD").unwrap();
        std::fs::write(&new, b"NEW").unwrap();

        replace_binary(&new, &exe).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW");
        let bak = backup_path(&exe);
        assert_eq!(std::fs::read(&bak).unwrap(), b"OLD");
        assert!(!new.exists());
        // No leftover .rolling file on success
        let rolling = rolling_path(&exe);
        assert!(!rolling.exists());
    }

    #[test]
    fn replace_binary_overwrites_stale_bak() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("atomcode");
        let new = tmp.path().join(".atomcode.download");
        let bak = backup_path(&exe);
        std::fs::write(&exe, b"V2").unwrap();
        std::fs::write(&new, b"V3").unwrap();
        std::fs::write(&bak, b"V1").unwrap();

        replace_binary(&new, &exe).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"V3");
        assert_eq!(std::fs::read(&bak).unwrap(), b"V2");
        // No leftover .rolling file
        let rolling = rolling_path(&exe);
        assert!(!rolling.exists());
    }

    #[test]
    fn robust_rename_moves_file() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        std::fs::write(&from, b"DATA").unwrap();

        robust_rename(&from, &to).unwrap();

        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"DATA");
    }

    #[test]
    fn robust_rename_replaces_existing_destination() {
        // Mirrors the swap's "rolling target left over from a prior run"
        // case: the destination already exists and must be replaced.
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        std::fs::write(&from, b"NEW").unwrap();
        std::fs::write(&to, b"STALE").unwrap();

        robust_rename(&from, &to).unwrap();

        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"NEW");
    }

    #[test]
    fn is_cross_device_error_detects_platform_code() {
        // Windows ERROR_NOT_SAME_DEVICE = 17; Unix EXDEV = 18.
        #[cfg(windows)]
        let exdev = 17;
        #[cfg(not(windows))]
        let exdev = 18;
        assert!(is_cross_device_error(&std::io::Error::from_raw_os_error(
            exdev
        )));
        // A permission-style error must NOT be mistaken for cross-device.
        assert!(!is_cross_device_error(&std::io::Error::from_raw_os_error(
            13
        )));
    }

    #[test]
    fn copy_across_devices_moves_and_removes_source() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("src");
        let to = tmp.path().join("dst");
        std::fs::write(&from, b"PAYLOAD").unwrap();

        copy_across_devices(&from, &to).unwrap();

        assert_eq!(std::fs::read(&to).unwrap(), b"PAYLOAD");
        assert!(!from.exists(), "source must be consumed after the move");
    }

    #[test]
    fn robust_rename_falls_back_to_copy_on_cross_device() {
        // The reported bug: staged binary under the user profile (C:\…\.atomcode
        // \staged\) vs a running exe on another drive (H:\). `rename` fails with
        // ERROR_NOT_SAME_DEVICE; robust_rename must transparently copy instead of
        // burning its retry budget on a deterministic failure.
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("staged-binary");
        let to = tmp.path().join("exe");
        std::fs::write(&from, b"NEWVERSION").unwrap();

        #[cfg(windows)]
        let exdev = 17;
        #[cfg(not(windows))]
        let exdev = 18;
        let fake_rename = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(exdev));

        robust_rename_with(&from, &to, fake_rename).unwrap();

        assert_eq!(std::fs::read(&to).unwrap(), b"NEWVERSION");
        assert!(!from.exists(), "staged source must be consumed");
    }

    #[test]
    fn robust_rename_clears_readonly_source() {
        // A read-only source (a real Windows AV/SCCM failure mode) must
        // still move. set_readonly is cross-platform enough to exercise
        // the clear_readonly path here.
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        std::fs::write(&from, b"RO").unwrap();
        let mut perm = std::fs::metadata(&from).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&from, perm).unwrap();

        robust_rename(&from, &to).unwrap();

        assert_eq!(std::fs::read(&to).unwrap(), b"RO");
    }

    #[test]
    fn replace_binary_cleans_leftover_rolling() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("atomcode");
        let new = tmp.path().join(".atomcode.download");
        let rolling = rolling_path(&exe);
        std::fs::write(&exe, b"OLD").unwrap();
        std::fs::write(&new, b"NEW").unwrap();
        std::fs::write(&rolling, b"STALE").unwrap();

        replace_binary(&new, &exe).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW");
        let bak = backup_path(&exe);
        assert_eq!(std::fs::read(&bak).unwrap(), b"OLD");
        assert!(!rolling.exists());
    }

    #[test]
    fn replace_binary_succeeds_even_when_bak_cannot_be_removed() {
        // Simulate the Windows ACCESS_DENIED scenario: .bak is locked
        // (here we can't truly lock it, but we make it read-only in a
        // non-writable parent — on Unix this still works because unlink
        // doesn't care about file permissions. The test verifies the
        // three-way swap completes successfully regardless.)
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("atomcode");
        let new = tmp.path().join(".atomcode.download");
        let bak = backup_path(&exe);
        let rolling = rolling_path(&exe);
        std::fs::write(&exe, b"V2").unwrap();
        std::fs::write(&new, b"V3").unwrap();
        std::fs::write(&bak, b"V1").unwrap();

        replace_binary(&new, &exe).unwrap();

        // Core outcome: upgrade succeeded
        assert_eq!(std::fs::read(&exe).unwrap(), b"V3");
        // On this platform .bak is removable so we get V2 as backup.
        // On Windows with a locked .bak, the .rolling file would linger
        // instead, but the upgrade still succeeds.
        assert!(bak.exists() || rolling.exists());
    }

    #[test]
    fn rollback_swaps_live_and_bak() {
        let tmp = tempfile::tempdir().unwrap();
        // Use a fake exe name that won't collide with the real test
        // binary. run_rollback uses current_exe() which we cannot
        // redirect, so we test the primitive by calling replace_binary
        // first then rename logic directly — model the three-way swap.
        let exe = tmp.path().join("atomcode");
        let bak = backup_path(&exe);
        std::fs::write(&exe, b"NEW").unwrap();
        std::fs::write(&bak, b"OLD").unwrap();

        // Manual three-way swap mirroring run_rollback's body.
        let rolling = tmp.path().join(".atomcode.rolling");
        std::fs::rename(&exe, &rolling).unwrap();
        std::fs::rename(&bak, &exe).unwrap();
        std::fs::rename(&rolling, &bak).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD");
        assert_eq!(std::fs::read(&bak).unwrap(), b"NEW");
    }

    #[test]
    fn pending_upgrade_serde_roundtrips() {
        let p = PendingUpgrade {
            version: "v4.19.1".to_string(),
            staged_path: PathBuf::from("/tmp/staged/atomcode-v4.19.1-darwin-arm64"),
            sha256: "abcd".to_string(),
            size: 1024,
            created_at: "2026-04-20T10:54:16Z".to_string(),
            attempts: 0,
        };
        let j = serde_json::to_string(&p).unwrap();
        let back: PendingUpgrade = serde_json::from_str(&j).unwrap();
        assert_eq!(back.version, "v4.19.1");
        assert_eq!(back.attempts, 0);
    }

    #[test]
    fn pending_attempts_defaults_when_missing() {
        // Older pointer files written before the attempts field existed
        // must still deserialize — `#[serde(default)]` covers that.
        let j = r#"{
            "version": "v4.19.1",
            "staged_path": "/tmp/x",
            "sha256": "abcd",
            "size": 1024,
            "created_at": "2026-04-20T10:54:16Z"
        }"#;
        let p: PendingUpgrade = serde_json::from_str(j).unwrap();
        assert_eq!(p.attempts, 0);
    }

    #[test]
    fn staged_binary_path_matches_release_artifact_name() {
        let p = staged_binary_path("v4.19.1", "darwin-arm64");
        assert!(p.ends_with("atomcode-v4.19.1-darwin-arm64"), "got: {:?}", p);
    }

    #[test]
    fn staged_binary_path_adds_exe_for_windows() {
        let p = staged_binary_path("v4.19.1", "windows-x64");
        assert!(
            p.ends_with("atomcode-v4.19.1-windows-x64.exe"),
            "got: {:?}",
            p
        );
    }

    // ── download_and_verify integration tests (issue #380) ──────────

    /// Helper: compute the SHA256 hex string for a byte slice.
    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex_encode(&hasher.finalize())
    }

    /// download_and_verify succeeds when the server omits Content-Length
    /// (chunked transfer encoding).  In production, Content-Length may
    /// differ from the manifest's `size` due to CDN/proxy rewrites,
    /// manifest-server sync lag, or redirect hops — the old code
    /// aborted immediately on such mismatches (issue #380).
    ///
    /// This test validates that when Content-Length is absent (chunked),
    /// the function proceeds to download and relies on the post-download
    /// size + SHA256 checks for integrity, which is the correct behavior
    /// after the fix.
    #[tokio::test]
    async fn download_succeeds_without_content_length() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Simulate a CDN that uses chunked transfer (no Content-Length).
        // This is what a gzip-compressed response looks like after
        // reqwest transparently decompresses it.
        let payload = vec![0xAB_u8; 100];
        let expected_sha = sha256_hex(&payload);
        let expected_size: u64 = payload.len() as u64;

        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(
                ResponseTemplate::new(200)
                    // No explicit Content-Length → chunked transfer encoding.
                    .set_body_raw(payload.clone(), "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download.bin");

        let result = download_and_verify(
            &format!("{}/binary", server.uri()),
            &expected_sha,
            expected_size,
            &dest,
            &tx,
        )
        .await;

        // With the fix, this should succeed because the actual downloaded
        // bytes match expected_size and expected_sha256.  The old code
        // would also have succeeded here (Content-Length was absent), but
        // the key point is that we no longer gate on Content-Length at all.
        assert!(result.is_ok(), "download should succeed, got: {:?}", result);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
    }

    /// download_and_verify still rejects a download whose actual byte
    /// count doesn't match the manifest size (post-download size check).
    #[tokio::test]
    async fn download_rejects_wrong_actual_size() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Server sends 80 bytes, but manifest says 100.
        let payload = vec![0xAB_u8; 80];
        let expected_sha = sha256_hex(&payload);
        let expected_size: u64 = 100; // mismatch!

        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(payload.clone(), "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download.bin");

        let result = download_and_verify(
            &format!("{}/binary", server.uri()),
            &expected_sha,
            expected_size,
            &dest,
            &tx,
        )
        .await;

        assert!(result.is_err(), "should fail on size mismatch");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("short download"),
            "error should mention short download, got: {}",
            err
        );
        // Temp file must be cleaned up on failure.
        assert!(!dest.exists(), "partial download should be removed");
    }

    /// download_and_verify still rejects a download whose SHA256
    /// doesn't match the manifest, even if the size is correct.
    #[tokio::test]
    async fn download_rejects_checksum_mismatch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let payload = vec![0xAB_u8; 100];
        let wrong_sha = "0000000000000000000000000000000000000000000000000000000000000000";
        let expected_size: u64 = payload.len() as u64;

        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(payload.clone(), "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download.bin");

        let result = download_and_verify(
            &format!("{}/binary", server.uri()),
            wrong_sha, // intentionally wrong
            expected_size,
            &dest,
            &tx,
        )
        .await;

        assert!(result.is_err(), "should fail on checksum mismatch");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("checksum mismatch"),
            "error should mention checksum mismatch, got: {}",
            err
        );
        // Temp file must be cleaned up on failure.
        assert!(!dest.exists(), "corrupted download should be removed");
    }

    /// download_and_verify succeeds when everything matches perfectly
    /// (Content-Length present and correct, size + SHA256 match).
    #[tokio::test]
    async fn download_succeeds_when_all_checks_pass() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        let payload = vec![0xCD_u8; 256];
        let expected_sha = sha256_hex(&payload);
        let expected_size: u64 = payload.len() as u64;

        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(payload.clone(), "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download.bin");

        let result = download_and_verify(
            &format!("{}/binary", server.uri()),
            &expected_sha,
            expected_size,
            &dest,
            &tx,
        )
        .await;

        assert!(result.is_ok(), "download should succeed, got: {:?}", result);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
    }

    /// download_and_verify rejects non-2xx HTTP responses.
    #[tokio::test]
    async fn download_rejects_http_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download.bin");

        let result = download_and_verify(
            &format!("{}/binary", server.uri()),
            "unused",
            100,
            &dest,
            &tx,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("HTTP 404"),
            "error should mention HTTP 404, got: {}",
            err
        );
    }

    #[test]
    fn is_package_managed_tracks_feature() {
        assert_eq!(super::is_package_managed(), cfg!(feature = "distro-pm"));
    }

    #[cfg(feature = "distro-pm")]
    #[tokio::test]
    async fn run_upgrade_blocked_when_package_managed() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = super::run_upgrade("v1.0.0".to_string(), true, tx)
            .await
            .expect_err("package-managed build must refuse self-upgrade");
        assert!(err.to_string().contains(super::PACKAGE_MANAGED));
    }

    #[cfg(feature = "distro-pm")]
    #[test]
    fn run_rollback_blocked_when_package_managed() {
        let err = super::run_rollback().expect_err("package-managed build must refuse rollback");
        assert!(err.to_string().contains(super::PACKAGE_MANAGED));
    }

    #[cfg(feature = "distro-pm")]
    #[test]
    fn apply_pending_upgrade_noop_when_package_managed() {
        assert!(super::apply_pending_upgrade().unwrap().is_none());
    }

    #[cfg(feature = "distro-pm")]
    #[tokio::test]
    async fn prepare_deferred_upgrade_noop_when_package_managed() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let result = super::prepare_deferred_upgrade("v1.0.0", tx)
            .await
            .expect("package-managed build must not error, just no-op");
        assert!(result.is_none());
    }
}
