//! `SessionContextHook` — injects the per-session "context block" (environment + project
//! instructions + git snapshot) as ONE leading `Role::System` message at session start,
//! the v2 port of v1's runtime-injected prompt sections (`agent/prompt.rs`).
//!
//! Cache-safe: the block is read ONCE at session start and frozen — a SNAPSHOT of where
//! the session began, not a live view (the git section says so explicitly). The wire
//! adapter then coalesces persona + this block + memory into a single system message
//! (commit `3956f9fc`), so a model never sees more than one system message.
//!
//! Mirrors [`MemoryHook`](crate::memory::MemoryHook): identified by [`CONTEXT_HEADER`] so a
//! `--resume` can reconcile it in place; lands after the leading-system run (persona).
//! `/cd` is a NEW SESSION (the driver re-prepares in the new dir), so there is deliberately
//! no mid-session refresh — `session_start` runs again.

use super::instructions::render_instructions;
use async_trait::async_trait;
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::{Conversation, Message, Role};
use std::path::PathBuf;

/// First line of the rendered block — how the resume path locates it for in-place refresh.
const CONTEXT_HEADER: &str = "=== SESSION CONTEXT ===";

/// Injects environment + project-instructions + git-status context at session start.
pub struct SessionContextHook {
    working_dir: PathBuf,
    /// Config root (`~/.atomcode`) for the GLOBAL instructions tier. Defaults to
    /// [`crate::paths::config_dir`]; the env honors `$ATOMCODE_HOME` there.
    home: PathBuf,
}

