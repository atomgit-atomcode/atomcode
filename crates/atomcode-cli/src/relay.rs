//! `/app` 要的那个中继客户端:找到它,必要时下载,校验之后给出可执行文件路径。
//!
//! 从 `atomcode-tuix/src/event_loop/commands.rs` 整段搬来(2026-09-23),逐字未改:
//! 去哪儿下载、怎么校验、清单拿不到时退到哪个版本,是这个产品的知识,不该跟着旧界面
//! 一起没掉。要改措辞或改策略另起一条,这一趟只是搬家。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};

/// 出问题时告诉人去哪儿自己下载。
const RELEASES_PAGE: &str = "https://gitcode.com/atomgit_atomcode/atomcode-relay-release/releases";
/// 一行装好的脚本。
const INSTALL_SCRIPT: &str =
    "curl -fsSL https://raw.gitcode.com/atomgit_atomcode/atomcode-relay-release/raw/main/scripts/install.sh | sh";
use std::path::PathBuf;

/// 中继客户端 oss 下载地址。
/// 对应 gitcode.com/atomgit_atomcode/atomcode-relay-release 仓库的 Release。
const RELAY_CLIENT_DOWNLOAD_BASE: &str =
    "https://gitcode.com/atomgit_atomcode/atomcode-relay-release/releases/download";

/// relay-client 版本清单地址。
const RELAY_MANIFEST_URL: &str =
    "https://raw.gitcode.com/atomgit_atomcode/atomcode-relay-release/raw/main/relay-latest.json";

/// 兜底版本号（远端清单获取失败时使用，与 release 版本保持一致）。
const FALLBACK_RELAY_VERSION: &str = "v0.1.0";

/// 兜底版本的 sha256 和 size（远端清单获取失败时使用）。
/// 各平台值从 relay-latest.json 同步。
const FALLBACK_BINARIES: &[(&str, &str, u64)] = &[
    (
        "aarch64-macos",
        "a3eb823821cc29526371aa11f0f03f08e0fe9089300d3d7e81b19d0d848ca78a",
        4577584,
    ),
    (
        "x86_64-macos",
        "eb77bd0e6f46ec6dbe8f7dcbafe814d3d0992ca26e5c6b05182349aa6f59ad03",
        4916448,
    ),
    (
        "x86_64-linux",
        "37725dfd94ab58efe619b6f8e087db40c9a456b6d87c075c409c9a2ce83e0e94",
        5263216,
    ),
    (
        "aarch64-linux",
        "e63d374daf27f7743fc28624bdd4fcfae04d011566bd42175291df5f4abcbd7d",
        4661464,
    ),
    (
        "ohos-arm64",
        "a5082c219aaea7114758774b9c9e4924c84c9fb16b39fe9f92e6c7ab083d0744",
        4646656,
    ),
    (
        "x86_64-win",
        "9819fad219bb743af036a134ff903de8c2469bcffe7a655548c2229edb5f398e",
        5683344,
    ),
];

/// relay-client 版本清单结构。
#[derive(serde::Deserialize)]
struct RelayManifest {
    version: String,
    binaries: std::collections::BTreeMap<String, RelayBinaryEntry>,
}

#[derive(serde::Deserialize)]
struct RelayBinaryEntry {
    sha256: String,
    size: u64,
}

