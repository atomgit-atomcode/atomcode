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
            while j < lines.len() && lines[j].trim().is_empty() { j += 1; }
            if j < lines.len() && lines[j].trim() == target_export.trim() {
                // Drop comment + intervening blanks + export line.
                for k in i..=j { keep[k] = false; }
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

    if !removed_any { return None; }

    let mut out = String::with_capacity(content.len());
    for (idx, line) in lines.iter().enumerate() {
        if keep[idx] {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !content.ends_with('\n') {
        if let Some(last) = out.strip_suffix('\n') { out = last.to_string(); }
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
    if !removed { return None; }
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
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new(),
    );
    let me = sysinfo::get_current_pid().ok();
    let mut out = Vec::new();
    for (pid, proc_) in sys.processes() {
        if Some(*pid) == me { continue; }
        let name = proc_.name().to_string_lossy();
        if matches_atomcode_name(&name) {
            out.push(ProcessInfo { pid: pid.as_u32(), name: name.into_owned() });
        }
    }
    out
}

/// Best-effort kill (SIGTERM on Unix, TerminateProcess on Windows) by PID.
pub fn kill_process(pid: u32) -> std::io::Result<()> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new(),
    );
    if let Some(p) = sys.process(Pid::from_u32(pid)) {
        if p.kill() {
            return Ok(());
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::Other, format!("could not kill pid {pid}")))
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
        assert_eq!(strip_atomcode_path_block(input, PREFIX).as_deref(), Some(expect));
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
        assert_eq!(out, Some(r"C:\Program Files\Git\cmd;C:\Windows".to_string()));
    }

    #[test]
    fn case_insensitive() {
        let path = r"c:\users\Theo\appdata\local\atomcode;C:\Windows";
        let out = strip_path_entry(path,
            r"C:\Users\theo\AppData\Local\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode");
        assert_eq!(out, Some(r"C:\Windows".to_string()));
    }

    #[test]
    fn ignores_trailing_backslash() {
        let path = r"C:\Users\theo\AppData\Local\AtomCode\;C:\Windows";
        let out = strip_path_entry(path,
            r"C:\Users\theo\AppData\Local\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode");
        assert_eq!(out, Some(r"C:\Windows".to_string()));
    }

    #[test]
    fn matches_unexpanded_localappdata() {
        let path = r"%LOCALAPPDATA%\AtomCode;C:\Windows";
        let out = strip_path_entry(path,
            r"%LOCALAPPDATA%\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode");
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
        let out = strip_path_entry(path,
            r"C:\Users\theo\AppData\Local\AtomCode",
            r"C:\Users\theo\AppData\Local\AtomCode");
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
