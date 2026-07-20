//! Platform-specific process utilities.
//!
//! On Windows, GUI processes (like VSCode extension host / atomcode-daemon)
//! that spawn console programs (git, curl, cmd.exe, etc.) will cause Windows
//! to automatically create a visible console window for the child process.
//! The `suppress_console_window` helpers apply the `CREATE_NO_WINDOW` creation
//! flag to prevent this.

/// Apply `CREATE_NO_WINDOW` to a `tokio::process::Command` on Windows.
/// No-op on other platforms.
///
/// `tokio::process::Command::creation_flags` is an inherent method on
/// Windows — unlike `std::process::Command` it does NOT require the
/// `std::os::windows::process::CommandExt` trait to be in scope, which
/// is why this body lacks the `use` statement that
/// `suppress_console_window_sync` below needs.
#[cfg(target_os = "windows")]
pub fn suppress_console_window(cmd: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No-op on non-Windows platforms.
#[cfg(not(target_os = "windows"))]
pub fn suppress_console_window(_cmd: &mut tokio::process::Command) {}

/// Apply `CREATE_NO_WINDOW` to a `std::process::Command` on Windows.
/// No-op on other platforms.
#[cfg(target_os = "windows")]
pub fn suppress_console_window_sync(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No-op on non-Windows platforms.
#[cfg(not(target_os = "windows"))]
pub fn suppress_console_window_sync(_cmd: &mut std::process::Command) {}

/// Build a shell command that runs `command` through the platform shell.
///
/// - Windows: `cmd.exe /C <command>` — the command string is passed via
///   `raw_arg` so cmd.exe receives it **verbatim**. Using the normal `.arg()`
///   would apply std's `CommandLineToArgvW` quoting, which cmd.exe does NOT
///   follow — embedded quotes / `%VAR%` / `^` etc. would be mangled. This
///   mirrors the spawn in `tool/bash.rs` (and `auth/oauth.rs`).
/// - Other: `sh -c <command>`.
///
/// Caller still chains env/stdio/`kill_on_drop` and `suppress_console_window`
/// as needed; this only fixes the program + command-string wiring.
#[cfg(target_os = "windows")]
pub fn shell_command(command: &str) -> tokio::process::Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = tokio::process::Command::new("cmd.exe");
    cmd.arg("/C");
    cmd.as_std_mut().raw_arg(command);
    cmd
}

/// See the Windows variant above.
#[cfg(not(target_os = "windows"))]
pub fn shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    #[cfg(unix)]
    apply_utf8_locale_env(&mut cmd);
    cmd
}

#[cfg(test)]
pub(crate) fn locale_env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};

    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // This lock only serializes env mutation; recover from a poisoned lock (a prior
    // holder panicked, e.g. an env-race assertion under the parallel harness) instead
    // of cascading a `.unwrap()` panic into every other locale test.
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn new(keys: &[&'static str]) -> Self {
        let lock = locale_env_test_lock();
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

/// Normalize locale environment variables so POSIX tools can print Unicode paths.
#[cfg(unix)]
pub fn normalize_utf8_locale_env(env: &mut std::collections::BTreeMap<String, String>) {
    normalize_utf8_locale_env_for_platform(
        env,
        UTF8_CTYPE_FALLBACK,
        UTF8_LANG_FALLBACK,
        ALLOW_BARE_UTF8_CTYPE,
    );
}

#[cfg(unix)]
fn normalize_utf8_locale_env_for_platform(
    env: &mut std::collections::BTreeMap<String, String>,
    ctype_fallback: &str,
    lang_fallback: &str,
    allow_bare_utf8_ctype: bool,
) {
    let lc_all = env.get("LC_ALL").map(String::as_str);
    if lc_all.is_some_and(|value| is_utf8_locale(value) && !is_bare_utf8_locale(value)) {
        return;
    }
    if lc_all.is_some_and(|value| is_c_locale(value) || is_bare_utf8_locale(value)) {
        env.remove("LC_ALL");
    }

    let has_utf8_ctype = env.get("LC_CTYPE").is_some_and(|value| {
        is_utf8_locale(value) && (allow_bare_utf8_ctype || !is_bare_utf8_locale(value))
    });
    if !has_utf8_ctype {
        env.insert("LC_CTYPE".to_string(), ctype_fallback.to_string());
    }

    let should_patch_lang = env
        .get("LANG")
        .map(|value| is_c_locale(value) || is_bare_utf8_locale(value))
        .unwrap_or(true);
    if should_patch_lang {
        env.insert("LANG".to_string(), lang_fallback.to_string());
    }
}

/// Apply a UTF-8-capable locale to subprocesses spawned from GUI hosts.
#[cfg(unix)]
pub fn apply_utf8_locale_env(cmd: &mut tokio::process::Command) {
    let mut env = std::collections::BTreeMap::new();
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.to_string(), value.into_string().unwrap_or_default());
        }
    }
    normalize_utf8_locale_env(&mut env);
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

