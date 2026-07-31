//! Side-effecting operations: rm, rc-file edits, Windows PATH, self-delete.

/// Remove the canonical `# Added by AtomCode installer\nexport PATH="<prefix>:$PATH"`
/// block(s) from a shell rc file's content. Strict matching: requires both
/// the comment and the export line targeting `prefix`. User-written PATH
/// lines without the comment are left alone.
///
/// Returns `Some(new_content)` if at least one block was removed,
/// `None` otherwise.
pub fn strip_atomcode_path_block(content: &str, prefix: &str) -> Option<String> {
    let comment = "# Added by AtomCode installer";
    let target_export = format!("export PATH=\"{prefix}:$PATH\"");

    let lines: Vec<&str> = content.lines().collect();
    let mut keep = vec![true; lines.len()];
    let mut removed_any = false;

    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == comment {
            // Look ahead for the export line; allow at most one blank line between.
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim() == target_export.trim() {
                // Drop comment + intervening blanks + export line.
                for flag in keep.iter_mut().take(j + 1).skip(i) {
                    *flag = false;
                }
                // Also drop one trailing blank line if present, to avoid leaving a double blank.
                if j + 1 < lines.len() && lines[j + 1].trim().is_empty() {
                    keep[j + 1] = false;
                }
                removed_any = true;
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    if !removed_any {
        return None;
    }

    let mut out = String::with_capacity(content.len());
    for (idx, line) in lines.iter().enumerate() {
        if keep[idx] {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !content.ends_with('\n') {
        if let Some(last) = out.strip_suffix('\n') {
            out = last.to_string();
        }
    }
    Some(out)
}

/// Remove an entry equal to `target_literal` (e.g. `%LOCALAPPDATA%\AtomCode`)
/// or `target_expanded` (e.g. `C:\Users\theo\AppData\Local\AtomCode`) from a
/// Windows PATH-style string. Comparison is case-insensitive and ignores
/// trailing slashes. Returns `None` if no entry matched.
pub fn strip_path_entry(path: &str, target_literal: &str, target_expanded: &str) -> Option<String> {
    let needles = [
        normalize_path_entry(target_literal),
        normalize_path_entry(target_expanded),
    ];
    let entries: Vec<&str> = path.split(';').collect();
    let mut kept = Vec::with_capacity(entries.len());
    let mut removed = false;
    for e in entries {
        let n = normalize_path_entry(e);
        if needles.iter().any(|nd| nd == &n) {
            removed = true;
            continue;
        }
        kept.push(e);
    }
    if !removed {
        return None;
    }
    Some(kept.join(";"))
}

fn normalize_path_entry(s: &str) -> String {
    let trimmed = s.trim().trim_end_matches(['\\', '/']);
    trimmed.to_ascii_lowercase()
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
}

pub fn matches_atomcode_name(name: &str) -> bool {
    let stripped = name.strip_suffix(".exe").unwrap_or(name);
    matches!(stripped, "atomcode" | "atomcode-daemon")
}

/// List all atomcode-family processes excluding the calling process.
pub fn list_atomcode_processes() -> Vec<ProcessInfo> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());
    let me = sysinfo::get_current_pid().ok();
    let mut out = Vec::new();
    for (pid, proc_) in sys.processes() {
        if Some(*pid) == me {
            continue;
        }
        let name = proc_.name().to_string_lossy();
        if matches_atomcode_name(&name) {
            out.push(ProcessInfo {
                pid: pid.as_u32(),
                name: name.into_owned(),
            });
        }
    }
    out
}

/// Best-effort kill (SIGTERM on Unix, TerminateProcess on Windows) by PID.
pub fn kill_process(pid: u32) -> std::io::Result<()> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::new());
    if let Some(p) = sys.process(Pid::from_u32(pid)) {
        if p.kill() {
            return Ok(());
        }
    }
    Err(std::io::Error::other(format!("could not kill pid {pid}")))
}

// ── Filesystem mutators ──────────────────────────────────────────────────────

use std::io;
use std::path::Path;

/// Remove a file or directory. If `needs_privilege` is true on Unix, shells
/// out to `sudo rm -rf <path>`; otherwise uses Rust stdlib.
pub fn remove_path(p: &Path, needs_privilege: bool) -> io::Result<()> {
    if !p.exists() {
        return Ok(());
    }
    if needs_privilege {
        return sudo_rm(&[p]);
    }
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

#[cfg(unix)]
pub fn sudo_rm(paths: &[&Path]) -> io::Result<()> {
    use std::process::Command;
    let status = Command::new("sudo")
        .arg("rm")
        .arg("-rf")
        .args(paths)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sudo rm failed",
        ))
    }
}

#[cfg(not(unix))]
pub fn sudo_rm(_paths: &[&Path]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Other,
        "sudo not supported on this platform",
    ))
}

