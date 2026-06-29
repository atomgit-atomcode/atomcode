//! Conformance gates for the `session` capability: the `recall` tool and the three
//! persistence/injection hooks must each satisfy the kernel seam contracts
//! (must-not-panic, bounded, meta-preserving, return-shape) — the same gates a
//! third-party provider/tool/hook is held to.
#![cfg(feature = "session")]

use std::sync::{Arc, Mutex, OnceLock};

use atomcode_capabilities::session::{
    RecallTool, SessionManager, SnapshotHook, StatusReminderHook, TranscriptHook,
};
use atomcode_kernel::conformance;
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::tool::Tool;

struct AtomcodeHomeGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
}

impl AtomcodeHomeGuard {
    fn set(path: &std::path::Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(()));
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("ATOMCODE_HOME");
        std::env::set_var("ATOMCODE_HOME", path);
        Self { _lock: guard, prev }
    }
}

impl Drop for AtomcodeHomeGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
    }
}

#[tokio::test]
async fn recall_tool_passes_kernel_tool_conformance() {
    // Isolate $ATOMCODE_HOME so the tool resolves an empty (temp) sessions tree rather
    // than the developer's real one.
    let home = tempfile::tempdir().unwrap();
    let _home = AtomcodeHomeGuard::set(home.path());

    let tool: Arc<dyn Tool> = Arc::new(RecallTool::new());
    conformance::tool::check(tool, &[r#"{"query":"oauth refresh"}"#, "{\"query\":\"\"}"])
        .await
        .assert_conformant();
}

#[tokio::test]
async fn transcript_hook_passes_lifecycle_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::with_root(dir.path()));
    let h: Arc<dyn LifecycleHooks> = Arc::new(TranscriptHook::new(mgr, "s1"));
    conformance::hooks::check(h).await.assert_conformant();
}

#[tokio::test]
async fn snapshot_hook_passes_lifecycle_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(SessionManager::with_root(dir.path()));
    let h: Arc<dyn LifecycleHooks> = Arc::new(SnapshotHook::new(mgr, "s1", "/proj"));
    conformance::hooks::check(h).await.assert_conformant();
}

#[tokio::test]
async fn status_reminder_hook_passes_lifecycle_conformance() {
    let h: Arc<dyn LifecycleHooks> = Arc::new(StatusReminderHook::new());
    conformance::hooks::check(h).await.assert_conformant();
}
