//! `memory` — let the model persist a durable, non-obvious learning to memory.md so
//! future sessions remember it. Reuses the same store the user's /remember writes to
//! (`<project dir>/memory.md` per project, `<user tree>/memory.md` global unless
//! the host names another file with [`MemoryTool::with_global`]). Injection is
//! handled separately by `MemoryHook` at session start; this tool only writes.

use super::{err, ok};
use crate::memory::MemoryStore;
use crate::ProductDirs;
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

const MEMORY_DESC: &str = "Persist a durable, non-obvious learning about the user or THIS \
project so future sessions remember it. Use `action:\"remember\"` when the user states a \
lasting preference, corrects you in a way that should stick, or you discover a non-obvious \
project convention/quirk. Use `action:\"forget\"` to drop entries matching a keyword, and \
`action:\"list\"` to review current memory. DO NOT record: obvious facts, standard \
tool/language behavior, anything already in AGENTS.md/.atomcode.md, verbose explanations, \
or session-specific one-offs. Keep each entry to one concise line.";

/// Where each tier lives is the tool's to know, not the store's to look up: the
/// host hands in its [`ProductDirs`], and may name a different global file on
/// top ([`Self::with_global`]).
pub struct MemoryTool {
    dirs: ProductDirs,
    global: PathBuf,
}

#[derive(Deserialize)]
struct Args {
    action: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    keyword: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

impl MemoryTool {
    /// The global tier at `<user tree>/memory.md`, the project tiers under
    /// each working directory's project dir.
    pub fn new(dirs: ProductDirs) -> Self {
        let global = dirs.user().join("memory.md");
        Self { dirs, global }
    }

    /// The global tier at `path` — the file itself, not a directory.
    pub fn with_global(mut self, path: impl Into<PathBuf>) -> Self {
        self.global = path.into();
        self
    }

    fn global(&self) -> MemoryStore {
        MemoryStore::new(self.global.clone())
    }

    fn store(&self, scope: &str, cwd: &Path) -> MemoryStore {
        match scope {
            "global" => self.global(),
            "local" => MemoryStore::local(&self.dirs.project(cwd)),
            _ => MemoryStore::project(&self.dirs.project(cwd)),
        }
    }

