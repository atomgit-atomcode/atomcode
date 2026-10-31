//! Where a phone gets an HTTPS git credential, and where git finds it.
//!
//! A phone has no terminal and no credential prompt — `atomcode-android` sets
//! `GIT_TERMINAL_PROMPT=0` so a missing credential fails a turn instead of
//! hanging one. The only way a private clone or push can work is a token handed
//! over once and stored where git's own `credential.helper = store` reads it.
//! This module is that hand-over, and nothing more.
//!
//! The file is `$HOME/.git-credentials`, which is exactly how git resolves `~`
//! for that helper. The Android host sets `HOME` to the app-private product dir
//! before the daemon starts, so one implementation is right on both platforms.
//!
//! Tokens never leave this module: no API response, config tree or `describe_self`
//! answer carries one (`AGENTS.md` credential rule). [`list`](list_credentials)
//! returns hosts and usernames only.

use std::path::{Path, PathBuf};

use axum::extract::Path as UrlPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};

use crate::json_error;

/// What a person may see about a stored credential: the host it belongs to and
/// the account name. Never the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialSummary {
    pub host: String,
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub struct StoreRequest {
    /// Bare host name, e.g. `atomgit.com`. A scheme or a path is refused.
    pub host: String,
    /// Account the token belongs to. Required: a token alone cannot be placed
    /// on the right account, and guessing produces a 401 nobody can explain.
    pub username: String,
    /// The personal access token. Stored, never echoed back.
    pub token: String,
}