/// Apply a UTF-8-capable locale to synchronous subprocesses spawned from GUI hosts.
#[cfg(unix)]
pub fn apply_utf8_locale_env_sync(cmd: &mut std::process::Command) {
    let mut env = std::collections::BTreeMap::new();
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.to_string(), value.into_string().unwrap_or_default());
        }
    }
    normalize_utf8_locale_env(&mut env);
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

/// Decode raw bytes captured from a subprocess's stdout / stderr.
///
/// Modern cross-platform tools (git, cargo, npm, …) emit UTF-8 even on
/// Windows, so we try strict UTF-8 first. Legacy Win32 console tools and
/// `cmd.exe` builtins (`dir`, `type`, `chcp`, …) emit *localized* strings
/// from cmd.exe's resource segment in the system's **OEM code page** —
/// CP936 (GBK) on Simplified Chinese, CP950 (Big5) on Traditional, CP932
/// (Shift-JIS) on Japanese, CP949 (UHC) on Korean — regardless of `chcp`
/// or `SetConsoleOutputCP` state, because resource strings are picked
/// before the console code page applies.
///
/// Without this fallback a Chinese-locale user running `dir` through the
/// Bash tool sees `������` mojibake: every CP936 multi-byte sequence
/// fails UTF-8 validation and `from_utf8_lossy` rewrites it as U+FFFD.
///
/// Chunk-boundary handling: if UTF-8 validation fails purely because the
/// last few bytes are an incomplete codepoint (the byte buffer landed
/// mid-character on a streaming read), the prefix is genuinely UTF-8 and
/// only the tail needs lossy replacement — don't punt the whole chunk to
/// the OEM decoder. `error_len() == None` is exactly that case.
///
/// On non-Windows, fall back to `from_utf8_lossy`: POSIX subprocess
/// stdout is UTF-8-by-convention and guessing another encoding from a
/// `LANG` value is the kind of vote we already retired for cell widths
/// (see `width::is_cjk_locale`).
pub fn decode_subprocess_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => return s.to_string(),
        // Error is "unexpected end" — chunk was sliced mid-codepoint, the
        // valid prefix is real UTF-8. Lossy decode here just inserts one
        // U+FFFD for the truncated tail; the next chunk replays the tail.
        Err(e) if e.error_len().is_none() => {
            return String::from_utf8_lossy(bytes).to_string();
        }
        Err(_) => {}
    }
    #[cfg(target_os = "windows")]
    {
        let cp = unsafe {
            extern "system" {
                fn GetOEMCP() -> u32;
            }
            GetOEMCP()
        };

        // Collect code-page candidates to try. The OEM CP reported by
        // GetOEMCP() is tried first; when it's 65001 (UTF-8, meaning the
        // "Beta: Use Unicode UTF-8 for worldwide language support" setting
        // is enabled), cmd.exe resource strings are still emitted in the
        // *original* OEM code page (e.g. 936/GBK on zh-CN, 950/Big5 on
        // zh-TW). Since from_utf8 already failed, the bytes are clearly
        // not UTF-8, so we append CJK fallbacks.
        let mut cps: Vec<u32> = vec![cp];
        if cp == 65001 {
            cps.extend_from_slice(&[936, 950, 932, 949]);
        }

        for &try_cp in &cps {
            let encoding = match try_cp {
                936 => encoding_rs::GB18030,
                950 => encoding_rs::BIG5,
                932 => encoding_rs::SHIFT_JIS,
                949 => encoding_rs::EUC_KR,
                _ => continue,
            };
            let (decoded, _, had_errors) = encoding.decode(bytes);
            if !had_errors {
                return decoded.into_owned();
            }
            // Partial decode is still better than the all-U+FFFD garbage
            // that from_utf8_lossy would produce. Accept it if at least
            // some characters decoded cleanly.
            let lossy_count = decoded.chars().filter(|&c| c == '\u{FFFD}').count();
            if lossy_count > 0 && lossy_count < decoded.chars().count() / 2 {
                return decoded.into_owned();
            }
        }

        String::from_utf8_lossy(bytes).to_string()
    }
    #[cfg(not(target_os = "windows"))]
    String::from_utf8_lossy(bytes).to_string()
}

