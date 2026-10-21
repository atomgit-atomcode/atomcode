//! `memory` — let the model persist a durable, non-obvious learning to memory.md so
//! future sessions remember it. Reuses the same store the user's /remember writes to
//! (`<project dir>/memory.md` per project, `<user tree>/memory.md` global unless
//! the host names another file with [`MemoryTool::with_global`]). Injection is
//! handled separately by the harness memory row (or `MemoryHook` for standalone
//! kernel embedders); this tool reads and writes the source stores.

use super::{err, ok};
use crate::memory::entry::{MemoryPatch, MemoryStatus};
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
or session-specific one-offs. Keep each entry to one concise line. \
Entries have IDs and optional topic/source metadata. Use update with an ID to correct content \
or metadata, pin with pinned:true|false to change budget priority, and deprecate with an ID \
to retain an obsolete entry without injecting it. Use list with topic/status to filter. \
Optional ttl_days on remember/update sets an expiry; clear_expiry removes it on update. Expired entries remain stored but are excluded from new session injection, even when pinned. List with expired or expiring_within_days to review. Unrelated edits never renew expiry. Exact ID mutations affect only the requested scope (project by default). \
Updating disk memory does not rewrite the current session's injected context.";

/// Where each tier lives is the tool's to know, not the store's to look up: the
/// host hands in its [`ProductDirs`], and may name a different global file on
/// top ([`Self::with_global`]).
pub struct MemoryTool {
    dirs: ProductDirs,
    global: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    keyword: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    id: Option<String>,
    topic: Option<String>,
    source: Option<String>,
    status: Option<MemoryStatus>,
    pinned: Option<bool>,
    ttl_days: Option<u32>,
    #[serde(default)]
    clear_expiry: bool,
    #[serde(default)]
    expired: bool,
    expiring_within_days: Option<u32>,
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
                "action": { "type": "string", "enum": ["remember", "forget", "list", "update", "pin", "deprecate"], "description": "remember; list/filter; exact update, pin or deprecate by ID; or explicit keyword deletion" },
                "content": { "type": "string", "description": "The concise fact to remember (required for action=remember)" },
                "keyword": { "type": "string", "description": "Substring of entries to remove (required for action=forget)" },
                "id": { "type": "string", "description": "Exact ID from list; required for update/pin/deprecate" },
                "topic": { "type": "string", "description": "Topic for remember/update or exact topic filter for list; empty string clears on update" },
                "source": { "type": "string", "description": "Optional provenance, such as a document or session reference; empty string clears on update" },
                "status": { "type": "string", "enum": ["active", "outdated", "deprecated"], "description": "Update state or list filter. Only active entries are injected." },
                "ttl_days": {"type":"integer", "minimum":1, "maximum":36500, "description":"Set/renew expiry from this explicit write; remember/update only. Unrelated edits never renew."},
                "clear_expiry": {"type":"boolean", "description":"Remove expiry on update; mutually exclusive with ttl_days"},
                "expired": {"type":"boolean", "description":"List only entries whose expiry has passed"},
                "expiring_within_days": {"type":"integer", "minimum":1, "maximum":36500, "description":"List active entries expiring in the next N days; exclusive with expired"},
                "pinned": { "type": "boolean", "description": "Prioritize this active entry within the shared prompt budget" },
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
        if !matches!(scope, "project" | "local" | "global") {
            return err(format!("memory: unknown scope '{scope}'"));
        }
        let invalid_fields = match a.action.as_str() {
            "list" => {
                a.content.is_some()
                    || a.keyword.is_some()
                    || a.id.is_some()
                    || a.source.is_some()
                    || a.pinned.is_some()
            }
            "remember" => a.keyword.is_some() || a.id.is_some(),
            "update" => a.keyword.is_some(),
            "pin" => {
                a.content.is_some()
                    || a.keyword.is_some()
                    || a.topic.is_some()
                    || a.source.is_some()
                    || a.status.is_some()
            }
            "deprecate" => {
                a.content.is_some()
                    || a.keyword.is_some()
                    || a.topic.is_some()
                    || a.source.is_some()
                    || a.status.is_some()
                    || a.pinned.is_some()
            }
            "forget" => {
                a.id.is_some()
                    || a.content.is_some()
                    || a.topic.is_some()
                    || a.source.is_some()
                    || a.status.is_some()
                    || a.pinned.is_some()
            }
            _ => false,
        };
        if invalid_fields {
            return err("memory: arguments do not match this action; use update for content/metadata changes.");
        }
        if (!matches!(a.action.as_str(), "remember" | "update")
            && (a.ttl_days.is_some() || a.clear_expiry))
            || (a.action != "list" && (a.expired || a.expiring_within_days.is_some()))
            || (a.expired && a.expiring_within_days.is_some())
            || (a.action == "remember" && a.clear_expiry)
        {
            return err("memory: expiry options do not match this action or conflict");
        }
        if let Some(days) = a.expiring_within_days {
            if let Err(error) = crate::memory::entry::validate_days(days) {
                return err(error.to_string());
            }
        }
        let now = chrono::Utc::now();
        let expires_at = match crate::memory::entry::expiry_patch(a.ttl_days, a.clear_expiry, now) {
            Ok(value) => value,
            Err(error) => return err(error.to_string()),
        };
        let patch = MemoryPatch {
            content: a.content.clone(),
            topic: a.topic.clone(),
            source: a.source.clone(),
            status: a.status,
            pinned: a.pinned,
            expires_at,
        };
        match a.action.as_str() {
            "remember" => {
                let Some(content) = a.content.as_deref().filter(|s| !s.trim().is_empty()) else {
                    return err("memory: action=remember requires a non-empty `content`.");
                };
                if a.status.is_some_and(|s| s != MemoryStatus::Active) || a.id.is_some() {
                    return err("memory: remember creates active entries; use update/deprecate with an existing ID.");
                }
                let patch = MemoryPatch { content: None, ..patch };
                match self.store(scope, &ctx.working_dir).remember(content, &patch) {
                    Ok((entry, true)) => ok(format!("remembered ({scope}): {}\nID: {}", entry.content, entry.metadata.id)),
                    Ok((entry, false)) => ok(format!("already remembered ({scope}), skipped: {}\nID: {}", entry.content, entry.metadata.id)),
                    Err(e) => err(format!("memory: failed to write: {e}")),
                }
            }
            "update" | "pin" | "deprecate" => {
                let Some(id) = a.id.as_deref().filter(|s| !s.trim().is_empty()) else { return err("memory: exact mutation requires an ID from list."); };
                let patch = match a.action.as_str() {
                    "pin" => MemoryPatch { pinned: Some(a.pinned.unwrap_or(true)), ..Default::default() },
                    "deprecate" => MemoryPatch { status: Some(MemoryStatus::Deprecated), ..Default::default() },
                    _ => patch,
                };
                if patch.content.is_none() && patch.topic.is_none() && patch.source.is_none() && patch.status.is_none() && patch.pinned.is_none() && patch.expires_at.is_none() {
                    return err("memory: update requires content or a metadata change.");
                }
                match self.store(scope, &ctx.working_dir).update(id, &patch) {
                    Ok(entry) => ok(format!("updated ({scope}): {}\nID: {} status={} pinned={}", entry.content, entry.metadata.id, entry.metadata.status.as_str(), entry.metadata.pinned)),
                    Err(e) => err(format!("memory: update failed: {e}")),
                }
            }
            "forget" => {
                let Some(keyword) = a.keyword.as_deref().filter(|s| !s.trim().is_empty()) else { return err("memory: action=forget requires a non-empty `keyword`. Use deprecate with an ID for an exact, reversible operation."); };
                if a.id.is_some() { return err("memory: do not mix keyword deletion and exact ID mutation; use deprecate."); }
                let stores = if a.scope.is_some() { vec![self.store(scope, &ctx.working_dir)] } else { vec![self.store("project", &ctx.working_dir), self.store("local", &ctx.working_dir), self.global()] };
                let mut count = 0;
                for store in stores {
                    match store.remove_matching(keyword) {
                        Ok(entries) => count += entries.len(),
                        Err(e) => return err(format!("memory: deletion failed after {count} entries were removed: {}: {e}", store.path().display())),
                    }
                }
                ok(if count == 0 { format!("no memory entries matched '{keyword}'.") } else { format!("forgot {count} entries.") })
            }
            "list" => {
                let stores = if a.scope.is_some() { vec![(scope, self.store(scope, &ctx.working_dir))] } else { vec![("Global", self.global()), ("Project", self.store("project", &ctx.working_dir)), ("Local", self.store("local", &ctx.working_dir))] };
                let mut out = String::new();
                for (name, store) in stores {
                    let entries = match store.entries() { Ok(entries) => entries, Err(e) => return err(format!("memory: {}: {e}", store.path().display())) };
                    let entries: Vec<_> = entries.into_iter().filter(|entry| entry.matches(a.topic.as_deref(), a.status) && entry.matches_expiry(now, a.expired, a.expiring_within_days)).collect();
                    if entries.is_empty() { continue; }
                    out.push_str(&format!("## {}\n\n[{name}]\n", store.path().display()));
                    for entry in entries { out.push_str(&entry.describe()); out.push('\n'); }
                }
                ok(if out.is_empty() { "(memory is empty or no entries match)".into() } else { out })
            }
            other => err(format!("memory: unknown action '{other}'. Use remember | forget | list | update | pin | deprecate.")),
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

    #[tokio::test]
    async fn expiry_tool_mutations_filters_and_invalid_writes_preserve_identity() {
        let tmp = Fixture::new();
        let result = tmp
            .tool()
            .execute(
                r#"{"action":"remember","content":"temporary fact","ttl_days":7}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let entry = tmp.project().entries().unwrap().remove(0);
        assert!(entry.metadata.expires_at.is_some());
        let before = tmp.project().full_text().unwrap();
        for args in [
            serde_json::json!({"action":"update","id":entry.metadata.id,"ttl_days":0}),
            serde_json::json!({"action":"update","id":entry.metadata.id,"ttl_days":7,"clear_expiry":true}),
            serde_json::json!({"action":"pin","id":entry.metadata.id,"ttl_days":7}),
            serde_json::json!({"action":"list","expired":true,"expiring_within_days":7}),
        ] {
            assert!(
                tmp.tool()
                    .execute(&args.to_string(), &ctx(tmp.path()))
                    .await
                    .is_error
            );
            assert_eq!(tmp.project().full_text().unwrap(), before);
        }
        let list = tmp
            .tool()
            .execute(
                r#"{"action":"list","expiring_within_days":7}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!list.is_error);
        assert!(list.content.contains("temporary fact"));
        tmp.project()
            .update(
                &entry.metadata.id,
                &MemoryPatch {
                    expires_at: Some(Some("2000-01-01T00:00:00Z".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let expired = tmp
            .tool()
            .execute(r#"{"action":"list","expired":true}"#, &ctx(tmp.path()))
            .await;
        assert!(expired.content.contains("temporary fact"));
        let update =
            serde_json::json!({"action":"update","id":entry.metadata.id,"clear_expiry":true});
        assert!(
            !tmp.tool()
                .execute(&update.to_string(), &ctx(tmp.path()))
                .await
                .is_error
        );
        let restored = tmp.project().entries().unwrap().remove(0);
        assert_eq!(restored.metadata.id, entry.metadata.id);
        assert!(restored.metadata.expires_at.is_none());
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
        for action in ["update", "pin", "deprecate"] {
            assert!(matches!(
                tmp.tool()
                    .risk(&serde_json::json!({"action":action,"id":"m-test"}).to_string()),
                RiskLevel::Risky
            ));
        }
        std::env::remove_var("ATOMCODE_MEMORY_APPROVAL");
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn exact_mutations_respect_scope_and_keep_identity_and_creation_time() {
        let tmp = Fixture::new();
        let entry = tmp
            .global()
            .remember(
                "original",
                &MemoryPatch {
                    topic: Some("runtime".into()),
                    source: Some("issue-1667".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .0;
        let tool = tmp.tool();
        let call = |action: &str, scope: &str| {
            serde_json::json!({"action":action,"id":entry.metadata.id,"scope":scope}).to_string()
        };
        assert!(
            tool.execute(&call("pin", "project"), &ctx(tmp.path()))
                .await
                .is_error
        );
        assert!(!tmp.global().entries().unwrap()[0].metadata.pinned);
        assert!(
            !tool
                .execute(&call("pin", "global"), &ctx(tmp.path()))
                .await
                .is_error
        );
        let update = serde_json::json!({"action":"update","scope":"global","id":entry.metadata.id,"content":"corrected","source":"","topic":"architecture"}).to_string();
        assert!(!tool.execute(&update, &ctx(tmp.path())).await.is_error);
        let changed = tmp.global().entries().unwrap().remove(0);
        assert_eq!(changed.metadata.id, entry.metadata.id);
        assert_eq!(changed.metadata.created_at, entry.metadata.created_at);
        assert!(changed.metadata.source.is_none());
        assert!(changed.metadata.pinned);
        assert!(
            !tool
                .execute(&call("deprecate", "global"), &ctx(tmp.path()))
                .await
                .is_error
        );
        assert!(tmp.global().load().is_empty());
        let activate = serde_json::json!({"action":"update","scope":"global","id":entry.metadata.id,"status":"active"}).to_string();
        assert!(!tool.execute(&activate, &ctx(tmp.path())).await.is_error);
        assert_eq!(tmp.global().load(), ["corrected"]);
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn list_filters_topic_and_state_without_exposing_unrelated_entries() {
        let tmp = Fixture::new();
        tmp.project()
            .remember(
                "RUNTIME_MARKER",
                &MemoryPatch {
                    topic: Some("runtime".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        tmp.project()
            .remember(
                "OTHER_MARKER",
                &MemoryPatch {
                    topic: Some("other".into()),
                    status: Some(MemoryStatus::Deprecated),
                    ..Default::default()
                },
            )
            .unwrap();
        let result = tmp
            .tool()
            .execute(
                r#"{"action":"list","topic":"runtime","status":"active"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("RUNTIME_MARKER"));
        assert!(result.content.contains("ID: m-"));
        assert!(!result.content.contains("OTHER_MARKER"));
        let result = tmp
            .tool()
            .execute(
                r#"{"action":"list","status":"deprecated"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(result.content.contains("OTHER_MARKER"));
        assert!(!result.content.contains("RUNTIME_MARKER"));
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn invalid_action_fields_never_turn_into_success_or_partial_writes() {
        let tmp = Fixture::new();
        tmp.project().append("KEEP").unwrap();
        let before = tmp.project().full_text().unwrap();
        for args in [
            r#"{"action":"remember","content":"bad","scope":"typo"}"#,
            r#"{"action":"update","id":"missing"}"#,
            r#"{"action":"update","id":"missing","status":"future"}"#,
            r#"{"action":"pin","id":"missing","topic":"ignored"}"#,
            r#"{"action":"list","id":"ignored"}"#,
            r#"{"action":"remember","content":"bad","topics":"typo"}"#,
        ] {
            assert!(
                tmp.tool().execute(args, &ctx(tmp.path())).await.is_error,
                "{args}"
            );
            assert_eq!(tmp.project().full_text().unwrap(), before);
        }
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn explicit_keyword_scope_does_not_delete_other_scopes() {
        let tmp = Fixture::new();
        tmp.project().append("SHARED_KEY project").unwrap();
        tmp.global().append("SHARED_KEY global").unwrap();
        let result = tmp
            .tool()
            .execute(
                r#"{"action":"forget","keyword":"SHARED_KEY","scope":"global"}"#,
                &ctx(tmp.path()),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(tmp.project().load(), ["SHARED_KEY project"]);
        assert!(tmp.global().entries().unwrap().is_empty());
    }
}
