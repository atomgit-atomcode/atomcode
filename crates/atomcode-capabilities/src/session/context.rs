//! `SessionContextHook` — injects the per-session "context block" (environment + project
//! instructions + git snapshot) as ONE leading `Role::System` message at session start,
//! replacing the former core runtime-injected prompt sections.
//!
//! Cache-safe: the block is read ONCE at session start and frozen — a SNAPSHOT of where
//! the session began, not a live view (the git section says so explicitly). The wire
//! adapter then coalesces persona + this block + memory into a single system message
//! (commit `3956f9fc`), so a model never sees more than one system message.
//!
//! Identified by [`CONTEXT_HEADER`] so `--resume` can locate it; lands after the
//! leading-system run (persona). On resume env + project instructions are re-rendered (edits to
//! AGENTS.md apply, the shell label refreshes), but the saved GIT section is FROZEN — its bytes
//! drift on every commit and rewriting them would invalidate prefix caching for the whole
//! resumed conversation on its first turn. When env/instructions are unchanged the re-rendered
//! block is byte-identical, so the cache still holds. A full fresh block is inserted only when a
//! legacy session carries none. `/cd` is a NEW SESSION (the driver re-prepares in the new dir),
//! so `session_start` runs fresh there.

use super::instructions::render_instructions;
use async_trait::async_trait;
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::{Conversation, Message, Role};
use std::path::PathBuf;

/// First line of the rendered block — how the resume path locates it for in-place refresh.
const CONTEXT_HEADER: &str = "=== SESSION CONTEXT ===";

/// Separator + marker that begins the git sub-section (always the LAST section, joined onto
/// the base with a blank line). On resume the saved git bytes — from this marker to the end —
/// are spliced back verbatim so the frozen snapshot survives while env/instructions refresh.
const GIT_SECTION_SEP: &str = "\n\n=== GIT STATUS";

/// Injects environment + project-instructions + git-status context at session start.
pub struct SessionContextHook {
    working_dir: PathBuf,
    /// Config root (`~/.atomcode`) for the GLOBAL instructions tier. Defaults to
    /// [`crate::paths::config_dir`]; the env honors `$ATOMCODE_HOME` there.
    home: PathBuf,
}