/// Detect whether the current process is running with administrator/root
/// privileges.
///
/// - Windows: calls `CheckTokenMembership(NULL, BUILTIN\Administrators)`
///   which correctly handles UAC split-token (returns `false` when NOT
///   elevated). This is the recommended replacement for the deprecated
///   `IsUserAnAdmin()`.
/// - Unix: checks `geteuid() == 0` (root).
/// - Other platforms: returns `false` (safe default — a missed warning is
///   preferable to a false alarm).
#[cfg(target_os = "windows")]
pub fn is_running_as_admin() -> bool {
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID, SECURITY_NT_AUTHORITY,
        SID_IDENTIFIER_AUTHORITY,
    };

    unsafe {
        let mut sid: PSID = std::ptr::null_mut();
        let authority: SID_IDENTIFIER_AUTHORITY = SECURITY_NT_AUTHORITY;

        // S-1-5-32-544: BUILTIN\Administrators group.
        // Uses literal RIDs 32 (SECURITY_BUILTIN_DOMAIN_RID) and 544
        // (DOMAIN_ALIAS_RID_ADMINS) to avoid pulling in the
        // Win32_System_SystemServices feature flag.
        let result = AllocateAndInitializeSid(
            &authority, 2,   // nSubAuthorityCount
            32,  // SECURITY_BUILTIN_DOMAIN_RID
            544, // DOMAIN_ALIAS_RID_ADMINS
            0, 0, 0, 0, 0, 0, &mut sid,
        );

        if result == 0 {
            return false;
        }

        let mut is_member: i32 = 0;
        // NULL token handle = current thread's effective token
        if CheckTokenMembership(std::ptr::null_mut(), sid, &mut is_member) == 0 {
            FreeSid(sid);
            return false;
        }

        FreeSid(sid);

        is_member != 0
    }
}