/// 获取 relay-client 远端版本清单。
async fn fetch_relay_manifest() -> Result<RelayManifest, String> {
    let token = atomcode_auth::oauth::get_valid_token()
        .map_err(|_| tr(SMsg::RelayNeedsLogin).into_owned())?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("atomcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let resp = client
        .get(RELAY_MANIFEST_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .map_err(|e| format!("fetching the manifest: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "fetching the manifest: HTTP {}",
            resp.status().as_u16()
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("reading the manifest: {e}"))?;
    let manifest: RelayManifest =
        serde_json::from_str(&body).map_err(|e| format!("parsing the manifest: {e}"))?;

    Ok(manifest)
}

/// 检测当前平台对应的目标标识，用于构建下载文件名。
/// 格式：{arch}-{os}，与 Release 实际文件名一致。
fn relay_client_target() -> &'static str {
    // HarmonyOS / OpenHarmony 在运行时 OS 显示为 "linux"，
    // 用编译时 cfg 区分
    #[cfg(target_env = "ohos")]
    {
        return match std::env::consts::ARCH {
            "aarch64" | "arm64" => "ohos-arm64",
            _ => "unknown",
        };
    }
    #[cfg(not(target_env = "ohos"))]
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-macos",
        ("macos", "x86_64") => "x86_64-macos",
        ("linux", "x86_64") => "x86_64-linux",
        ("linux", "aarch64") => "aarch64-linux",
        ("windows", "x86_64") => "x86_64-win",
        _ => "unknown",
    }
}

/// 根据平台名构建下载文件名（含版本号，Windows 加 .exe 后缀）。
fn relay_client_filename(target: &str, version: &str) -> String {
    if target.starts_with("x86_64-win") {
        format!("atomcode-relay-client-{}-{}.exe", version, target)
    } else {
        format!("atomcode-relay-client-{}-{}", version, target)
    }
}

/// 字节数组转小写 hex 字符串。
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 解析 semver 版本号 `vMAJOR.MINOR.PATCH`，返回 (major, minor, patch)。
/// 无法解析时返回 None。
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().strip_prefix('v')?.split('-').next()?;
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

/// 判断 latest 是否比 current 新（semver 比较）。
fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => latest.trim() != current.trim(),
    }
}

/// 解析 relay-client 二进制路径。优先级：
/// 1. `ATOMCODE_RELAY_CLIENT_BIN` 环境变量 —— 开发者/特殊部署覆盖。
/// 2. 与 atomcode 自身可执行文件同目录 —— 安装包捆绑分发。
fn resolve_relay_client_bin() -> Option<String> {
    // 1) 显式环境变量覆盖（非空才采纳）。
    if let Ok(p) = std::env::var("ATOMCODE_RELAY_CLIENT_BIN") {
        if !p.is_empty() && std::path::Path::new(&p).is_file() {
            return Some(p);
        }
    }

    // 2) 与自身同目录。Windows 带 .exe 后缀；命中文件才返回绝对路径。
    let exe_name = if cfg!(windows) {
        "atomcode-relay-client.exe"
    } else {
        "atomcode-relay-client"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sibling) = exe.parent().map(|dir| dir.join(exe_name)) {
            if sibling.is_file() {
                return Some(sibling.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// relay-client 的缓存目录：`$ATOMCODE_HOME/bin`。
///
/// 走 `Config::config_dir()` 而不是硬拼 `~/.atomcode`：设了 `$ATOMCODE_HOME`
/// 时,下载的二进制本该和其它数据落在同一棵树里 —— 否则 `uninstall` 扫不到它,
/// 而且提示语指的目录和实际写入的目录会对不上。
fn relay_client_cache_dir() -> PathBuf {
    atomcode_config::config::Config::config_dir().join("bin")
}

/// 确保 relay-client 二进制可用。
/// 先尝试本地查找（环境变量 → 同目录 → 缓存），都不存在则自动下载到缓存目录。
pub fn ensure_relay_client_bin() -> Result<String, String> {
    // 先尝试环境变量和同目录
    if let Some(bin) = resolve_relay_client_bin() {
        return Ok(bin);
    }

    let bare_name = if cfg!(windows) {
        "atomcode-relay-client.exe"
    } else {
        "atomcode-relay-client"
    };

    let cache_dir = relay_client_cache_dir();
    let cache_path = cache_dir.join(bare_name);
    let version_path = cache_dir.join(".version");

    // 缓存已存在 → 直接使用
    if cache_path.is_file() {
        return Ok(cache_path.to_string_lossy().into_owned());
    }

    // 跳过下载标志
    if std::env::var("ATOMCODE_RELAY_CLIENT_SKIP_DOWNLOAD").is_ok_and(|v| v == "1") {
        return Err(tr(SMsg::RelayDownloadOff {
            dir: &cache_dir.display().to_string(),
        })
        .into_owned());
    }

    // 检测平台
    let target = relay_client_target();
    if target == "unknown" {
        return Err(tr(SMsg::RelayUnsupportedPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            dir: &cache_dir.display().to_string(),
        })
        .into_owned());
    }

    // 6) 获取远端版本清单（含最新版本号 + sha256）
    let manifest = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(fetch_relay_manifest())
    });
    let manifest = match manifest {
        Ok(m) => m,
        Err(_) => {
            // 清单获取失败 → 使用兜底版本
            // 有缓存且版本不低于兜底版本 → 直接用缓存
            if cache_path.is_file() {
                if let Ok(ref ver) = std::fs::read_to_string(&version_path) {
                    let ver = ver.trim();
                    // 兜底版本作为最低要求，用 semver 比对
                    if !is_newer_version(FALLBACK_RELAY_VERSION, ver) {
                        return Ok(cache_path.to_string_lossy().into_owned());
                    }
                }
                // 缓存版本低于兜底版本 → 继续走兜底下载
            }
            // 构造兜底 manifest
            let mut fallback_binaries = std::collections::BTreeMap::new();
            for (platform, sha256, size) in FALLBACK_BINARIES {
                fallback_binaries.insert(
                    platform.to_string(),
                    RelayBinaryEntry {
                        sha256: sha256.to_string(),
                        size: *size,
                    },
                );
            }
            RelayManifest {
                version: FALLBACK_RELAY_VERSION.to_string(),
                binaries: fallback_binaries,
            }
        }
    };

    // 7) 检查缓存版本是否最新
    let cached_version = std::fs::read_to_string(&version_path).ok();
    if let Some(ref ver) = cached_version {
        let ver = ver.trim();
        if !is_newer_version(&manifest.version, ver) && cache_path.is_file() {
            return Ok(cache_path.to_string_lossy().into_owned());
        }
    }

    // 8) 获取当前平台的 binary entry
    let entry = match manifest.binaries.get(target) {
        Some(e) => e,
        None => {
            return Err(format!(
                "release {} has no binary for {target}",
                manifest.version
            ));
        }
    };

    // 9) 自动下载 + SHA256 校验
    let filename = relay_client_filename(target, &manifest.version);
    let url = format!(
        "{}/{}/{}",
        RELAY_CLIENT_DOWNLOAD_BASE, manifest.version, filename
    );

    // 使用 block_in_place 执行异步下载（当前在同步上下文中）
    let download_result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(download_relay_client(
            &url,
            &cache_path,
            &entry.sha256,
            entry.size,
        ))
    });

    match download_result {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&cache_path, std::fs::Permissions::from_mode(0o755));
            }
            // 写入缓存版本号
            let _ = std::fs::write(&version_path, manifest.version.as_bytes());
            Ok(cache_path.to_string_lossy().into_owned())
        }
        Err(e) => {
            let msg = tr(SMsg::RelayDownloadFailed {
                error: &e.to_string(),
                dir: &cache_dir.display().to_string(),
                releases: RELEASES_PAGE,
                install: INSTALL_SCRIPT,
            })
            .into_owned();
            Err(msg)
        }
    }
}

