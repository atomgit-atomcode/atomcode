//! `list_sessions` — enumerate THIS project's past sessions so the agent can
//! DETECT what conversations exist (titles, when, size) and then pull their
//! content with [`recall`](super::recall). Complements `recall`: `recall`
//! searches turn *content* by topic; this lists the *sessions* themselves,
//! which `recall` cannot do.
//!
//! Reads the per-project `<project_hash>` bucket's `<id>.meta` files (the same
//! catalog `/resume` shows) — derived from `ToolContext.working_dir`, so the
//! tool needs no session wiring, or PINNED to an assembly's
//! `SessionManager::root()` (see [`ListSessionsTool::with_sessions_dir`]).
//! Read-only ⇒ `risk = Safe`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use chrono::TimeZone;
use serde::Deserialize;

use super::manager::SessionMeta;
use super::SessionManager;

const DEFAULT_LIMIT: usize = 20;

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The `list_sessions` tool.
pub struct ListSessionsTool {
    /// PINNED sessions dir — same rationale as [`super::recall::RecallTool`]: the
    /// live `ToolContext.working_dir` MOVES when the model runs `cd`, so an
    /// assembly that owns a `SessionManager` pins its `root()` here to stay on the
    /// bucket the session hooks actually write.
    sessions_dir: Option<PathBuf>,
}

impl Default for ListSessionsTool {
    fn default() -> Self {
        Self { sessions_dir: None }
    }
}

impl ListSessionsTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin the sessions dir this tool lists (an assembly passes its
    /// `SessionManager::root()`), instead of re-deriving it from the live —
    /// `cd`-movable — working dir at each call.
    pub fn with_sessions_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.sessions_dir = Some(dir.into());
        self
    }

    /// The testable core: list the sessions under `sessions_dir`, optionally
    /// filtered by a case-insensitive substring of the title, capped at `limit`,
    /// formatted for the model. Separated from `execute` so it is unit-testable
    /// against a temp dir without `$ATOMCODE_HOME`.
    pub fn list_dir(&self, sessions_dir: &Path, query: Option<&str>, limit: usize) -> String {
        // `list_visible` already validates each `.meta`, sorts by `updated_at`
        // desc, and excludes scheduled-run sessions (matching /resume). We just
        // filter by the optional title substring and cap at `limit`.
        let needle = query.map(|q| q.to_lowercase());
        let rows: Vec<String> = SessionManager::with_root(sessions_dir)
            .list_visible()
            .iter()
            .filter(|m| match &needle {
                Some(q) => m.name.to_lowercase().contains(q.as_str()),
                None => true,
            })
            .take(limit)
            .map(format_row)
            .collect();

        if rows.is_empty() {
            return "No sessions found for this project. (Sessions are listed once a \
                    turn completes; the current in-progress one may not appear yet.)"
                .to_string();
        }

        format!(
            "{} session(s) in this project, most recent first:\n{}\n\n(To read the \
             actual content/decisions of any of these, use `recall` with keywords \
             from its title.)",
            rows.len(),
            rows.join("\n")
        )
    }
}

fn format_row(m: &SessionMeta) -> String {
    let when = chrono::Local
        .timestamp_millis_opt(m.updated_at)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string());
    format!(
        "  • {}  ·  {}  ·  {} turns  ·  id {}",
        m.name, when, m.turn_count, m.id
    )
}

#[async_trait]
impl Tool for ListSessionsTool {
    fn name(&self) -> &str {
        "list_sessions"
    }

    fn description(&self) -> &str {
        "List THIS project's past conversation sessions — their titles, when they \
         were last active, and size — so you can tell the user what exists or decide \
         which one to pull from. This only ENUMERATES sessions; to read the actual \
         content/decisions of one, use `recall` (keyword/topic search across the same \
         sessions). Read-only. Optional `query` filters by a case-insensitive \
         substring of the session title."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "optional case-insensitive substring filter on the session title" },
                "limit": { "type": "integer", "description": "max sessions to return (default 20)" }
            }
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: ListArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("invalid list_sessions arguments: {e}"),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        let sessions_dir = match &self.sessions_dir {
            Some(d) => d.clone(),
            None => SessionManager::for_project(&ctx.working_dir)
                .root()
                .to_path_buf(),
        };
        let content = self.list_dir(
            &sessions_dir,
            a.query.as_deref(),
            a.limit.unwrap_or(DEFAULT_LIMIT),
        );
        ToolResult {
            call_id: String::new(),
            content,
            is_error: false,
            images: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seed(dir: &Path, id: &str, name: &str, updated_at: i64, turns: u32) {
        let mut m = SessionMeta::new(id, "/proj", updated_at);
        m.name = name.to_string();
        m.updated_at = updated_at;
        m.turn_count = turns;
        let bytes = serde_json::to_vec(&m).unwrap();
        fs::write(dir.join(format!("{id}.meta")), bytes).unwrap();
    }

    #[test]
    fn lists_sessions_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "old", "fix login bug", 1_000, 3);
        seed(dir.path(), "new", "refactor parser", 2_000, 7);
        let out = ListSessionsTool::new().list_dir(dir.path(), None, 20);
        assert!(out.contains("fix login bug"), "out: {out}");
        assert!(out.contains("refactor parser"), "out: {out}");
        let i_new = out.find("refactor parser").unwrap();
        let i_old = out.find("fix login bug").unwrap();
        assert!(i_new < i_old, "newer session must be listed first: {out}");
    }

    #[test]
    fn query_filters_by_title_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "a", "Fix Login Bug", 1_000, 1);
        seed(dir.path(), "b", "refactor parser", 2_000, 1);
        let out = ListSessionsTool::new().list_dir(dir.path(), Some("login"), 20);
        assert!(out.contains("Fix Login Bug"), "out: {out}");
        assert!(!out.contains("refactor parser"), "filtered out: {out}");
    }

    #[test]
    fn limit_caps_the_number_of_rows() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            seed(
                dir.path(),
                &format!("s{i}"),
                &format!("session {i}"),
                i as i64,
                1,
            );
        }
        let out = ListSessionsTool::new().list_dir(dir.path(), None, 2);
        let rows = out.matches("  • ").count();
        assert_eq!(rows, 2, "limit must cap rows: {out}");
    }

    #[test]
    fn empty_project_reports_none() {
        let dir = tempfile::tempdir().unwrap();
        let out = ListSessionsTool::new().list_dir(dir.path(), None, 20);
        assert!(
            out.to_lowercase().contains("no sessions"),
            "empty dir must say so: {out}"
        );
    }
}