#[derive(Debug)]
pub struct PathCleanupResult {
    pub modified: bool,
    pub backup_path: Option<std::path::PathBuf>,
}

/// Read an rc file, strip the AtomCode installer block targeting `prefix`,
/// write a `.atomcode-uninstall.bak` next to it, then write the cleaned file.
/// No-op (returns `modified=false`) if the file is missing or no block found.
pub fn apply_unix_path_cleanup(rc_path: &Path, prefix: &str) -> io::Result<PathCleanupResult> {
    if !rc_path.exists() {
        return Ok(PathCleanupResult {
            modified: false,
            backup_path: None,
        });
    }
    let content = std::fs::read_to_string(rc_path)?;
    let new_content = match strip_atomcode_path_block(&content, prefix) {
        Some(c) => c,
        None => {
            return Ok(PathCleanupResult {
                modified: false,
                backup_path: None,
            })
        }
    };
    let backup = {
        let mut s = rc_path.as_os_str().to_os_string();
        s.push(".atomcode-uninstall.bak");
        std::path::PathBuf::from(s)
    };
    std::fs::copy(rc_path, &backup)?;
    std::fs::write(rc_path, new_content)?;
    Ok(PathCleanupResult {
        modified: true,
        backup_path: Some(backup),
    })
}

#[cfg(windows)]
pub fn apply_windows_path_cleanup(
    install_dir_literal: &str,
    install_dir_expanded: &str,
) -> io::Result<bool> {
    use winreg::enums::*;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let env = hkcu
        .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("open Environment key: {e}")))?;
    let cur: String = env.get_value("Path").unwrap_or_default();
    let new = match strip_path_entry(&cur, install_dir_literal, install_dir_expanded) {
        Some(s) => s,
        None => return Ok(false),
    };
    env.set_value("Path", &new)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("write Path: {e}")))?;
    broadcast_setting_change();
    Ok(true)
}

