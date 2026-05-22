//! Session-start environment snapshot (C1 in CC's taxonomy).
//!
//! Captures git branch / HEAD / working-tree status **once** at session
//! start (or on `ChangeDir`), memoized by ownership: the struct lives on
//! `AgentLoop`, is constructed in `new()`, and renders into
//! `build_system_prompt` as a dedicated section.
//!
//! ## Why snapshot, not live
//!
//! Git status changes every turn (every tool call shifts working tree
//! state). If we re-read each turn, the system-prompt prefix changes
//! every turn → prompt cache breaks → every turn re-bills full prefix.
//! CC observed this tradeoff and chose snapshot-at-session-start with an
//! explicit disclaimer in prompt text, so the model doesn't mistake
//! stale info for live state. We follow that pattern.
//!
//! ## Graceful degradation
//!
//! Non-git directories, no git binary on PATH, detached HEAD, permission
//! errors — all surface as `git: None` and the prompt section is simply
//! omitted. No panics, no half-rendered sections.

use std::path::Path;
use std::process::Command;

/// Git repository state at the moment of capture.
#[derive(Debug, Clone, Default)]
pub struct GitSnapshot {
    /// Current branch name. `None` in detached-HEAD state.
    pub branch: Option<String>,
    /// One-line HEAD description: `"<short-sha> <subject>"`.
    pub head_oneline: Option<String>,
    /// `git status --short` output, truncated to [`STATUS_MAX_LINES`] lines.
    pub status_short: String,
    /// `true` when the working tree has uncommitted changes / untracked files.
    pub is_dirty: bool,
}

/// Hard cap on `git status --short` lines rendered into prompt.
/// Prevents a repo with 500 untracked files from blowing up system prompt.
const STATUS_MAX_LINES: usize = 20;

/// Session-start environment snapshot.
///
/// Only git is captured today. Extend by adding fields (e.g. `rust_toolchain`,
/// `node_version`) and rendering them in [`as_prompt_section`].
#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot {
    pub git: Option<GitSnapshot>,
}

impl EnvSnapshot {
    /// Capture the current git state at `wd`. Blocking I/O — acceptable
    /// at session start / `ChangeDir`, which run at most a few times per
    /// process. Returns an empty snapshot (`git: None`) on any failure.
    pub fn capture(wd: &Path) -> Self {
        // First gate: is `wd` inside a git work tree at all? This avoids
        // stderr spam in plain directories where every `git` subcommand
        // would complain identically.
        if !is_git_repo(wd) {
            return Self::default();
        }

        let branch = run_git(wd, &["branch", "--show-current"]).filter(|s| !s.is_empty());

        let head_oneline = run_git(wd, &["log", "-1", "--format=%h %s"]).filter(|s| !s.is_empty());

        let status_raw = run_git(wd, &["status", "--short"]).unwrap_or_default();
        let is_dirty = !status_raw.trim().is_empty();

        // Truncate aggressively — long status (e.g. first clone with many
        // untracked files) shouldn't dominate the system prompt.
        let status_short = truncate_status(&status_raw, STATUS_MAX_LINES);

        Self {
            git: Some(GitSnapshot {
                branch,
                head_oneline,
                status_short,
                is_dirty,
            }),
        }
    }

    /// Render as a `=== GIT STATUS ===` block for splicing into the
    /// system prompt. Returns `String::new()` when no git state exists
    /// (so the caller can unconditionally push the result without
    /// checking — empty string is a no-op).
    ///
    /// Format (matches CC's snapshot disclaimer pattern):
    ///
    /// ```text
    /// === GIT STATUS (snapshot at session start, not live) ===
    /// Branch: main
    /// HEAD:   06f4537 feat(tuix): /context 命令
    /// Status: 3 files modified
    ///  M src/foo.rs
    ///  M src/bar.rs
    /// ?? new.rs
    ///
    /// This is a snapshot from session start. Use `bash` + `git status`
    /// to check live state.
    /// ```
    pub fn as_prompt_section(&self) -> String {
        let Some(git) = self.git.as_ref() else {
            return String::new();
        };

        let mut out = String::from("\n=== GIT STATUS (snapshot at session start, not live) ===\n");

        if let Some(branch) = git.branch.as_deref() {
            out.push_str(&format!("Branch: {}\n", branch));
        } else {
            out.push_str("Branch: (detached HEAD)\n");
        }

        if let Some(head) = git.head_oneline.as_deref() {
            out.push_str(&format!("HEAD:   {}\n", head));
        }

        if git.is_dirty {
            let line_count = git.status_short.lines().count();
            out.push_str(&format!("Status: {} change(s)\n", line_count));
            out.push_str(&git.status_short);
            if !git.status_short.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str("Status: clean\n");
        }

        out.push_str(
            "\nThis is a snapshot from session start. \
             Use `bash` + `git status` to check live state.\n",
        );

        out
    }
}