/// POST /git/credentials
pub(crate) async fn store_credential(Json(req): Json<StoreRequest>) -> impl IntoResponse {
    let path = match credentials_path() {
        Ok(path) => path,
        Err(error) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    match store_at(&path, &req.host, &req.username, &req.token) {
        Ok(host) => Json(serde_json::json!({ "success": true, "host": host })).into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

/// GET /git/credentials
pub(crate) async fn list_credentials() -> impl IntoResponse {
    let path = match credentials_path() {
        Ok(path) => path,
        Err(error) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    Json(serde_json::json!({
        "success": true,
        "credentials": list_at(&path),
    }))
    .into_response()
}

/// DELETE /git/credentials/:host
pub(crate) async fn remove_credential(UrlPath(host): UrlPath<String>) -> impl IntoResponse {
    let path = match credentials_path() {
        Ok(path) => path,
        Err(error) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    };
    match remove_at(&path, &host) {
        Ok(removed) => {
            Json(serde_json::json!({ "success": true, "removed": removed })).into_response()
        }
        Err(error) => json_error(StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

/// `$HOME/.git-credentials`, the path git's `store` helper uses.
fn credentials_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(windows_home);
    match home {
        Some(home) => Ok(home.join(".git-credentials")),
        None => anyhow::bail!(
            "HOME is not set: git's credential store has no place to live. \
             Start the daemon with HOME pointing at the product tree."
        ),
    }
}

/// Windows has no `HOME`; the daemon may still be started from a shell that only
/// set `USERPROFILE`.
#[cfg(windows)]
fn windows_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

#[cfg(not(windows))]
fn windows_home() -> Option<PathBuf> {
    None
}

/// Store (or replace) the credential for `host`. Returns the normalised host.
fn store_at(path: &Path, host: &str, username: &str, token: &str) -> anyhow::Result<String> {
    let host = normalize_host(host)?;
    let username = require_single_line(username, "username")?;
    let token = require_single_line(token, "token")?;

    let mut lines = read_lines(path);
    lines.retain(|line| host_of(line).as_deref() != Some(host.as_str()));
    lines.push(format!(
        "https://{}:{}@{}",
        encode(&username),
        encode(&token),
        host
    ));
    write_lines(path, &lines)?;
    Ok(host)
}

/// The credentials this file holds, hosts and usernames only.
fn list_at(path: &Path) -> Vec<CredentialSummary> {
    read_lines(path)
        .into_iter()
        .filter_map(|line| {
            let (userinfo, host) = line.rsplit_once('@')?;
            let (username, _token) = userinfo
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(userinfo)
                .split_once(':')?;
            Some(CredentialSummary {
                host: host.to_string(),
                username: decode(username),
            })
        })
        .collect()
}

/// Delete the credential for `host`, if there is one. The file disappears with
/// its last entry so a cleared credential leaves nothing behind.
fn remove_at(path: &Path, host: &str) -> anyhow::Result<bool> {
    let host = normalize_host(host)?;
    let lines = read_lines(path);
    let kept: Vec<String> = lines
        .iter()
        .filter(|line| host_of(line).as_deref() != Some(host.as_str()))
        .cloned()
        .collect();
    let removed = kept.len() != lines.len();
    if removed {
        write_lines(path, &kept)?;
    }
    Ok(removed)
}

fn read_lines(path: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Replace the file atomically: a half-written credential file would make git's
/// `store` helper fail to parse the very entry it is looking for.
fn write_lines(path: &Path, lines: &[String]) -> anyhow::Result<()> {
    if lines.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension("credentials.tmp");
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(&staging, body)?;
    restrict_permissions(&staging)?;
    std::fs::rename(&staging, path)?;
    Ok(())
}

/// Owner-only: the file is a token in plain text, and on a phone the product
/// tree is shared with everything the app itself runs.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn host_of(line: &str) -> Option<String> {
    let (_, host) = line.rsplit_once('@')?;
    Some(host.to_string())
}

/// A host, not a URL: the helper matches on host, and a pasted repository URL
/// would silently never match anything.
fn normalize_host(host: &str) -> anyhow::Result<String> {
    let host = host.trim().to_ascii_lowercase();
    let bad = host.is_empty()
        || host.contains("://")
        || host
            .chars()
            .any(|c| c == '/' || c == '@' || c == '\\' || c.is_whitespace());
    if bad {
        anyhow::bail!("not a host name: {host:?} (give `atomgit.com`, not a repository URL)");
    }
    Ok(host)
}

fn require_single_line(value: &str, what: &str) -> anyhow::Result<String> {
    if value.trim().is_empty() {
        anyhow::bail!("{what} is empty");
    }
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!("{what} must be one line");
    }
    Ok(value.to_string())
}

/// Percent-encode what git splits on in `user:token@host`.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let plain = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if plain {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_token_is_listed_only_as_a_host_and_account() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join(".git-credentials");
        store_at(&path, "AtomGit.com", "alice", "tok_123").unwrap();

        let listed = list_at(&path);
        assert_eq!(
            listed,
            vec![CredentialSummary {
                host: "atomgit.com".into(),
                username: "alice".into(),
            }]
        );
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "https://alice:tok_123@atomgit.com\n");
        assert!(!format!("{listed:?}").contains("tok_123"));
    }

    #[test]
    fn storing_the_same_host_twice_keeps_one_entry() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join(".git-credentials");
        store_at(&path, "atomgit.com", "alice", "old").unwrap();
        store_at(&path, "atomgit.com", "alice", "new").unwrap();
        store_at(&path, "gitcode.com", "bob", "other").unwrap();

        let listed = list_at(&path);
        assert_eq!(listed.len(), 2, "one entry per host: {listed:?}");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("alice:new@atomgit.com"));
        assert!(!body.contains("old"));
    }

    #[test]
    fn removing_the_last_credential_removes_the_file() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join(".git-credentials");
        store_at(&path, "atomgit.com", "alice", "tok").unwrap();
        assert!(remove_at(&path, "atomgit.com").unwrap());
        assert!(!path.exists(), "an empty credential file must not linger");
        assert!(!remove_at(&path, "atomgit.com").unwrap());
    }

    #[test]
    fn a_repository_url_is_not_a_host() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join(".git-credentials");
        for bad in [
            "https://atomgit.com/a/b.git",
            "atomgit.com/a",
            "git@atomgit.com",
            "",
            "atom git.com",
        ] {
            assert!(store_at(&path, bad, "alice", "tok").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_token_with_the_separators_is_encoded() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join(".git-credentials");
        store_at(&path, "atomgit.com", "al ice", "to:k@en").unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "https://al%20ice:to%3Ak%40en@atomgit.com\n");
        assert_eq!(list_at(&path)[0].username, "al ice");
        assert_eq!(host_of(body.trim()).as_deref(), Some("atomgit.com"));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join(".git-credentials");
        store_at(&path, "atomgit.com", "alice", "tok").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a token file must not be group-readable"
        );
    }

    #[test]
    fn a_multi_line_token_is_refused() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join(".git-credentials");
        assert!(store_at(&path, "atomgit.com", "alice", "a\nb").is_err());
        assert!(store_at(&path, "atomgit.com", "alice", "").is_err());
    }
}
