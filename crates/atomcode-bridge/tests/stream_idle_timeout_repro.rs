//! Bug reproduction for `[Error: stream idle timeout]`.
//!
//! The fix threads `cfg.stream_timeout` (default 300s, env
//! `ATOMCODE_STREAM_TIMEOUT_SECS`) down to the L1 provider adapter's
//! `idle_timeout` field in two places:
//!   1. `crates/atomcode-bridge/src/runtime.rs::build_provider` — three
//!      provider branches (claude/anthropic, ollama, openai-compat default).
//!   2. `crates/atomcode-coding/src/assemble.rs::build_coding_agent` — the
//!      OpenAI-compat branch used by the clix / unit-test entry point.
//!
//! Without that threading, `*Config::new`'s hardcoded 120s default stays in
//! effect and a thinking model's legitimate >120s mid-reasoning silence is
//! misclassified as `[Error: stream idle timeout]`.
//!
//! This is a STATIC source check. A plain `contains` would falsely pass on
//! the commented-out line, so we strip `//`-prefixed lines first. The test
//! files themselves live OUTSIDE the source files being checked, so there is
//! no self-reference: the string literals in this test are not present in
//! `runtime.rs` / `assemble.rs`.

use std::path::PathBuf;

fn crate_root(crate_name: &str) -> PathBuf {
    // tests/ is `<crate>/tests/`; root is two levels up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("..").join(crate_name).canonicalize().unwrap_or_else(|_| {
        panic!("failed to canonicalize crate root for {crate_name}")
    })
}

/// Read a source file, drop full-line `//` comments, return the live source.
fn live_source(path: &PathBuf) -> String {
    let src = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn build_provider_threads_stream_timeout_in_all_three_branches() {
    let runtime = crate_root("atomcode-bridge").join("src/runtime.rs");
    let live = live_source(&runtime);

    let ac_line = "ac.idle_timeout = cfg.stream_timeout;";
    let oc_line = "oc.idle_timeout = cfg.stream_timeout;";
    let pc_line = "pc.idle_timeout = cfg.stream_timeout;";

    assert!(
        live.contains(ac_line),
        "BUG REPRODUCED: Anthropic branch missing live `ac.idle_timeout = cfg.stream_timeout;` \
         in runtime.rs::build_provider — provider falls back to hardcoded 120s, thinking model \
         mid-reasoning silence >120s triggers spurious `[Error: stream idle timeout]`"
    );
    assert!(
        live.contains(oc_line),
        "BUG REPRODUCED: Ollama branch missing live `oc.idle_timeout = cfg.stream_timeout;` \
         in runtime.rs::build_provider — same 120s hardcoded fallback bug"
    );
    assert!(
        live.contains(pc_line),
        "BUG REPRODUCED: OpenAI-compat branch missing live `pc.idle_timeout = cfg.stream_timeout;` \
         in runtime.rs::build_provider — same 120s hardcoded fallback bug"
    );
}

#[test]
fn build_coding_agent_threads_stream_timeout() {
    let assemble = crate_root("atomcode-coding").join("src/assemble.rs");
    let live = live_source(&assemble);

    let line = "provider_cfg.idle_timeout = cfg.stream_timeout;";
    assert!(
        live.contains(line),
        "BUG REPRODUCED: build_coding_agent missing live `provider_cfg.idle_timeout = \
         cfg.stream_timeout;` in assemble.rs — OpenAiCompatConfig::new's hardcoded 120s default \
         stays in effect, thinking model mid-reasoning silence >120s triggers spurious \
         `[Error: stream idle timeout]`"
    );
}
