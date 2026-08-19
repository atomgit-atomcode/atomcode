//! 本地 daemon token 文件：`~/.atomcode/daemon-<port>.json`（0600）。
//! daemon 是该文件的唯一 writer；合法客户端（VSCode / JetBrains）读取其中的 token
//! 并作为 `Authorization: Bearer` 携带。

use atomcode_config::config::Config;
use std::path::PathBuf;

/// `~/.atomcode/daemon-<port>.json`（遵循 `ATOMCODE_HOME`）。
pub fn token_file_path(port: u16) -> PathBuf {
    Config::config_dir().join(format!("daemon-{port}.json"))
}

/// 写入 token 文件（0600）。返回写入路径。
pub fn write(port: u16, pid: u32, token: &str) -> std::io::Result<PathBuf> {
    let path = token_file_path(port);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "{{\"pid\":{pid},\"port\":{port},\"token\":{}}}",
        serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_string())
    );
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Create with 0600 up front (no world-readable 0644 window between write
        // and chmod). `mode` only applies on creation, so also fchmod the handle
        // to correct a pre-existing (stale) file; truncation empties any old
        // token before we set perms and write, so the token is never exposed.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, body)?;
    Ok(path)
}

/// best-effort 删除 token 文件（退出时调用；忽略错误）。
pub fn remove(port: u16) {
    let _ = std::fs::remove_file(token_file_path(port));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_0600_json_and_remove_deletes() {
        // Acquire the process-global lock to serialise ATOMCODE_HOME mutations
        // across parallel tests, preventing concurrent set_var from other tests.
        let _home_guard = crate::atomcode_home_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Redirect ~/.atomcode to a temp dir via ATOMCODE_HOME.
        let tmp = std::env::temp_dir().join(format!("atomcode_tokfile_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("ATOMCODE_HOME", &tmp);

        let path = write(19999, 4242, "tok-abc").unwrap();
        assert_eq!(path, token_file_path(19999));

        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["port"], 19999);
        assert_eq!(v["token"], "tok-abc");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        remove(19999);
        assert!(!path.exists());

        std::env::remove_var("ATOMCODE_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
