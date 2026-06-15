//! Three-tier project-instructions loader (global / project / user) — the v2 port of
//! v1 `config/instructions.rs`. Each tier is a markdown file injected into the session
//! context block under a header carrying its source path. Pure (paths in, string out) so
//! it is testable without touching the real home dir; [`SessionContextHook`] supplies the
//! config-root and project paths.
//!
//! [`SessionContextHook`]: super::context::SessionContextHook

use std::path::{Path, PathBuf};

/// Per-tier size cap (v1 parity): a runaway instructions file can't blow the prompt.
const MAX_INSTRUCTIONS_BYTES: usize = 1024 * 1024; // 1 MiB

/// Project-root filenames checked IN ORDER for the project tier; the FIRST existing wins.
/// Includes the ecosystem names (`AGENTS.md`, `CLAUDE.md`) so a repo's existing agent
/// instructions are honored (matches v1).
const PROJECT_NAMES: [&str; 5] =
    [".atomcode.md", "ATOMCODE.md", "AGENTS.md", "CLAUDE.md", "claude.md"];

/// Render the global / project / user instruction tiers as headered blocks joined by blank
/// lines. Empty string when none exist (the caller then omits the section). `home` = the
/// config root (`~/.atomcode`); `project` = the workspace root.
pub fn render_instructions(home: &Path, project: &Path) -> String {
    let mut out: Vec<String> = Vec::new();
    let global = home.join("ATOMCODE.md");
    if let Some(body) = read_tier(&global) {
        out.push(format!("=== GLOBAL INSTRUCTIONS ({}) ===\n{body}", global.display()));
    }
    if let Some(proj) = project_file(project) {
        if let Some(body) = read_tier(&proj) {
            out.push(format!("=== PROJECT INSTRUCTIONS ({}) ===\n{body}", proj.display()));
        }
    }
    let user = project.join(".atomcode.user.md");
    if let Some(body) = read_tier(&user) {
        out.push(format!("=== USER INSTRUCTIONS ({}) ===\n{body}", user.display()));
    }
    out.join("\n\n")
}

/// The first existing project-tier file (precedence order), if any.
fn project_file(project: &Path) -> Option<PathBuf> {
    PROJECT_NAMES.iter().map(|n| project.join(n)).find(|p| p.is_file())
}

/// Read a tier file → its trimmed body, or `None` if missing/non-file/empty. Capped at
/// [`MAX_INSTRUCTIONS_BYTES`] (truncated with a marker).
fn read_tier(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if body.len() > MAX_INSTRUCTIONS_BYTES {
        let cut: String = body.chars().take(MAX_INSTRUCTIONS_BYTES).collect();
        Some(format!("{cut}\n[truncated: instructions file exceeds 1 MiB]"))
    } else {
        Some(body.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn project_tier_picks_first_existing_name() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path();
        // AGENTS.md and CLAUDE.md both exist → AGENTS.md (earlier in precedence) wins.
        fs::write(proj.join("AGENTS.md"), "agents rules").unwrap();
        fs::write(proj.join("CLAUDE.md"), "claude rules").unwrap();
        let out = render_instructions(&d.path().join("nohome"), proj);
        assert!(out.contains("PROJECT INSTRUCTIONS"));
        assert!(out.contains("agents rules"));
        assert!(!out.contains("claude rules"), "first-precedence file wins");
        assert!(out.contains("AGENTS.md"), "header carries the source path");
    }

    #[test]
    fn global_and_user_tiers_included() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("ATOMCODE.md"), "global rules").unwrap();
        let proj = d.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join(".atomcode.user.md"), "user rules").unwrap();
        let out = render_instructions(&home, &proj);
        assert!(out.contains("GLOBAL INSTRUCTIONS") && out.contains("global rules"));
        assert!(out.contains("USER INSTRUCTIONS") && out.contains("user rules"));
    }

    #[test]
    fn missing_and_empty_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("AGENTS.md"), "   \n  ").unwrap(); // whitespace-only → empty
        let out = render_instructions(&d.path().join("nohome"), &proj);
        assert!(out.is_empty(), "no non-empty tiers → empty: {out:?}");
    }

    #[test]
    fn oversized_file_is_truncated() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let big = "x".repeat(MAX_INSTRUCTIONS_BYTES + 100);
        fs::write(proj.join("AGENTS.md"), &big).unwrap();
        let out = render_instructions(&d.path().join("nohome"), &proj);
        assert!(out.contains("[truncated:"), "oversized tier truncated");
    }
}