impl SessionContextHook {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self { working_dir: working_dir.into(), home: crate::paths::config_dir() }
    }

    /// Test/embedder seam: supply an explicit config-root (global-instructions base).
    pub fn with_home(working_dir: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self { working_dir: working_dir.into(), home: home.into() }
    }

    /// Render the full context block. Always non-empty (the env sub-section is
    /// unconditional), so the session always carries a context message.
    fn render(&self) -> String {
        let mut out = vec![CONTEXT_HEADER.to_string(), self.env_block()];
        let instr = render_instructions(&self.home, &self.working_dir);
        if !instr.is_empty() {
            out.push(instr);
        }
        if let Some(git) = self.git_snapshot() {
            out.push(git);
        }
        out.join("\n\n")
    }

    fn env_block(&self) -> String {
        // Report the shell the `bash` tool ACTUALLY uses, so the model's env line agrees
        // with the tool description. On Windows that is Git Bash when present, else
        // cmd.exe (NOT `$SHELL`, which the tool ignores) — the old hard-coded "cmd.exe"
        // lied whenever Git Bash was installed, so the model emitted cmd syntax that then
        // ran in bash and broke. See `crate::tools::bash::windows_bash_active`.
        let shell = if cfg!(windows) {
            crate::tools::bash::windows_shell_label(crate::tools::bash::windows_bash_active())
                .to_string()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
        };
        format!(
            "Working directory: {}\nPlatform: {}\nShell: {}",
            self.working_dir.display(),
            std::env::consts::OS,
            shell
        )
    }

    /// `Some(block)` when `working_dir` is inside a git work tree, else `None`. A
    /// session-start snapshot (NOT live) — `git status --short` capped at 20 lines.
    fn git_snapshot(&self) -> Option<String> {
        if self.git(&["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
            return None;
        }
        let branch = self
            .git(&["branch", "--show-current"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(detached HEAD)".into());
        let head = self.git(&["log", "-1", "--format=%h %s"]).unwrap_or_default().trim().to_string();
        let raw = self.git(&["status", "--short"]).unwrap_or_default();
        let mut lines: Vec<&str> = raw.lines().collect();
        let status = if lines.len() > 20 {
            let extra = lines.len() - 20;
            lines.truncate(20);
            format!("{}\n... and {extra} more line(s)", lines.join("\n"))
        } else {
            lines.join("\n")
        };
        let status = if status.trim().is_empty() { "(working tree clean)".to_string() } else { status };
        Some(format!(
            "=== GIT STATUS (snapshot at session start, not live) ===\n\
             Branch: {branch}\nHEAD: {head}\n{status}\n\
             (This is a session-start snapshot — run `git status` for live state.)"
        ))
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(&self.working_dir);
        // No console-window flash for the session-start git snapshot when run from a
        // console-less daemon (mirrors core's ctx/env); no-op off Windows.
        crate::process_utils::suppress_console_window_sync(&mut cmd);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[async_trait]
impl LifecycleHooks for SessionContextHook {
    async fn session_start(&self, convo: &mut Conversation, resumed: bool) {
        let block = self.render(); // always non-empty (env is unconditional)
        if !resumed {
            // FRESH: land right after the leading-system run (persona, and any context hook
            // registered before this one), before the first user message.
            let at = convo.messages.iter().take_while(|m| m.role == Role::System).count();
            convo.messages.insert(at, Message::system(block));
            return;
        }
        // RESUME (same project): refresh in place — git status / instructions may have
        // changed since the snapshot was saved. Byte-identical when nothing changed.
        match convo
            .messages
            .iter()
            .position(|m| m.role == Role::System && m.text.starts_with(CONTEXT_HEADER))
        {
            Some(i) => convo.messages[i] = Message::system(block),
            None => {
                let at = convo.messages.iter().take_while(|m| m.role == Role::System).count();
                convo.messages.insert(at, Message::system(block));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_init(dir: &std::path::Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            std::process::Command::new("git").args(&args).current_dir(dir).output().unwrap();
        }
    }

    #[tokio::test]
    async fn fresh_injects_env_after_persona() {
        let d = tempfile::tempdir().unwrap();
        let hook = SessionContextHook::with_home(d.path(), d.path().join("nohome"));
        let mut convo = Conversation::new();
        convo.push(Message::system("persona"));
        hook.session_start(&mut convo, false).await;
        assert_eq!(convo.messages.len(), 2);
        assert_eq!(convo.messages[0].text, "persona");
        let ctx = &convo.messages[1];
        assert_eq!(ctx.role, Role::System);
        assert!(ctx.text.starts_with(CONTEXT_HEADER), "block leads with the header");
        assert!(ctx.text.contains("Working directory:"), "env block always present");
        assert!(ctx.text.contains("Platform:"));
    }

    #[tokio::test]
    async fn git_section_only_inside_a_repo() {
        // Not a repo → no git section.
        let bare = tempfile::tempdir().unwrap();
        let h1 = SessionContextHook::with_home(bare.path(), bare.path().join("nohome"));
        assert!(!h1.render().contains("GIT STATUS"), "no git section outside a repo");

        // A repo → git section present.
        let repo = tempfile::tempdir().unwrap();
        git_init(repo.path());
        let h2 = SessionContextHook::with_home(repo.path(), repo.path().join("nohome"));
        assert!(h2.render().contains("=== GIT STATUS"), "git section inside a repo");
    }

    #[tokio::test]
    async fn project_instructions_are_included() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "project rule X").unwrap();
        let hook = SessionContextHook::with_home(d.path(), d.path().join("nohome"));
        assert!(hook.render().contains("PROJECT INSTRUCTIONS"));
        assert!(hook.render().contains("project rule X"));
    }

    #[tokio::test]
    async fn resume_refreshes_in_place_no_growth() {
        let d = tempfile::tempdir().unwrap();
        let hook = SessionContextHook::with_home(d.path(), d.path().join("nohome"));
        // A resumed history that already carries a (stale) context block.
        let mut convo = Conversation::new();
        convo.push(Message::system("persona"));
        convo.push(Message::system(format!("{CONTEXT_HEADER}\nstale snapshot")));
        convo.push(Message::user("earlier turn"));
        hook.session_start(&mut convo, true).await;
        assert_eq!(convo.messages.len(), 3, "refresh replaces in place — no growth");
        assert!(convo.messages[1].text.starts_with(CONTEXT_HEADER));
        assert!(convo.messages[1].text.contains("Working directory:"), "refreshed with live env");
        assert!(!convo.messages[1].text.contains("stale snapshot"), "stale block replaced");
        assert_eq!(convo.messages[2].text, "earlier turn", "history untouched");
    }

    #[tokio::test]
    async fn resume_inserts_when_absent() {
        // Snapshot predates the context hook → insert after the leading system run.
        let d = tempfile::tempdir().unwrap();
        let hook = SessionContextHook::with_home(d.path(), d.path().join("nohome"));
        let mut convo = Conversation::new();
        convo.push(Message::system("persona"));
        convo.push(Message::user("earlier turn"));
        hook.session_start(&mut convo, true).await;
        assert_eq!(convo.messages.len(), 3);
        assert!(convo.messages[1].text.starts_with(CONTEXT_HEADER), "lands after persona");
        assert_eq!(convo.messages[2].text, "earlier turn");
    }
}
