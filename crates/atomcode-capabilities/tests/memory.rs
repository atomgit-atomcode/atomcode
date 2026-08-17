//! Memory capability — kernel conformance gate (the same release gate every L1 hook
//! passes; see `atomcode_kernel::conformance`).
//!
//! The gate runs with POPULATED stores — including a global file padded past the
//! 64KB cap with multi-byte UTF-8 straddling the tail boundary, and a project store
//! big enough to trip the 4000-char prompt truncation — so the certified
//! must-not-panic surface is the REAL injection path (tail-read byte slicing +
//! truncation + push), not the empty-store no-op.

#![cfg(feature = "memory")]

use std::sync::Arc;

use atomcode_capabilities::memory::{MemoryHook, MemoryStore};

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary runs,
// so any memory persistence that resolves the global store without an explicit path
// never writes into the developer's real home. Inherited shell values are replaced.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[tokio::test]
async fn memory_hook_passes_kernel_conformance_with_populated_stores() {
    let dir = tempfile::tempdir().unwrap();

    // Global store: pad PAST the 64KB tail-read cap with multi-byte UTF-8 (3-byte
    // CJK) so the tail's start lands mid-character — the byte-slicing path must
    // recover via the newline scan, never panic.
    let global = MemoryStore::new(dir.path().join("g.md"));
    for i in 0..900 {
        global
            .append(&format!(
                "全局记忆条目第{i}号——多字节内容填充以越过六十四KB上限"
            ))
            .unwrap();
    }
    let len = std::fs::metadata(global.path()).unwrap().len();
    assert!(
        len > 64 * 1024,
        "fixture must exceed the 64KB cap (got {len})"
    );

    // Project store: enough to trip the 4000-char merged-prompt truncation.
    let project = MemoryStore::new(dir.path().join("p.md"));
    project.append(&"项目偏好".repeat(2000)).unwrap();

    let hook = MemoryHook::with_stores(global, project, "conformance");
    atomcode_kernel::conformance::hooks::check(Arc::new(hook))
        .await
        .assert_conformant();
}

#[tokio::test]
async fn memory_hook_passes_kernel_conformance_with_empty_stores() {
    let dir = tempfile::tempdir().unwrap();
    let hook = MemoryHook::with_stores(
        MemoryStore::new(dir.path().join("g.md")),
        MemoryStore::new(dir.path().join("p.md")),
        "conformance",
    );
    atomcode_kernel::conformance::hooks::check(Arc::new(hook))
        .await
        .assert_conformant();
}
