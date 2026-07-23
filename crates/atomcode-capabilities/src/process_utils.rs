//! Platform-specific process utilities (console-window suppression, shell-command
//! construction, UTF-8 locale, admin check) — the shared L1 home now that
//! `capabilities` must not depend on `core`. Consumed here plus by the CLI/TUI drivers.
//!
//! `shell_command` + `is_running_as_admin` also exist in `atomcode_core::process_utils`
//! for core's remaining hook consumers. Consolidate that copy when those consumers migrate.
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
pub fn suppress_console_window(cmd: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub fn suppress_console_window(_cmd: &mut tokio::process::Command) {}

/// Apply `CREATE_NO_WINDOW` to a `std::process::Command` on Windows; no-op
/// elsewhere. The std `creation_flags` requires `CommandExt` in scope.
#[cfg(target_os = "windows")]
pub fn suppress_console_window_sync(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub fn suppress_console_window_sync(_cmd: &mut std::process::Command) {}

/// RAII owner of a Windows Job Object configured with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Every process assigned to the job —
/// and every process THOSE spawn — dies when either [`JobHandle::terminate`]
/// runs or the last handle to the job closes (this guard dropping, INCLUDING
/// when the atomcode process itself is killed and the OS closes its handles).
///
/// Why: on Windows the Bash tool's only cleanup is `kill_on_drop`, which
/// terminates the direct child (`cmd.exe` / Git Bash) but NOT its descendants.
/// A timed-out `mvn compile` orphans the `java` compiler JVM (and pipeline
/// sub-shells / busybox applets); the JVM keeps burning CPU and holds `target/`
/// locks, so the next compile is slower and also times out → a runaway that
/// pins the machine. The job makes the whole tree reapable in one call and,
/// via kill-on-close, guarantees nothing survives atomcode itself.
#[cfg(target_os = "windows")]
pub struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

// The HANDLE is an opaque kernel handle; safe to move/share across the threads
// of tokio's multithreaded runtime.
#[cfg(target_os = "windows")]
unsafe impl Send for JobHandle {}
#[cfg(target_os = "windows")]
unsafe impl Sync for JobHandle {}

#[cfg(target_os = "windows")]
impl JobHandle {
    /// Terminate every process in the job (children and grandchildren).
    /// Idempotent — a job whose processes already exited terminates to a no-op.
    pub fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if !self.0.is_null() {
            // Exit code 1: the tree was force-killed, not a clean exit.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for JobHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        if !self.0.is_null() {
            // KILL_ON_JOB_CLOSE: closing the last handle terminates whatever is
            // still in the job — reaping the tree on cancel (the wait future is
            // dropped) or on an atomcode crash/kill.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Assign `child` (and everything it later spawns) to a fresh kill-on-close Job
/// Object; return the guard to hold for the child's lifetime. `None` if any
/// Win32 call fails — the caller then relies on the pre-existing direct-child
/// `kill_on_drop`, which is no worse than before.
///
/// Tiny race: a grandchild spawned in the microseconds between `CreateProcess`
/// (inside `spawn`) and `AssignProcessToJobObject` here escapes the job. In
/// practice the shell takes milliseconds to initialise before it spawns
/// `mvn`/pipeline children, so assigning immediately after spawn captures them.
/// Fully closing the window would need `CREATE_SUSPENDED` + `ResumeThread`,
/// which tokio's `Child` doesn't expose.
#[cfg(target_os = "windows")]
pub fn assign_child_to_kill_on_close_job(child: &tokio::process::Child) -> Option<JobHandle> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let proc_handle = child.raw_handle()? as HANDLE;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set_ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set_ok == 0 || AssignProcessToJobObject(job, proc_handle) == 0 {
            CloseHandle(job);
            return None;
        }
        Some(JobHandle(job))
    }
}

/// Best-effort fallback tree-kill for the rare case the Job Object couldn't be
/// created/assigned (so [`assign_child_to_kill_on_close_job`] returned `None`).
/// `taskkill /T` walks the live parent→child tree and force-kills all of it —
/// a defense-in-depth net so a job-setup failure still doesn't orphan a runaway
/// `mvn`/`java`. Fire-and-forget: spawned console-suppressed and not awaited (on
/// Windows a dropped `Child` handle leaves no zombie).
#[cfg(target_os = "windows")]
pub fn taskkill_tree(pid: u32) {
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
    suppress_console_window_sync(&mut cmd);
    let _ = cmd.spawn();
}

/// Force-kill a shell's whole descendant tree on Windows: terminate the Job
/// Object if it was set up, else fall back to `taskkill /T` rooted at `pid`
/// (the direct child). The single entry point every Bash spawn path calls, so
/// their Windows cleanup can't drift apart. `pid` is `None` only if the child
/// was already reaped (nothing to kill).
#[cfg(target_os = "windows")]
pub fn kill_windows_tree(job: &Option<JobHandle>, pid: Option<u32>) {
    match job {
        Some(job) => job.terminate(),
        None => {
            if let Some(pid) = pid {
                taskkill_tree(pid);
            }
        }
    }
}

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
