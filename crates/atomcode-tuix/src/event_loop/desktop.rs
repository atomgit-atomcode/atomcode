//! `/desktop` support: detect an installed AtomCode desktop app and launch it,
//! or point the user at the download page. Detection is path-based and lives in
//! the pure `candidate_apps` list so per-OS names are a one-line change.

use std::path::{Path, PathBuf};

/// Releases page for the desktop app (old "Air" + new "Desktop" builds).
pub const DOWNLOAD_URL: &str =
    "https://atomgit.com/atomgit_atomcode/atomCode-air-releases/releases";

/// How to launch a resolved candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)] // each variant is constructed only on its own OS (cfg-gated in candidate_apps)
pub enum LaunchKind {
    /// macOS `.app` bundle — launched with `open <bundle>`.
    MacOpen,
    /// A concrete executable (Windows `.exe` / Linux binary) — spawned directly.
    Spawn,
}

/// One possible install location + how to launch it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Human name shown in messages ("AtomCode Desktop" / "AtomCode Air").
    pub display_name: &'static str,
    /// The path whose existence means "installed" (bundle / exe / binary).
    pub path: PathBuf,
    pub launch: LaunchKind,
}

/// Ordered candidate installs for the CURRENT OS. New "Desktop" builds come
/// before old "Air" builds so a machine with both opens the new one. `env` is
/// injected (`|k| std::env::var(k).ok()` in production) so the list is testable.
#[cfg(target_os = "macos")]
pub fn candidate_apps(home: &Path, _env: &impl Fn(&str) -> Option<String>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (name, bundle) in [
        ("AtomCode Desktop", "AtomCode Desktop.app"),
        ("AtomCode Air", "AtomCode Air.app"),
    ] {
        out.push(Candidate {
            display_name: name,
            path: PathBuf::from("/Applications").join(bundle),
            launch: LaunchKind::MacOpen,
        });
        out.push(Candidate {
            display_name: name,
            path: home.join("Applications").join(bundle),
            launch: LaunchKind::MacOpen,
        });
    }
    out
}

/// Best-effort Windows locations — VERIFY names on a real machine.
#[cfg(target_os = "windows")]
pub fn candidate_apps(_home: &Path, env: &impl Fn(&str) -> Option<String>) -> Vec<Candidate> {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(v) = env("LOCALAPPDATA") {
        bases.push(PathBuf::from(v).join("Programs"));
    }
    for k in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(v) = env(k) {
            bases.push(PathBuf::from(v));
        }
    }
    let mut out = Vec::new();
    for (name, dir, exe) in [
        (
            "AtomCode Desktop",
            "AtomCode Desktop",
            "AtomCode Desktop.exe",
        ),
        ("AtomCode Air", "AtomCode Air", "AtomCode Air.exe"),
    ] {
        for base in &bases {
            out.push(Candidate {
                display_name: name,
                path: base.join(dir).join(exe),
                launch: LaunchKind::Spawn,
            });
        }
    }
    out
}

/// Best-effort Linux locations — VERIFY names on a real machine.
#[cfg(target_os = "linux")]
pub fn candidate_apps(_home: &Path, env: &impl Fn(&str) -> Option<String>) -> Vec<Candidate> {
    let mut out = Vec::new();
    let path_var = env("PATH").unwrap_or_default();
    for (name, bin) in [
        ("AtomCode Desktop", "atomcode-desktop"),
        ("AtomCode Air", "atomcode-air"),
    ] {
        for dir in std::env::split_paths(&path_var) {
            out.push(Candidate {
                display_name: name,
                path: dir.join(bin),
                launch: LaunchKind::Spawn,
            });
        }
        out.push(Candidate {
            display_name: name,
            path: PathBuf::from("/opt").join(name).join(bin),
            launch: LaunchKind::Spawn,
        });
    }
    out
}

/// Fallback for unsupported OSes: nothing detected → the command shows the URL.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub fn candidate_apps(_home: &Path, _env: &impl Fn(&str) -> Option<String>) -> Vec<Candidate> {
    Vec::new()
}

/// First candidate whose `path` passes `probe`. Production passes
/// `|p| p.exists()`; tests pass a fake set so no disk is touched.
pub fn detect<'a>(
    candidates: &'a [Candidate],
    probe: impl Fn(&Path) -> bool,
) -> Option<&'a Candidate> {
    candidates.iter().find(|c| probe(&c.path))
}

/// Spawn-and-detach launch of a resolved candidate. Never blocks the TUI; the
/// child's stdio is nulled so it can't scribble on the terminal.
pub fn launch(c: &Candidate) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = match c.launch {
        LaunchKind::MacOpen => {
            let mut c2 = Command::new("open");
            c2.arg(&c.path);
            c2
        }
        LaunchKind::Spawn => Command::new(&c.path),
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // On Windows, spawning the .exe directly would flash a console window;
    // suppress it with CREATE_NO_WINDOW (no-op on other platforms). Mirrors
    // what `atomcode_core::tool::open_file` does for the same reason.
    atomcode_capabilities::process_utils::suppress_console_window_sync(&mut cmd);
    cmd.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &'static str, path: &str) -> Candidate {
        Candidate {
            display_name: name,
            path: PathBuf::from(path),
            launch: LaunchKind::Spawn,
        }
    }

    #[test]
    fn detect_returns_first_existing() {
        let cands = vec![
            cand("AtomCode Desktop", "/a/desktop"),
            cand("AtomCode Air", "/b/air"),
        ];
        // Only the Air path "exists".
        let hit = detect(&cands, |p| p == Path::new("/b/air"));
        assert_eq!(hit.map(|c| c.display_name), Some("AtomCode Air"));
    }

    #[test]
    fn detect_prefers_earlier_candidate_when_both_exist() {
        let cands = vec![
            cand("AtomCode Desktop", "/a/desktop"),
            cand("AtomCode Air", "/b/air"),
        ];
        let hit = detect(&cands, |_| true); // both exist → first wins
        assert_eq!(hit.map(|c| c.display_name), Some("AtomCode Desktop"));
    }

    #[test]
    fn detect_none_when_nothing_exists() {
        let cands = vec![cand("AtomCode Desktop", "/a/desktop")];
        assert!(detect(&cands, |_| false).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_candidates_desktop_before_air_and_cover_both_roots() {
        let home = PathBuf::from("/Users/tester");
        let env = |_: &str| None;
        let cands = candidate_apps(&home, &env);
        let paths: Vec<String> = cands.iter().map(|c| c.path.display().to_string()).collect();
        assert_eq!(
            paths,
            vec![
                "/Applications/AtomCode Desktop.app".to_string(),
                "/Users/tester/Applications/AtomCode Desktop.app".to_string(),
                "/Applications/AtomCode Air.app".to_string(),
                "/Users/tester/Applications/AtomCode Air.app".to_string(),
            ]
        );
        assert!(cands.iter().all(|c| c.launch == LaunchKind::MacOpen));
    }
}
