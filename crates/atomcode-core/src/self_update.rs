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
//! 6. Rename the live binary to `atomcode.bak` (Windows:
//!    `atomcode.exe.bak`), then rename the downloaded file into place.
//!    Same-directory rename is atomic on POSIX; on Windows, renaming
//!    the currently-running exe is allowed (deleting it is not).
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
pub fn detect_target() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            Some("darwin-arm64")
        } else if cfg!(target_arch = "x86_64") {
            Some("darwin-x64")
        } else {
            None
        }
    } else if cfg!(target_os = "linux") {
        if cfg!(target_arch = "x86_64") {
            Some("linux-x64")
        } else {
            None
        }
    } else if cfg!(target_os = "windows") {
        if cfg!(target_arch = "x86_64") {
            Some("windows-x64")
        } else if cfg!(target_arch = "aarch64") {
            Some("windows-arm64")
        } else {
            None
        }
    } else {
        None
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

/// Same-dir temp used during a three-way rollback swap.
fn rolling_path(exe: &Path) -> PathBuf {
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(crate::ATOMCODE_USER_AGENT)
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

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .user_agent(crate::ATOMCODE_USER_AGENT)
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

    // Prefer the manifest's size (authoritative) over Content-Length,
    // which mirrors may gzip or mis-report. But if Content-Length is
    // present and differs, that's a likely wrong-file red flag — bail.
    let reported = resp.content_length();
    if let Some(n) = reported {
        if n != expected_size {
            return Err(anyhow!(
                "size mismatch: manifest says {} bytes, server says {} bytes — aborting",
                expected_size,
                n
            ));
        }
    }

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

/// Query the actual version string from a binary on disk by running
/// `<binary> --version` and parsing the output. Returns `None` if the
/// subprocess fails, times out, or produces unparseable output — callers
/// should fall back to the manifest version in that case.
///
/// This is the single source of truth for "what version is this binary?"
/// after `replace_binary` has put it in place, ensuring the version
/// reported to the user always matches the real binary rather than the
/// potentially-stale `latest.json` manifest.
fn query_binary_version(exe: &Path) -> Option<String> {
    let mut child = std::process::Command::new(exe)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // Short timeout: a healthy binary prints its version instantly.
    // If this hangs (AV scan, NFS stall), don't block the upgrade.
    // Poll with try_wait since std::process::Child has no built-in timeout.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    };

    if !status.success() {
        return None;
    }

    let stdout = child.stdout.take()?;
    let output = std::io::read_to_string(stdout).ok()?;
    // `atomcode --version` outputs like "4.21.0 (build-id)" on the first
    // line. Extract the first whitespace-delimited token and prepend "v"
    // so the result matches the `vX.Y.Z` convention used everywhere.
    let first_line = output.lines().next()?;
    let raw = first_line.split_whitespace().next()?;
    if raw.starts_with('v') {
        Some(raw.to_string())
    } else {
        Some(format!("v{}", raw))
    }
}

/// Put `new_bin` in place of `exe`, keeping the previous `exe` as `.bak`.
///
/// On Unix, `rename(2)` within a directory is atomic; on Windows, an
/// exe currently being executed can be renamed (but not deleted), so
/// the same sequence works. If a stale `.bak` exists we remove it
/// first — if that removal fails on Windows because the user hasn't
/// restarted since a prior upgrade (the `.bak` is the running
/// process), we surface a clear message.
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
    if backup.exists() {
        std::fs::remove_file(&backup).map_err(|e| {
            anyhow!(
                "could not remove previous backup {}: {}.\n\
                 If you upgraded without restarting, exit atomcode and retry.",
                backup.display(),
                e
            )
        })?;
    }

    std::fs::rename(exe, &backup).with_context(|| {
        format!(
            "renaming current binary {} -> {}",
            exe.display(),
            backup.display()
        )
    })?;

    // Move the new binary into place. `rename(2)` is atomic when source
    // and destination are on the same filesystem, but fails with EXDEV
    // when they are on different mount points (e.g. staged dir on /root
    // and exe on /data). Fall back to copy-then-delete in that case.
    if let Err(e) = std::fs::rename(new_bin, exe) {
        // Cross-device link? Try copy + delete.
        if let Err(copy_err) = std::fs::copy(new_bin, exe).and_then(|_| std::fs::remove_file(new_bin)) {
            // Copy also failed — restore backup and report the original rename error.
            let _ = std::fs::rename(&backup, exe);
            return Err(anyhow!(
                "moving new binary into place failed (rename: {}, copy: {}). Previous version restored.",
                e,
                copy_err
            ));
        }
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
pub async fn run_upgrade(
    current_version: String,
    force: bool,
    tx: mpsc::UnboundedSender<UpgradeEvent>,
) -> Result<UpgradeSummary> {
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

    // After replace_binary succeeds, the new binary is live on disk.
    // Query its actual version instead of trusting manifest.version —
    // the two can diverge when the manifest isn't updated in lockstep
    // with the published artifacts (the root cause of #276).
    let actual_version = query_binary_version(&exe).unwrap_or_else(|| manifest.version.clone());

    let backup = backup_path(&exe);
    let _ = tx.send(UpgradeEvent::Done {
        version: actual_version.clone(),
        backup: backup.clone(),
    });

    Ok(UpgradeSummary {
        version: actual_version,
        backup,
        exe,
    })
}

/// Sentinel substring in the "already latest" error so the CLI/TUI
/// layer can render a calm informational message instead of a scary
/// red error. Kept as a plain string to avoid an error-type refactor.
pub const ALREADY_LATEST: &str = "ALREADY_LATEST";

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
    crate::config::Config::config_dir().join("staged")
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
pub async fn prepare_deferred_upgrade(
    current_version: &str,
    tx: mpsc::UnboundedSender<UpgradeEvent>,
) -> Result<Option<PendingUpgrade>> {
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
pub fn apply_pending_upgrade() -> Result<Option<AppliedUpgrade>> {
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

    // Query the actual version from the new binary on disk instead of
    // trusting pending.version (which came from the manifest and may be
    // stale — see #276). Fall back to pending.version if the query fails
    // (e.g. AV intercept, NFS stall) so the upgrade still completes.
    let actual_version = query_binary_version(&exe).unwrap_or_else(|| pending.version.clone());

    Ok(Some(AppliedUpgrade {
        version: actual_version,
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
/// Never returns on the happy path. An `Err` return means the handoff
/// failed (e.g., new binary missing execute bit under unusual filesystem
/// constraints); caller should surface the error and keep running with
/// the old binary rather than exiting silently.
pub fn re_exec_self() -> Result<std::convert::Infallible> {
    let exe = current_exe_path()?;
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
pub fn run_rollback() -> Result<RollbackSummary> {
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

    // Step 1: live -> rolling
    std::fs::rename(&exe, &rolling).with_context(|| {
        format!(
            "renaming {} -> {} (swap step 1)",
            exe.display(),
            rolling.display()
        )
    })?;
    // Step 2: backup -> live
    if let Err(e) = std::fs::rename(&backup, &exe) {
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
    fn backup_path_appends_bak() {
        let p = Path::new("/usr/local/bin/atomcode");
        assert_eq!(backup_path(p), PathBuf::from("/usr/local/bin/atomcode.bak"));
    }

    #[test]
    fn backup_path_preserves_exe_suffix_on_windows_style() {
        let p = Path::new("C:/Tools/atomcode.exe");
        assert_eq!(backup_path(p), PathBuf::from("C:/Tools/atomcode.exe.bak"));
    }

    #[test]
    fn is_newer_semver() {
        assert!(is_newer("v4.19.0", "v4.18.2"));
        assert!(is_newer("v4.19.0", "v4.18.9"));
        assert!(is_newer("v5.0.0", "v4.99.99"));
        assert!(!is_newer("v4.19.0", "v4.19.0"));
        assert!(!is_newer("v4.18.0", "v4.19.0"));
    }

    #[test]
    fn is_newer_falls_back_to_string_diff_on_garbage() {
        // If we can't parse, err on the side of allowing reinstall
        // when strings differ (user may have a custom channel).
        assert!(is_newer("build-abc", "build-xyz"));
        assert!(!is_newer("build-abc", "build-abc"));
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
    fn replace_binary_renames_live_to_bak() {
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

    // ========================================================================
    // query_binary_version tests
    // ========================================================================
    //
    // Strategy: we create small shell-script / batch "fake binaries" in a
    // temp directory that print a version string when run with --version.
    // This lets us exercise query_binary_version on every OS without
    // needing the real atomcode binary.

    /// Helper: create a fake "binary" in `dir` that prints `output` when
    /// invoked with any arguments. On Unix this is a shell script; on
    /// Windows it's a .bat file.
    fn create_fake_binary(dir: &Path, name: &str, output: &str) -> PathBuf {
        #[cfg(unix)]
        {
            let path = dir.join(name);
            let script = format!("#!/bin/sh\necho '{}'", output);
            std::fs::write(&path, script).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        #[cfg(windows)]
        {
            let path = dir.join(format!("{}.bat", name));
            let script = format!("@echo {}\n@exit /b 0", output);
            std::fs::write(&path, script).unwrap();
            path
        }
    }

    // ---- Cross-platform: basic version parsing ----

    #[test]
    fn query_binary_version_parses_bare_version() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "4.21.0");
        let result = query_binary_version(&fake);
        assert_eq!(result, Some("v4.21.0".to_string()), "bare version should get v- prefix");
    }

    #[test]
    fn query_binary_version_parses_v_prefixed_version() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "v4.21.0");
        let result = query_binary_version(&fake);
        assert_eq!(result, Some("v4.21.0".to_string()), "v-prefixed version should be preserved");
    }

    #[test]
    fn query_binary_version_parses_version_with_build_metadata() {
        // Real atomcode --version outputs: "4.21.0 (build-id)" or similar
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "4.21.0 (abc123-dirty)");
        let result = query_binary_version(&fake);
        assert_eq!(result, Some("v4.21.0".to_string()), "should extract first token only");
    }

    #[test]
    fn query_binary_version_parses_v_prefix_with_build_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "v4.21.1 (release)");
        let result = query_binary_version(&fake);
        assert_eq!(result, Some("v4.21.1".to_string()), "should extract v-prefixed first token");
    }

    #[test]
    fn query_binary_version_parses_major_minor_only() {
        // Unlikely but should still work
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "5.0");
        let result = query_binary_version(&fake);
        assert_eq!(result, Some("v5.0".to_string()), "two-part version should work");
    }

    // ---- Cross-platform: error / edge cases ----

    #[test]
    fn query_binary_version_returns_none_for_nonexistent_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist");
        assert_eq!(query_binary_version(&path), None, "missing binary should return None");
    }

    #[test]
    fn query_binary_version_returns_none_for_empty_output() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a script that outputs nothing
        #[cfg(unix)]
        {
            let path = tmp.path().join("silent");
            std::fs::write(&path, "#!/bin/sh\ntrue").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(query_binary_version(&path), None, "empty output should return None");
        }
        #[cfg(windows)]
        {
            let path = tmp.path().join("silent.bat");
            std::fs::write(&path, "@echo off\n@exit /b 0").unwrap();
            assert_eq!(query_binary_version(&path), None, "empty output should return None");
        }
    }

    #[test]
    fn query_binary_version_returns_none_for_nonzero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            let path = tmp.path().join("failing");
            std::fs::write(&path, "#!/bin/sh\necho '4.21.0'; exit 1").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(query_binary_version(&path), None, "non-zero exit should return None");
        }
        #[cfg(windows)]
        {
            let path = tmp.path().join("failing.bat");
            std::fs::write(&path, "@echo 4.21.0\n@exit /b 1").unwrap();
            assert_eq!(query_binary_version(&path), None, "non-zero exit should return None");
        }
    }

    // ---- macOS-specific: code-signed binary behavior ----

    #[test]
    #[cfg(target_os = "macos")]
    fn query_binary_version_works_with_real_atomcode_binary_on_macos() {
        // If the test binary itself can be found, use it as a real binary
        // that supports --version. This validates the full pipe on macOS.
        let exe = std::env::current_exe().unwrap();
        // The test binary may or may not support --version the same way as
        // the production binary; just verify the function doesn't panic or
        // hang. If it returns Some, check the format.
        if let Some(v) = query_binary_version(&exe) {
            assert!(v.starts_with('v'), "version should start with 'v', got: {}", v);
        }
        // Returning None is also acceptable (test binary might not support --version)
    }

    // ---- Linux-specific: executable permission behavior ----

    #[test]
    #[cfg(target_os = "linux")]
    fn query_binary_version_fails_for_non_executable_on_linux() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("noexec");
        std::fs::write(&path, "4.21.0").unwrap();
        // File exists but is NOT executable → spawn should fail → None
        assert_eq!(query_binary_version(&path), None, "non-executable should return None");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn query_binary_version_works_with_executable_on_linux() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "4.21.0");
        let result = query_binary_version(&fake);
        assert_eq!(result, Some("v4.21.0".to_string()), "executable script on Linux should work");
    }

    // ---- Windows-specific: .exe behavior ----

    #[test]
    #[cfg(target_os = "windows")]
    fn query_binary_version_works_with_bat_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "4.21.0");
        let result = query_binary_version(&fake);
        assert_eq!(result, Some("v4.21.0".to_string()), "bat script on Windows should work");
    }

    // ---- Timeout behavior ----

    #[test]
    fn query_binary_version_times_out_on_hanging_binary() {
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            let path = tmp.path().join("hanging");
            // Script that sleeps for 30 seconds — much longer than the 5s timeout
            std::fs::write(&path, "#!/bin/sh\nsleep 30\necho '4.21.0'").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

            let start = std::time::Instant::now();
            let result = query_binary_version(&path);
            let elapsed = start.elapsed();

            assert_eq!(result, None, "hanging binary should return None after timeout");
            assert!(elapsed < std::time::Duration::from_secs(8),
                "should timeout within ~5s, not hang for 30s (elapsed: {:?})", elapsed);
        }
        #[cfg(windows)]
        {
            let path = tmp.path().join("hanging.bat");
            // Batch that sleeps for 30 seconds
            std::fs::write(&path, "@ping -n 30 127.0.0.1 >nul\n@echo 4.21.0").unwrap();

            let start = std::time::Instant::now();
            let result = query_binary_version(&path);

            assert_eq!(result, None, "hanging binary should return None after timeout");
            let elapsed = start.elapsed();
            assert!(elapsed < std::time::Duration::from_secs(8),
                "should timeout within ~5s (elapsed: {:?})", elapsed);
        }
    }

    // ---- Fallback behavior in upgrade paths ----

    #[test]
    fn query_binary_version_fallback_returns_manifest_version_on_failure() {
        // Simulates the fallback pattern used in run_upgrade and
        // apply_pending_upgrade: query_binary_version(exe).unwrap_or_else(|| manifest.version)
        let manifest_version = "v4.20.0".to_string();
        let nonexistent = Path::new("/nonexistent-binary-xyz");

        let result = query_binary_version(nonexistent).unwrap_or_else(|| manifest_version.clone());
        assert_eq!(result, "v4.20.0", "should fall back to manifest version when query fails");
    }

    #[test]
    fn query_binary_version_prefers_real_version_over_manifest() {
        // When query succeeds, it should override the manifest version
        let tmp = tempfile::tempdir().unwrap();
        let fake = create_fake_binary(tmp.path(), "atomcode", "4.21.1");
        let manifest_version = "v4.21.0".to_string(); // stale manifest

        let result = query_binary_version(&fake).unwrap_or_else(|| manifest_version.clone());
        assert_eq!(result, "v4.21.1", "actual binary version should override stale manifest");
    }
}
