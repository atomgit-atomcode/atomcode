//! Platform-specific process utilities — a kernel-only (L0) local copy of
//! `atomcode_core::process_utils`'s console-window suppressors, since
//! `capabilities` must not depend on `core`.
//!
//! On Windows, a GUI / **console-less** parent (the atomcode-daemon behind
//! clawbot/OpenClaw) that spawns a console program (cmd.exe, git, ast-grep, a
//! language server, …) makes Windows allocate a *fresh* console window for the
//! child — it flashes on the desktop every turn. A TUI parent does NOT show
//! this because the child inherits the TUI's existing console. `CREATE_NO_WINDOW`
//! tells Windows not to allocate one at all, fixing the daemon case without
//! affecting the TUI.
//!
//! NOTE: `creation_flags` is set-only — std cannot read it back — so this flag
//! is not unit-testable off Windows; coverage here is structural (the spawn path
//! routes through this helper) and the behavior must be verified on a Windows build.

/// Apply `CREATE_NO_WINDOW` to a `tokio::process::Command` on Windows; no-op
/// elsewhere. `tokio`'s `creation_flags` is an inherent method on Windows, so
/// (unlike the `_sync` std variant) no `CommandExt` import is needed.
#[cfg(target_os = "windows")]
pub(crate) fn suppress_console_window(cmd: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn suppress_console_window(_cmd: &mut tokio::process::Command) {}

/// Apply `CREATE_NO_WINDOW` to a `std::process::Command` on Windows; no-op
/// elsewhere. The std `creation_flags` requires `CommandExt` in scope.
#[cfg(target_os = "windows")]
pub(crate) fn suppress_console_window_sync(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn suppress_console_window_sync(_cmd: &mut std::process::Command) {}

#[cfg(test)]
pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn new(keys: &[&'static str]) -> Self {
        use std::sync::{Mutex, OnceLock};

        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let saved = keys
            .iter()
            .map(|&key| (key, std::env::var_os(key)))
            .collect();
        Self { saved, _lock: lock }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(target_os = "macos")]
const UTF8_CTYPE_FALLBACK: &str = "UTF-8";
#[cfg(target_os = "macos")]
const UTF8_LANG_FALLBACK: &str = "en_US.UTF-8";
#[cfg(target_os = "macos")]
const ALLOW_BARE_UTF8_CTYPE: bool = true;
#[cfg(all(unix, not(target_os = "macos")))]
const UTF8_CTYPE_FALLBACK: &str = "C.UTF-8";
#[cfg(all(unix, not(target_os = "macos")))]
const UTF8_LANG_FALLBACK: &str = "C.UTF-8";
#[cfg(all(unix, not(target_os = "macos")))]
const ALLOW_BARE_UTF8_CTYPE: bool = false;

#[cfg(unix)]
fn is_c_locale(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case("C") || trimmed.eq_ignore_ascii_case("POSIX")
}

#[cfg(unix)]
fn is_utf8_locale(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.contains("utf-8") || lower.contains("utf8")
}

#[cfg(unix)]
fn is_bare_utf8_locale(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower == "utf-8" || lower == "utf8"
}

#[cfg(unix)]
pub(crate) fn normalize_utf8_locale_env(env: &mut std::collections::BTreeMap<String, String>) {
    let lc_all = env.get("LC_ALL").map(String::as_str);
    if lc_all.is_some_and(|value| is_utf8_locale(value) && !is_bare_utf8_locale(value)) {
        return;
    }
    if lc_all.is_some_and(|value| is_c_locale(value) || is_bare_utf8_locale(value)) {
        env.remove("LC_ALL");
    }

    let has_utf8_ctype = env.get("LC_CTYPE").is_some_and(|value| {
        is_utf8_locale(value) && (ALLOW_BARE_UTF8_CTYPE || !is_bare_utf8_locale(value))
    });
    if !has_utf8_ctype {
        env.insert("LC_CTYPE".to_string(), UTF8_CTYPE_FALLBACK.to_string());
    }

    let should_patch_lang = env
        .get("LANG")
        .map(|value| is_c_locale(value) || is_bare_utf8_locale(value))
        .unwrap_or(true);
    if should_patch_lang {
        env.insert("LANG".to_string(), UTF8_LANG_FALLBACK.to_string());
    }
}

#[cfg(unix)]
fn normalized_locale_env_from_process() -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.to_string(), value.into_string().unwrap_or_default());
        }
    }
    normalize_utf8_locale_env(&mut env);
    env
}

/// Apply a UTF-8-capable locale to async subprocesses spawned from v2 capabilities.
#[cfg(unix)]
pub(crate) fn apply_utf8_locale_env(cmd: &mut tokio::process::Command) {
    let env = normalized_locale_env_from_process();
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        match env.get(key) {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
}

/// Apply a UTF-8-capable locale to sync subprocesses spawned from v2 capabilities.
#[cfg(unix)]
#[cfg_attr(not(feature = "skills"), allow(dead_code))]
pub(crate) fn apply_utf8_locale_env_sync(cmd: &mut std::process::Command) {
    let env = normalized_locale_env_from_process();
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        match env.get(key) {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // STRUCTURAL ONLY: `creation_flags` is set-only (std can't read it back), so the
    // actual CREATE_NO_WINDOW behavior is unverifiable off Windows and must be checked
    // on a Windows build. These tests only assert the helpers leave a command spawnable
    // (i.e. the spawn path can route through them without breaking).
    #[tokio::test]
    async fn tokio_helper_keeps_command_spawnable() {
        let prog = if cfg!(windows) { "cmd" } else { "true" };
        let mut cmd = tokio::process::Command::new(prog);
        if cfg!(windows) {
            cmd.args(["/C", "exit 0"]);
        }
        suppress_console_window(&mut cmd);
        let status = cmd.status().await.expect("spawn after suppress");
        assert!(status.success());
    }

    #[test]
    fn sync_helper_keeps_command_spawnable() {
        let prog = if cfg!(windows) { "cmd" } else { "true" };
        let mut cmd = std::process::Command::new(prog);
        if cfg!(windows) {
            cmd.args(["/C", "exit 0"]);
        }
        suppress_console_window_sync(&mut cmd);
        let status = cmd.status().expect("spawn after suppress");
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn normalize_utf8_locale_env_replaces_c_locale() {
        let mut env = BTreeMap::from([
            ("LC_ALL".to_string(), "C".to_string()),
            ("LANG".to_string(), "C".to_string()),
        ]);

        normalize_utf8_locale_env(&mut env);

        assert!(env
            .values()
            .any(|value| value.to_ascii_lowercase().contains("utf")));
    }

    #[test]
    #[cfg(unix)]
    fn normalize_utf8_locale_env_preserves_existing_utf8_locale() {
        let mut env = BTreeMap::from([
            ("LC_ALL".to_string(), "zh_CN.UTF-8".to_string()),
            ("LANG".to_string(), "zh_CN.UTF-8".to_string()),
        ]);

        normalize_utf8_locale_env(&mut env);

        assert_eq!(env.get("LC_ALL").map(String::as_str), Some("zh_CN.UTF-8"));
    }
}