    fn approval_required() -> bool {
        std::env::var("ATOMCODE_MEMORY_APPROVAL")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "on" | "yes"
                )
            })
            .unwrap_or(false)
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        MEMORY_DESC
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["remember", "forget", "list"], "description": "remember a fact, forget entries by keyword, or list current memory" },
                "content": { "type": "string", "description": "The concise fact to remember (required for action=remember)" },
                "keyword": { "type": "string", "description": "Substring of entries to remove (required for action=forget)" },
                "scope": { "type": "string", "enum": ["project", "local", "global"], "description": "project (default) = this repo only; local = this repo on this machine only (not committed); global = all projects" }
            },
            "required": ["action"]
        })
    }
    /// Safe (visible, auto-approved) by default; ATOMCODE_MEMORY_APPROVAL gates the
    /// mutating actions behind the approval middleware. `list` is always Safe.
    fn risk(&self, args: &str) -> RiskLevel {
        if !Self::approval_required() {
            return RiskLevel::Safe;
        }
        match serde_json::from_str::<Args>(args) {
            Ok(a) if a.action == "list" => RiskLevel::Safe,
            _ => RiskLevel::Risky,
        }
    }
    /// Tool-wide "Always" grant (like write_file): approving once covers all memory writes.
    fn always_grant_scope(&self, _args: &str) -> String {
        "memory".to_string()
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("memory: invalid arguments: {e}")),
        };
        let scope = a.scope.as_deref().unwrap_or("project");
        match a.action.as_str() {
            "remember" => {
                let content = match a
                    .content
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(c) => c,
                    None => return err("memory: action=remember requires a non-empty `content`."),
                };
                match self.store(scope, &ctx.working_dir).append_deduped(content) {
                    Ok(true) => ok(format!("📝 remembered ({scope}): {content}")),
                    Ok(false) => ok(format!("already remembered ({scope}), skipped: {content}")),
                    Err(e) => err(format!("memory: failed to write: {e}")),
                }
            }
            "forget" => {
                let keyword = match a
                    .keyword
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(k) => k,
                    None => return err("memory: action=forget requires a non-empty `keyword`."),
                };
                // Scan ALL three stores regardless of `scope` (parity with the
                // `/forget` command): a forget-by-keyword should remove the entry
                // wherever it lives, so a global or local entry can be dropped
                // without an explicit scope.
                let mut removed = self
                    .store("project", &ctx.working_dir)
                    .remove_matching(keyword)
                    .unwrap_or_default();
                removed.extend(
                    self.store("local", &ctx.working_dir)
                        .remove_matching(keyword)
                        .unwrap_or_default(),
                );
                removed.extend(self.global().remove_matching(keyword).unwrap_or_default());
                if removed.is_empty() {
                    ok(format!("no memory entries matched '{keyword}'."))
                } else {
                    ok(format!(
                        "forgot {} entr{}.",
                        removed.len(),
                        if removed.len() == 1 { "y" } else { "ies" }
                    ))
                }
            }
            "list" => {
                let g = self.global();
                let p = self.store("project", &ctx.working_dir);
                let l = self.store("local", &ctx.working_dir);
                let stores = match a.scope.as_deref() {
                    Some("global") => vec![("Global", &g)],
                    Some("project") => vec![("Project", &p)],
                    Some("local") => vec![("Local", &l)],
                    Some(other) => return err(format!("memory: unknown scope '{other}'")),
                    None => vec![("Global", &g), ("Project", &p), ("Local", &l)],
                };
                let merged = match MemoryStore::for_display(&stores) {
                    Ok(text) => text,
                    Err(error) => return err(format!("memory: {error}")),
                };
                if merged.trim().is_empty() {
                    ok("(memory is empty)".to_string())
                } else {
                    ok(merged)
                }
            }
            other => err(format!(
                "memory: unknown action '{other}'. Use remember | forget | list."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    /// A project under `tmp` and a user tree beside it, both under our test
    /// names — so nothing here touches a real `~/.atomcode`.
    struct Fixture {
        tmp: tempfile::TempDir,
        dirs: ProductDirs,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dirs = ProductDirs::new(tmp.path().join("tree"), ".ours");
            Self { tmp, dirs }
        }
        fn path(&self) -> &Path {
            self.tmp.path()
        }
        fn tool(&self) -> MemoryTool {
            MemoryTool::new(self.dirs.clone())
        }
        fn project(&self) -> crate::memory::MemoryStore {
            crate::memory::MemoryStore::project(&self.dirs.project(self.path()))
        }
        fn local(&self) -> crate::memory::MemoryStore {
            crate::memory::MemoryStore::local(&self.dirs.project(self.path()))
        }
        fn global(&self) -> crate::memory::MemoryStore {
            crate::memory::MemoryStore::global(self.dirs.user())
        }
    }

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn remember_writes_local_scope_entry() {
        let tmp = Fixture::new();
        let r = tmp.tool()
            .execute(
                r#"{"action":"remember","content":"node at /opt/homebrew/bin/node","scope":"local"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(tmp
            .local()
            .load()
            .iter()
            .any(|e| e == "node at /opt/homebrew/bin/node"));
        // local scope must NOT land in the project store.
        assert!(tmp.project().load().is_empty());
    }

    #[tokio::test]
    async fn forget_scan_removes_local_entry_without_scope() {
        let tmp = Fixture::new();
        tmp.tool()
            .execute(
                r#"{"action":"remember","content":"local-only fact lq1","scope":"local"}"#,
                &ctx(tmp.path()),
            )
            .await;
        let r = tmp
            .tool()
            .execute(r#"{"action":"forget","keyword":"lq1"}"#, &ctx(tmp.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            tmp.local().find_matching("lq1").is_empty(),
            "local entry must be forgotten via bare forget"
        );
    }

    #[tokio::test]
    async fn list_includes_local_section() {
        let tmp = Fixture::new();
        tmp.tool()
            .execute(
                r#"{"action":"remember","content":"l1 local marker","scope":"local"}"#,
                &ctx(tmp.path()),
            )
            .await;
        let r = tmp
            .tool()
            .execute(r#"{"action":"list"}"#, &ctx(tmp.path()))
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("[Local]\n- l1 local marker"));
    }

    #[tokio::test]
    async fn remember_writes_project_entry() {
        let tmp = Fixture::new();
        let r = tmp
            .tool()
            .execute(
                r#"{"action":"remember","content":"uses tabs"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(tmp.project().load().iter().any(|e| e == "uses tabs"));
    }

    #[tokio::test]
    async fn remember_dedup_reports_skip() {
        let tmp = Fixture::new();
        tmp.tool()
            .execute(r#"{"action":"remember","content":"x"}"#, &ctx(tmp.path()))
            .await;
        let r = tmp
            .tool()
            .execute(r#"{"action":"remember","content":"x"}"#, &ctx(tmp.path()))
            .await;
        assert!(!r.is_error);
        assert!(r.content.to_lowercase().contains("skip") || r.content.contains("already"));
    }

    #[tokio::test]
    async fn remember_missing_content_errors_not_panics() {
        let tmp = Fixture::new();
        let r = tmp
            .tool()
            .execute(r#"{"action":"remember"}"#, &ctx(tmp.path()))
            .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn forget_removes_matching() {
        let tmp = Fixture::new();
        tmp.tool()
            .execute(
                r#"{"action":"remember","content":"delete me please"}"#,
                &ctx(tmp.path()),
            )
            .await;
        let r = tmp
            .tool()
            .execute(
                r#"{"action":"forget","keyword":"delete me"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!r.is_error);
        assert!(tmp.project().load().is_empty());
    }

    #[tokio::test]
    async fn forget_without_scope_reaches_global_store() {
        // A global entry must be forgettable via a bare `forget` (no scope) — parity
        // with the `/forget` command, which scans both stores.
        let tmp = Fixture::new();
        tmp.tool()
            .execute(
                r#"{"action":"remember","content":"projq7x1 marker"}"#,
                &ctx(tmp.path()),
            )
            .await;
        tmp.tool()
            .execute(
                r#"{"action":"remember","content":"globq7x2 marker","scope":"global"}"#,
                &ctx(tmp.path()),
            )
            .await;
        let r = tmp
            .tool()
            .execute(r#"{"action":"forget","keyword":"q7x"}"#, &ctx(tmp.path()))
            .await;
        assert!(!r.is_error);
        assert!(tmp.project().find_matching("q7x").is_empty());
        assert!(
            tmp.global().find_matching("globq7x2").is_empty(),
            "global entry must be forgotten"
        );
    }

    /// All three actions reach the global tier through the path the tool was
    /// given — `remember` alone is not enough, a `forget` or `list` that still
    /// read the user tree's own `memory.md` would leave the host's users with two
    /// global memories and no way to tell which one the model sees.
    #[tokio::test]
    async fn a_global_path_given_to_the_tool_is_the_only_global_tier_it_touches() {
        let project = Fixture::new();
        let state = tempfile::tempdir().unwrap();
        let global = state.path().join("memory.md");
        let tool = project.tool().with_global(&global);

        let r = tool
            .execute(
                r#"{"action":"remember","content":"hostg4k1 marker","scope":"global"}"#,
                &ctx(project.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            crate::memory::MemoryStore::new(global.clone())
                .load()
                .iter()
                .any(|e| e == "hostg4k1 marker"),
            "remember must write the given file"
        );
        assert!(
            project.global().find_matching("hostg4k1").is_empty(),
            "remember must not also write the user tree's memory.md"
        );

        let listed = tool
            .execute(r#"{"action":"list"}"#, &ctx(project.path()))
            .await;
        assert!(
            listed.content.contains("hostg4k1 marker"),
            "list must read the given file: {}",
            listed.content
        );

        tool.execute(
            r#"{"action":"forget","keyword":"hostg4k1"}"#,
            &ctx(project.path()),
        )
        .await;
        assert!(
            crate::memory::MemoryStore::new(global)
                .find_matching("hostg4k1")
                .is_empty(),
            "forget must remove from the given file"
        );
    }

    #[test]
    fn risk_is_safe_by_default_and_risky_under_approval_env() {
        let tmp = Fixture::new();
        // 默认 Safe
        std::env::remove_var("ATOMCODE_MEMORY_APPROVAL");
        assert!(matches!(
            tmp.tool().risk(r#"{"action":"remember","content":"x"}"#),
            RiskLevel::Safe
        ));
        // 开审批 → remember Risky, list 仍 Safe
        std::env::set_var("ATOMCODE_MEMORY_APPROVAL", "1");
        assert!(matches!(
            tmp.tool().risk(r#"{"action":"remember","content":"x"}"#),
            RiskLevel::Risky
        ));
        assert!(matches!(
            tmp.tool().risk(r#"{"action":"list"}"#),
            RiskLevel::Safe
        ));
        std::env::remove_var("ATOMCODE_MEMORY_APPROVAL");
    }
}