#[cfg(unix)]
pub fn is_running_as_admin() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(any(target_os = "windows", unix)))]
pub fn is_running_as_admin() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn decode_passes_through_ascii() {
        assert_eq!(decode_subprocess_output(b"hello world\n"), "hello world\n");
    }

    #[test]
    fn decode_passes_through_valid_utf8() {
        assert_eq!(decode_subprocess_output("你好世界".as_bytes()), "你好世界");
    }

    #[test]
    fn decode_handles_truncated_utf8_tail_as_lossy_not_oem_decode() {
        // "你好" = E4 BD A0  E5 A5 BD. Slice off the last byte: the prefix
        // "你" is valid UTF-8, the trailing "E5 A5" is an incomplete codepoint.
        // The fix path takes the lossy branch (error_len == None) so the
        // valid prefix is preserved verbatim and only the tail becomes U+FFFD —
        // we do NOT misclassify the whole chunk as CP936 and garble the prefix.
        let full = "你好".as_bytes();
        let truncated = &full[..full.len() - 1];
        let decoded = decode_subprocess_output(truncated);
        assert!(
            decoded.starts_with('你'),
            "prefix 你 must survive: got {:?}",
            decoded
        );
    }

    #[test]
    fn decode_empty_input_is_empty_string() {
        assert_eq!(decode_subprocess_output(b""), "");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
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
    #[cfg(not(target_os = "windows"))]
    fn normalize_utf8_locale_env_preserves_existing_utf8_locale() {
        let mut env = BTreeMap::from([
            ("LC_ALL".to_string(), "zh_CN.UTF-8".to_string()),
            ("LANG".to_string(), "zh_CN.UTF-8".to_string()),
        ]);

        normalize_utf8_locale_env(&mut env);

        assert_eq!(env.get("LC_ALL").map(String::as_str), Some("zh_CN.UTF-8"));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn normalize_utf8_locale_env_sets_ctype_when_only_lang_mentions_utf8() {
        let mut env = BTreeMap::from([("LANG".to_string(), "UTF-8".to_string())]);

        normalize_utf8_locale_env(&mut env);

        assert!(env
            .get("LC_CTYPE")
            .is_some_and(|value| value.to_ascii_lowercase().contains("utf")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn normalize_utf8_locale_env_for_non_macos_replaces_bare_utf8_ctype() {
        let mut env = BTreeMap::from([
            ("LC_CTYPE".to_string(), "UTF-8".to_string()),
            ("LANG".to_string(), "C".to_string()),
        ]);

        normalize_utf8_locale_env_for_platform(&mut env, "C.UTF-8", "C.UTF-8", false);

        assert_eq!(env.get("LC_CTYPE").map(String::as_str), Some("C.UTF-8"));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn normalize_utf8_locale_env_for_macos_preserves_bare_utf8_ctype() {
        let mut env = BTreeMap::from([
            ("LC_CTYPE".to_string(), "UTF-8".to_string()),
            ("LANG".to_string(), "C".to_string()),
        ]);

        normalize_utf8_locale_env_for_platform(&mut env, "UTF-8", "en_US.UTF-8", true);

        assert_eq!(env.get("LC_CTYPE").map(String::as_str), Some("UTF-8"));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn locale_env_guard_restores_values_after_panic() {
        std::env::set_var("ATOMCODE_ENV_GUARD_TEST", "before");

        let result = std::panic::catch_unwind(|| {
            let _guard = EnvVarGuard::new(&["ATOMCODE_ENV_GUARD_TEST"]);
            std::env::set_var("ATOMCODE_ENV_GUARD_TEST", "during");
            panic!("trigger restore");
        });

        assert!(result.is_err());
        assert_eq!(
            std::env::var("ATOMCODE_ENV_GUARD_TEST").as_deref(),
            Ok("before")
        );
        std::env::remove_var("ATOMCODE_ENV_GUARD_TEST");
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn shell_command_preserves_utf8_paths_when_parent_locale_is_c() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let command = r#"
            mkdir -p "产品需求/流水线/帮助文档"
            printf 'line\n' > "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
            wc -l "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
        "#;

        let _guard = EnvVarGuard::new(&["LC_ALL", "LANG", "LC_CTYPE"]);
        std::env::set_var("LC_ALL", "C");
        std::env::set_var("LANG", "C");
        std::env::set_var("LC_CTYPE", "C");
        let output = shell_command(command)
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let stdout = decode_subprocess_output(&output.stdout);
        assert!(
            stdout.contains("产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"),
            "stdout was: {:?}",
            stdout
        );
    }

    #[test]
    #[cfg(unix)]
    fn apply_utf8_locale_env_ignores_unrelated_non_utf8_environment_variables() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let _guard = EnvVarGuard::new(&["ATOMCODE_NON_UTF8_TEST"]);
        std::env::set_var(
            "ATOMCODE_NON_UTF8_TEST",
            OsString::from_vec(vec![b'o', b'k', 0x80]),
        );

        let result = std::panic::catch_unwind(|| {
            let mut cmd = tokio::process::Command::new("sh");
            apply_utf8_locale_env(&mut cmd);
        });

        assert!(result.is_ok());
    }
}