/// 从指定 URL 下载 relay-client 二进制到缓存路径。
/// 使用 GitCode OAuth token 进行鉴权，下载完成后校验 SHA256 和文件大小。
async fn download_relay_client(
    url: &str,
    dest: &std::path::Path,
    expected_sha256: &str,
    expected_size: u64,
) -> Result<(), String> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    // SHA256 计算
    use sha2::{Digest, Sha256};

    // 确保缓存目录存在
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating the cache directory: {e}"))?;
    }

    // 获取 GitCode OAuth token（用户需先 /login）
    let token = atomcode_auth::oauth::get_valid_token()
        .map_err(|_| tr(SMsg::RelayNeedsLogin).into_owned())?;

    // 构建 HTTP 客户端 + 添加鉴权头
    let client = reqwest::Client::builder()
        .user_agent(concat!("atomcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let resp = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .map_err(|e| format!("download request: {e}"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(tr(SMsg::RelayNeedsLogin).into_owned());
    }
    if !resp.status().is_success() {
        return Err(format!(
            "download: HTTP {} (no such release, or no access)",
            resp.status().as_u16()
        ));
    }

    // 流式下载 + SHA256 累积
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("creating the file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("reading the download: {e}"))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("writing the file: {e}"))?;
        written += chunk.len() as u64;
    }
    file.flush()
        .await
        .map_err(|e| format!("flushing the file: {e}"))?;
    drop(file);

    // 校验文件大小
    if expected_size > 0 && written != expected_size {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "size mismatch: expected {expected_size} bytes, got {written}"
        ));
    }

    // 校验 SHA256
    let got = hex_encode(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_sha256) {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "sha256 mismatch: expected {expected_sha256}, got {got}"
        ));
    }

    Ok(())
}