#[cfg(windows)]
fn broadcast_setting_change() {
    use std::ffi::CString;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutA, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };
    let env = CString::new("Environment").unwrap();
    unsafe {
        let mut result: usize = 0;
        SendMessageTimeoutA(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            env.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
}

// ── Self-delete strategy ─────────────────────────────────────────────────────

/// Strategy abstraction so tests can override the actual self-delete step.
pub trait SelfDeleteStrategy {
    fn run(&self, exe: &Path) -> io::Result<()>;
}

pub struct PlatformSelfDelete;

impl SelfDeleteStrategy for PlatformSelfDelete {
    #[cfg(unix)]
    fn run(&self, exe: &Path) -> io::Result<()> {
        // POSIX: we can unlink ourselves; the inode lives until exit.
        if let Some(parent) = exe.parent() {
            // If parent is not effectively writable, sudo it.
            use std::os::unix::ffi::OsStrExt;
            let parent_c = std::ffi::CString::new(parent.as_os_str().as_bytes())
                .unwrap_or_else(|_| std::ffi::CString::new(".").unwrap());
            let writable = unsafe { libc::access(parent_c.as_ptr(), libc::W_OK) == 0 };
            if !writable {
                return sudo_rm(&[exe]);
            }
        }
        std::fs::remove_file(exe)
    }

    #[cfg(windows)]
    fn run(&self, exe: &Path) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        // Rename live exe to .atomcode.rolling so the install dir can be deleted.
        let rolling = atomcode_updater::rolling_path(exe);
        if exe.file_name() != rolling.file_name() {
            let _ = std::fs::rename(exe, &rolling);
        }
        let install_dir = exe
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no parent dir"))?;
        let dir_str = install_dir.to_string_lossy().to_string();

        // Use `timeout` for the delay instead of `ping` — it is semantically
        // clearer and avoids the "cmd window flashing ping 127.0.0.1" bug
        // reported in gitcode.com/atomgit_atomcode/atomcode/issues/352.
        // CREATE_NO_WINDOW prevents the console window from appearing at all
        // (DETACHED_PROCESS does NOT reliably hide the window on Win10).
        let cmd_arg = format!("timeout /t 2 /nobreak >nul & rmdir /S /Q \"{}\"", dir_str);
        Command::new("cmd")
            .args(["/C", &cmd_arg])
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn run(&self, exe: &Path) -> io::Result<()> {
        std::fs::remove_file(exe)
    }
}

/// Test stub strategy used by integration tests to avoid actually self-deleting.
pub struct NoopSelfDelete;

impl SelfDeleteStrategy for NoopSelfDelete {
    fn run(&self, _exe: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod path_line_tests {
    use super::strip_atomcode_path_block;

    const PREFIX: &str = "/Users/test/.local/bin";

    #[test]
    fn strips_canonical_block() {
        let input = "\
# user stuff
alias gs=\"git status\"

# Added by AtomCode installer
export PATH=\"/Users/test/.local/bin:$PATH\"

# more user stuff
";
        let expect = "\
# user stuff
alias gs=\"git status\"

# more user stuff
";
        assert_eq!(
            strip_atomcode_path_block(input, PREFIX).as_deref(),
            Some(expect)
        );
    }

    #[test]
    fn returns_none_when_no_block() {
        let input = "alias gs=\"git status\"\n";
        assert_eq!(strip_atomcode_path_block(input, PREFIX), None);
    }

    #[test]
    fn strips_multiple_blocks_from_repeat_installs() {
        let input = "\
# Added by AtomCode installer
export PATH=\"/Users/test/.local/bin:$PATH\"

alias x=1

# Added by AtomCode installer
export PATH=\"/Users/test/.local/bin:$PATH\"
";
        let out = strip_atomcode_path_block(input, PREFIX).unwrap();
        assert!(!out.contains("AtomCode installer"));
        assert!(out.contains("alias x=1"));
    }

    #[test]
    fn does_not_touch_user_written_path_lines() {
        let input = "\
export PATH=\"/Users/test/.local/bin:$PATH\"
# unrelated comment
";
        // No installer comment → must return None even though prefix matches.
        assert_eq!(strip_atomcode_path_block(input, PREFIX), None);
    }

    #[test]
    fn ignores_block_with_different_prefix() {
        let input = "\
# Added by AtomCode installer
export PATH=\"/opt/somewhere/else:$PATH\"
";
        assert_eq!(strip_atomcode_path_block(input, PREFIX), None);
    }

    #[test]
    fn handles_block_at_end_of_file() {
        let input = "alias x=1\n\n# Added by AtomCode installer\nexport PATH=\"/Users/test/.local/bin:$PATH\"\n";
        let out = strip_atomcode_path_block(input, PREFIX).unwrap();
        assert_eq!(out.trim_end(), "alias x=1");
    }
}

#[cfg(test)]
mod windows_path_tests {
    use super::strip_path_entry;

    #[test]
    fn strips_exact_match() {
        let path = r"C:\Program Files\Git\cmd;C:\Users\theo\AppData\Local\AtomCode;C:\Windows";
        let target = r"C:\Users\theo\AppData\Local\AtomCode";
        let expanded = r"C:\Users\theo\AppData\Local\AtomCode";
        let out = strip_path_entry(path, target, expanded);
        assert_eq!(
            out,
            Some(r"C:\Program Files\Git\cmd;C:\Windows".to_string())
        );
    }

    #[test]
    fn case_insensitive() {
        let path = r"c:\users\Theo\appdata\local\atomcode;C:\Windows";
        let out = strip_path_entry(
            path,
            r"C:\Users\theo\AppData\Local\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode",
        );
        assert_eq!(out, Some(r"C:\Windows".to_string()));
    }

    #[test]
    fn ignores_trailing_backslash() {
        let path = r"C:\Users\theo\AppData\Local\AtomCode\;C:\Windows";
        let out = strip_path_entry(
            path,
            r"C:\Users\theo\AppData\Local\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode",
        );
        assert_eq!(out, Some(r"C:\Windows".to_string()));
    }

    #[test]
    fn matches_unexpanded_localappdata() {
        let path = r"%LOCALAPPDATA%\AtomCode;C:\Windows";
        let out = strip_path_entry(
            path,
            r"%LOCALAPPDATA%\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode",
        );
        assert!(out.unwrap().eq_ignore_ascii_case(r"C:\Windows"));
    }

    #[test]
    fn returns_none_when_not_present() {
        let path = r"C:\Windows;C:\Program Files\Git\cmd";
        let out = strip_path_entry(path, r"C:\nope", r"C:\nope");
        assert_eq!(out, None);
    }

    #[test]
    fn preserves_other_atomcode_substring_entries() {
        // A directory that *contains* AtomCode in its name but isn't the install dir.
        let path = r"C:\AtomCodeStuff\bin;C:\Users\theo\AppData\Local\AtomCode;C:\Windows";
        let out = strip_path_entry(
            path,
            r"C:\Users\theo\AppData\Local\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode",
        );
        assert_eq!(out, Some(r"C:\AtomCodeStuff\bin;C:\Windows".to_string()));
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    #[test]
    fn excludes_self() {
        let me = std::process::id();
        let procs = list_atomcode_processes();
        for p in procs {
            assert_ne!(p.pid, me);
        }
    }

    #[test]
    fn name_matcher_recognizes_atomcode_variants() {
        assert!(matches_atomcode_name("atomcode"));
        assert!(matches_atomcode_name("atomcode.exe"));
        assert!(matches_atomcode_name("atomcode-daemon"));
        assert!(matches_atomcode_name("atomcode-daemon.exe"));
        assert!(!matches_atomcode_name("vscode"));
        assert!(!matches_atomcode_name("atomcode-stuff"));
    }
}

#[cfg(test)]
mod remove_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn removes_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x");
        std::fs::write(&p, b"hi").unwrap();
        remove_path(&p, false).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn removes_dir_recursively() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("a"), b"a").unwrap();
        remove_path(&d, false).unwrap();
        assert!(!d.exists());
    }

    #[test]
    fn nonexistent_path_is_ok() {
        let tmp = TempDir::new().unwrap();
        remove_path(&tmp.path().join("missing"), false).unwrap();
    }
}

#[cfg(test)]
mod rc_apply_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn backup_created_and_block_removed() {
        let tmp = TempDir::new().unwrap();
        let rc = tmp.path().join(".zshrc");
        std::fs::write(
            &rc,
            "# Added by AtomCode installer\nexport PATH=\"/p:$PATH\"\n",
        )
        .unwrap();
        let res = apply_unix_path_cleanup(&rc, "/p").unwrap();
        assert!(res.modified);
        assert!(rc.with_file_name(".zshrc.atomcode-uninstall.bak").exists());
        let new = std::fs::read_to_string(&rc).unwrap();
        assert!(!new.contains("AtomCode"));
    }

    #[test]
    fn no_change_when_block_absent() {
        let tmp = TempDir::new().unwrap();
        let rc = tmp.path().join(".zshrc");
        std::fs::write(&rc, "alias x=1\n").unwrap();
        let res = apply_unix_path_cleanup(&rc, "/p").unwrap();
        assert!(!res.modified);
        assert!(!rc.with_file_name(".zshrc.atomcode-uninstall.bak").exists());
    }
}