impl SessionContextHook {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            home: crate::paths::config_dir(),
        }
    }

    /// Test/embedder seam: supply an explicit config-root (global-instructions base).
    pub fn with_home(working_dir: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            home: home.into(),
        }
    }

    /// Render the full context block. Always non-empty (the env sub-section is
    /// unconditional), so the session always carries a context message.
    fn render(&self) -> String {
        match self.git_snapshot() {
            Some(git) => format!("{}\n\n{}", self.render_base(), git),
            None => self.render_base(),
        }
    }

    /// The NON-git portion — header + env + project instructions. Split from the git snapshot
    /// because on resume we RE-RENDER this (the user may have edited AGENTS.md, or the shell
    /// changed) while KEEPING the saved git section: git bytes drift on every commit and would
    /// otherwise break the cached prefix (see `session_start`). `GIT_SECTION_SEP` assumes this
    /// base carries no `=== GIT STATUS` marker of its own.
    fn render_base(&self) -> String {
        let mut out = vec![CONTEXT_HEADER.to_string(), self.env_block()];
        let instr = render_instructions(&self.home, &self.working_dir);
        if !instr.is_empty() {
            out.push(instr);
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
            // Forward-slash on Windows so the model's cwd anchor is bash-safe
            // (matches the bash tool's "use forward slashes" guidance).
            crate::pathnorm::to_display(&self.working_dir),
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
        let head = self
            .git(&["log", "-1", "--format=%h %s"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let raw = self.git(&["status", "--short"]).unwrap_or_default();
        let mut lines: Vec<&str> = raw.lines().collect();
        let status = if lines.len() > 20 {
            let extra = lines.len() - 20;
            lines.truncate(20);
            format!("{}\n... and {extra} more line(s)", lines.join("\n"))
        } else {
            lines.join("\n")
        };
        let status = if status.trim().is_empty() {
            "(working tree clean)".to_string()
        } else {
            status
        };
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
        if !resumed {
            // FRESH: land right after the leading-system run (persona, and any context hook
            // registered before this one), before the first user message.
            let at = convo
                .messages
                .iter()
                .take_while(|m| m.role == Role::System)
                .count();
            convo.messages.insert(at, Message::system(self.render()));
            return;
        }
        // RESUME (same project): re-render env + project instructions (the user may have edited
        // AGENTS.md, or the shell changed), but FREEZE the saved git section.
        //
        // The block lives in the leading, cached prefix (it coalesces into the persona system
        // message on the wire). The one part that drifts on nearly every resume is the git
        // section — a new HEAD after a commit, `git status` after edits — and rewriting it
        // changes the prefix, invalidating the gateway's prefix cache for the WHOLE resumed
        // conversation on its first turn (observed: HEAD `fcf0b5b6` → `dd526cb4` across a resume
        // forcing a full re-prefill). Git is a session-start snapshot by design — its header
        // says "run `git status` for live state" — so freezing it is the intended contract.
        // Env/instructions, by contrast, are stable-or-rarely-edited and SHOULD apply on resume:
        // when they are unchanged the re-rendered base is byte-identical and the cache still
        // holds; when a rule genuinely changed, a one-turn re-prefill is the correct cost.
        //
        // Detection is scoped to the leading system run (where the block lives and is inserted)
        // so a stray later message echoing the header can't suppress insertion.
        let leading = convo
            .messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .count();
        match convo.messages[..leading]
            .iter()
            .position(|m| m.text.starts_with(CONTEXT_HEADER))
        {
            Some(i) => {
                let saved = &convo.messages[i].text;
                let refreshed = match saved.rfind(GIT_SECTION_SEP) {
                    // Splice the frozen git bytes (marker → end) onto a freshly rendered base.
                    // `+ 2` skips the "\n\n" the separator carries so the join isn't doubled.
                    Some(sep) => format!("{}\n\n{}", self.render_base(), &saved[sep + 2..]),
                    // Saved block carried no git section (not a repo at save time) — just refresh.
                    None => self.render_base(),
                };
                convo.messages[i] = Message::system(refreshed);
            }
            // Legacy/pre-upgrade session that never carried the block — insert a full fresh one.
            None => convo.messages.insert(leading, Message::system(self.render())),
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
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .unwrap();
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
        assert!(
            ctx.text.starts_with(CONTEXT_HEADER),
            "block leads with the header"
        );
        assert!(
            ctx.text.contains("Working directory:"),
            "env block always present"
        );
        assert!(ctx.text.contains("Platform:"));
    }

    #[tokio::test]
    async fn git_section_only_inside_a_repo() {
        // Not a repo → no git section.
        let bare = tempfile::tempdir().unwrap();
        let h1 = SessionContextHook::with_home(bare.path(), bare.path().join("nohome"));
        assert!(
            !h1.render().contains("GIT STATUS"),
            "no git section outside a repo"
        );

        // A repo → git section present.
        let repo = tempfile::tempdir().unwrap();
        git_init(repo.path());
        let h2 = SessionContextHook::with_home(repo.path(), repo.path().join("nohome"));
        assert!(
            h2.render().contains("=== GIT STATUS"),
            "git section inside a repo"
        );
    }

    #[tokio::test]
    async fn project_instructions_are_included() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "project rule X").unwrap();
        let hook = SessionContextHook::with_home(d.path(), d.path().join("nohome"));
        assert!(hook.render().contains("PROJECT INSTRUCTIONS"));
        assert!(hook.render().contains("project rule X"));
    }

    fn git_commit(dir: &std::path::Path, msg: &str) {
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", msg]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn resume_freezes_git_but_refreshes_instructions() {
        // Not a git repo → the live render carries no git section; the ONLY git section is the
        // frozen one already in the saved block.
        let d = tempfile::tempdir().unwrap();
        // The user edited project instructions AFTER the session was saved.
        std::fs::write(d.path().join("AGENTS.md"), "new project rule Z").unwrap();
        let hook = SessionContextHook::with_home(d.path(), d.path().join("nohome"));
        // Saved block: a stale env/base + a frozen git section from an earlier HEAD.
        let saved = format!(
            "{CONTEXT_HEADER}\n\nWorking directory: /old\n\n=== GIT STATUS (snapshot at session start, not live) ===\nHEAD: oldsha frozen commit"
        );
        let mut convo = Conversation::new();
        convo.push(Message::system("persona"));
        convo.push(Message::system(saved));
        convo.push(Message::user("earlier turn"));
        hook.session_start(&mut convo, true).await;
        assert_eq!(convo.messages.len(), 3, "no growth on resume");
        let block = &convo.messages[1].text;
        assert!(
            block.contains("new project rule Z"),
            "project instructions re-rendered from disk on resume: {block}"
        );
        assert!(
            block.contains("HEAD: oldsha frozen commit"),
            "saved git section frozen (not refreshed): {block}"
        );
        assert!(
            !block.contains("Working directory: /old"),
            "env re-rendered (stale env replaced): {block}"
        );
        assert_eq!(convo.messages[2].text, "earlier turn", "history untouched");
    }

    #[tokio::test]
    async fn resume_git_frozen_across_head_move_keeps_prefix_byte_stable() {
        // The core cache guarantee: even when the repo's HEAD moves between save and resume,
        // the resumed block is BYTE-IDENTICAL to the saved one (git frozen) so the cached
        // prefix survives.
        let repo = tempfile::tempdir().unwrap();
        git_init(repo.path());
        std::fs::write(repo.path().join("a.txt"), "1").unwrap();
        git_commit(repo.path(), "first");
        let hook = SessionContextHook::with_home(repo.path(), repo.path().join("nohome"));
        let saved = hook.render(); // captures HEAD #1
        // HEAD moves after the save.
        std::fs::write(repo.path().join("b.txt"), "2").unwrap();
        git_commit(repo.path(), "second");
        let mut convo = Conversation::new();
        convo.push(Message::system("persona"));
        convo.push(Message::system(saved.clone()));
        convo.push(Message::user("t"));
        hook.session_start(&mut convo, true).await;
        assert_eq!(
            convo.messages[1].text, saved,
            "git frozen across a HEAD move ⇒ block byte-identical ⇒ prefix cache holds"
        );
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
        assert!(
            convo.messages[1].text.starts_with(CONTEXT_HEADER),
            "lands after persona"
        );
        assert_eq!(convo.messages[2].text, "earlier turn");
    }
}
