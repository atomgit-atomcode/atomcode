//! `memory` — let the model persist a durable, non-obvious learning to memory.md so
//! future sessions remember it. Reuses the same store the user's /remember writes to
//! (`.atomcode/memory.md` per project, `$ATOMCODE_HOME/memory.md` global). Injection is
//! handled separately by `MemoryHook` at session start; this tool only writes.

use super::{err, ok};
use crate::memory::MemoryStore;
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

const MEMORY_DESC: &str = "Persist a durable, non-obvious learning about the user or THIS \
project so future sessions remember it. Use `action:\"remember\"` when the user states a \
lasting preference, corrects you in a way that should stick, or you discover a non-obvious \
project convention/quirk. Use `action:\"forget\"` to drop entries matching a keyword, and \
`action:\"list\"` to review current memory. DO NOT record: obvious facts, standard \
tool/language behavior, anything already in AGENTS.md/.atomcode.md, verbose explanations, \
or session-specific one-offs. Keep each entry to one concise line.";

pub struct MemoryTool;

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
    fn store(scope: &str, cwd: &Path) -> MemoryStore {
        if scope == "global" {
            MemoryStore::global()
        } else {
            MemoryStore::project(cwd)
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
                "scope": { "type": "string", "enum": ["project", "global"], "description": "project (default) = this repo only; global = all projects" }
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
                match Self::store(scope, &ctx.working_dir).append_deduped(content) {
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
                // Scan BOTH stores regardless of `scope` (parity with the `/forget`
                // command): a forget-by-keyword should remove the entry wherever it
                // lives, so a global entry can be dropped without an explicit scope.
                let mut removed = MemoryStore::project(&ctx.working_dir)
                    .remove_matching(keyword)
                    .unwrap_or_default();
                removed.extend(
                    MemoryStore::global()
                        .remove_matching(keyword)
                        .unwrap_or_default(),
                );
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
                let g = MemoryStore::global();
                let p = MemoryStore::project(&ctx.working_dir);
                let name = ctx
                    .working_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "project".into());
                let merged = MemoryStore::merged_for_prompt(&g, &p, &name);
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

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn remember_writes_project_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let r = MemoryTool
            .execute(
                r#"{"action":"remember","content":"uses tabs"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(crate::memory::MemoryStore::project(tmp.path())
            .load()
            .iter()
            .any(|e| e == "uses tabs"));
    }

    #[tokio::test]
    async fn remember_dedup_reports_skip() {
        let tmp = tempfile::tempdir().unwrap();
        MemoryTool
            .execute(r#"{"action":"remember","content":"x"}"#, &ctx(tmp.path()))
            .await;
        let r = MemoryTool
            .execute(r#"{"action":"remember","content":"x"}"#, &ctx(tmp.path()))
            .await;
        assert!(!r.is_error);
        assert!(r.content.to_lowercase().contains("skip") || r.content.contains("already"));
    }

    #[tokio::test]
    async fn remember_missing_content_errors_not_panics() {
        let tmp = tempfile::tempdir().unwrap();
        let r = MemoryTool
            .execute(r#"{"action":"remember"}"#, &ctx(tmp.path()))
            .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn forget_removes_matching() {
        let tmp = tempfile::tempdir().unwrap();
        MemoryTool
            .execute(
                r#"{"action":"remember","content":"delete me please"}"#,
                &ctx(tmp.path()),
            )
            .await;
        let r = MemoryTool
            .execute(
                r#"{"action":"forget","keyword":"delete me"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!r.is_error);
        assert!(crate::memory::MemoryStore::project(tmp.path())
            .load()
            .is_empty());
    }

    #[tokio::test]
    async fn forget_without_scope_reaches_global_store() {
        // A global entry must be forgettable via a bare `forget` (no scope) — parity
        // with the `/forget` command, which scans both stores. Unique keywords avoid
        // colliding with the process-shared (isolated-home) global store.
        let tmp = tempfile::tempdir().unwrap();
        MemoryTool
            .execute(
                r#"{"action":"remember","content":"projq7x1 marker"}"#,
                &ctx(tmp.path()),
            )
            .await;
        MemoryTool
            .execute(
                r#"{"action":"remember","content":"globq7x2 marker","scope":"global"}"#,
                &ctx(tmp.path()),
            )
            .await;
        let r = MemoryTool
            .execute(r#"{"action":"forget","keyword":"q7x"}"#, &ctx(tmp.path()))
            .await;
        assert!(!r.is_error);
        assert!(crate::memory::MemoryStore::project(tmp.path())
            .find_matching("q7x")
            .is_empty());
        assert!(
            crate::memory::MemoryStore::global()
                .find_matching("globq7x2")
                .is_empty(),
            "global entry must be forgotten"
        );
    }

    #[test]
    fn risk_is_safe_by_default_and_risky_under_approval_env() {
        // 默认 Safe
        std::env::remove_var("ATOMCODE_MEMORY_APPROVAL");
        assert!(matches!(
            MemoryTool.risk(r#"{"action":"remember","content":"x"}"#),
            RiskLevel::Safe
        ));
        // 开审批 → remember Risky, list 仍 Safe
        std::env::set_var("ATOMCODE_MEMORY_APPROVAL", "1");
        assert!(matches!(
            MemoryTool.risk(r#"{"action":"remember","content":"x"}"#),
            RiskLevel::Risky
        ));
        assert!(matches!(
            MemoryTool.risk(r#"{"action":"list"}"#),
            RiskLevel::Safe
        ));
        std::env::remove_var("ATOMCODE_MEMORY_APPROVAL");
    }
}