#[cfg(test)]
mod execute_tests {
    use super::super::{execute, scan, Decisions};
    use super::NoopSelfDelete;
    use tempfile::TempDir;

    fn fake_install(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let exe = tmp.path().join("atomcode");
        std::fs::write(&exe, b"x").unwrap();
        let data = tmp.path().join(".atomcode");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("auth.toml"), b"k").unwrap();
        std::fs::write(data.join("history"), b"h").unwrap();
        std::fs::create_dir(data.join("plugins")).unwrap();
        (exe, data)
    }

    #[test]
    fn keep_data_only_removes_binary() {
        let tmp = TempDir::new().unwrap();
        let (exe, data) = fake_install(&tmp);
        let plan = scan::scan(&exe, &data).unwrap();
        let outcome = execute(&plan, Decisions::KEEP_DATA, &NoopSelfDelete, None).unwrap();
        // NoopSelfDelete doesn't actually delete the file, but execute() records it.
        // Our assertions: data files preserved.
        assert!(data.join("auth.toml").exists());
        assert!(data.join("history").exists());
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn purge_removes_everything_under_data() {
        let tmp = TempDir::new().unwrap();
        let (exe, data) = fake_install(&tmp);
        let plan = scan::scan(&exe, &data).unwrap();
        execute(&plan, Decisions::PURGE, &NoopSelfDelete, None).unwrap();
        assert!(!data.join("auth.toml").exists());
        assert!(!data.join("history").exists());
        assert!(!data.join("plugins").exists());
    }

    #[test]
    fn defaults_keep_credentials_remove_state() {
        let tmp = TempDir::new().unwrap();
        let (exe, data) = fake_install(&tmp);
        let plan = scan::scan(&exe, &data).unwrap();
        execute(&plan, Decisions::DEFAULTS, &NoopSelfDelete, None).unwrap();
        assert!(data.join("auth.toml").exists()); // kept
        assert!(!data.join("history").exists());
        assert!(!data.join("plugins").exists());
    }

    #[test]
    fn execution_order_state_then_credentials_then_binary() {
        let tmp = TempDir::new().unwrap();
        let (exe, data) = fake_install(&tmp);
        let plan = scan::scan(&exe, &data).unwrap();
        let outcome = execute(&plan, Decisions::PURGE, &NoopSelfDelete, None).unwrap();
        // Removed list ordering proves the spec-mandated order.
        // history (state) must appear before auth.toml (credentials), which
        // must appear before the binary path itself.
        let pos_history = outcome
            .removed
            .iter()
            .position(|p| p.file_name().and_then(|n| n.to_str()) == Some("history"))
            .expect("history was not removed");
        let pos_auth = outcome
            .removed
            .iter()
            .position(|p| p.file_name().and_then(|n| n.to_str()) == Some("auth.toml"))
            .expect("auth.toml was not removed");
        let pos_bin = outcome
            .removed
            .iter()
            .position(|p| p == &exe)
            .expect("binary was not removed");
        assert!(
            pos_history < pos_auth,
            "state should be removed before credentials"
        );
        assert!(
            pos_auth < pos_bin,
            "credentials should be removed before binary"
        );
    }
}