/// True when `wd` is inside a git working tree (checked via
/// `git rev-parse --is-inside-work-tree`). Returns `false` on any
/// error or when stdout isn't the literal `"true"`.
fn is_git_repo(wd: &Path) -> bool {
    run_git(wd, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Run `git <args>` in `wd`, return trimmed stdout on exit-0. `None` on
/// any failure (git missing, non-zero exit, non-UTF8 output). stderr is
/// intentionally discarded — this is best-effort context enrichment and
/// error spam doesn't help the user.
fn run_git(wd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(wd)
        // Suppress paging in case user has `pager.*` configured.
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    crate::process_utils::suppress_console_window_sync(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    Some(s.trim().to_string())
}

/// Truncate `git status --short` output to at most `max_lines` lines.
/// When truncated, appends a summary line indicating how many were
/// omitted so the model doesn't think the repo is smaller than it is.
fn truncate_status(raw: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= max_lines {
        return raw.to_string();
    }
    let kept: Vec<&str> = lines.iter().take(max_lines).copied().collect();
    format!(
        "{}\n... and {} more line(s)\n",
        kept.join("\n"),
        lines.len() - max_lines
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_renders_nothing() {
        let snap = EnvSnapshot::default();
        assert_eq!(snap.as_prompt_section(), "");
    }

    #[test]
    fn capture_non_git_dir_returns_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snap = EnvSnapshot::capture(tmp.path());
        assert!(snap.git.is_none(), "non-git dir must yield git: None");
        assert_eq!(snap.as_prompt_section(), "");
    }

    #[test]
    fn snapshot_with_branch_renders_section() {
        let snap = EnvSnapshot {
            git: Some(GitSnapshot {
                branch: Some("main".into()),
                head_oneline: Some("abc1234 test commit".into()),
                status_short: String::new(),
                is_dirty: false,
            }),
        };
        let out = snap.as_prompt_section();
        assert!(out.contains("=== GIT STATUS"));
        assert!(out.contains("snapshot at session start"));
        assert!(out.contains("Branch: main"));
        assert!(out.contains("HEAD:   abc1234 test commit"));
        assert!(out.contains("Status: clean"));
        // Disclaimer at the end
        assert!(out.contains("Use `bash` + `git status` to check live state"));
    }

    #[test]
    fn detached_head_shown_explicitly() {
        let snap = EnvSnapshot {
            git: Some(GitSnapshot {
                branch: None, // detached
                head_oneline: Some("deadbee detached state".into()),
                status_short: String::new(),
                is_dirty: false,
            }),
        };
        let out = snap.as_prompt_section();
        assert!(out.contains("(detached HEAD)"));
    }

    #[test]
    fn dirty_status_includes_changes() {
        let snap = EnvSnapshot {
            git: Some(GitSnapshot {
                branch: Some("feat/x".into()),
                head_oneline: Some("abc1234 wip".into()),
                status_short: " M src/foo.rs\n M src/bar.rs\n?? new.rs".into(),
                is_dirty: true,
            }),
        };
        let out = snap.as_prompt_section();
        assert!(out.contains("Status: 3 change(s)"));
        assert!(out.contains(" M src/foo.rs"));
        assert!(out.contains("?? new.rs"));
        assert!(!out.contains("Status: clean"));
    }

    #[test]
    fn truncate_status_caps_long_output() {
        let raw = (0..50)
            .map(|i| format!(" M file_{}.rs", i))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_status(&raw, 20);
        let kept_lines = out.lines().filter(|l| l.starts_with(" M")).count();
        assert_eq!(kept_lines, 20);
        assert!(out.contains("... and 30 more line"));
    }

    #[test]
    fn truncate_status_passthrough_when_under_cap() {
        let raw = " M a.rs\n M b.rs";
        let out = truncate_status(raw, 20);
        assert_eq!(out, raw);
        assert!(!out.contains("more line"));
    }
}
