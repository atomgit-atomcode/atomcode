//! Regression guard for `[Error: stream idle timeout]` on thinking models.
//!
//! Every provider-construction site must thread its config's `stream_timeout`
//! (default 300s for coding, env `ATOMCODE_STREAM_TIMEOUT_SECS`; 120s for review)
//! into the L1 adapter's byte-idle `idle_timeout` — otherwise `*Config::new`'s
//! hardcoded 120s default silently governs, and a thinking model's legitimate
//! >120s mid-reasoning silence is misclassified as `[Error: stream idle timeout]`.
//!
//! NOTE — this is a STATIC source check (`.idle_timeout = ... .stream_timeout`),
//! not a behavioural one: the built provider is an opaque `Arc<dyn LlmProvider>`
//! whose `idle_timeout` isn't observable, so we can't assert the propagated value
//! post-build. It's a coarse regression net (comment-stripped so a commented-out
//! line won't pass) and it degrades GRACEFULLY: when a source file can't be read
//! (packaged/vendored build outside the `crates/<name>/` workspace layout) the
//! check SKIPS rather than panicking. Replacing it with a real assertion wants a
//! small refactor exposing the assembled config — tracked as a follow-up.

use std::path::PathBuf;

/// Live (comment-stripped) source of `<workspace>/crates/<crate>/<rel>`, or
/// `None` when the file can't be read (build env without the workspace layout).
fn live_source(crate_name: &str, rel: &str) -> Option<String> {
    // tests/ is `<crate>/tests/`; sibling crates are one level up.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(crate_name).join(rel);
    let src = std::fs::read_to_string(&path).ok()?;
    Some(
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Assert every `needle` appears in the live source of `crate/rel`, or SKIP
/// (with a note) when the file isn't reachable in this build environment.
fn assert_threaded(crate_name: &str, rel: &str, needles: &[&str]) {
    let Some(live) = live_source(crate_name, rel) else {
        eprintln!("SKIP: {crate_name}/{rel} not readable in this build env — cannot verify");
        return;
    };
    for needle in needles {
        assert!(
            live.contains(needle),
            "BUG REGRESSED: `{needle}` missing (or commented out) in {crate_name}/{rel} — \
             the adapter falls back to the hardcoded 120s idle timeout, so a thinking model's \
             >120s mid-reasoning silence triggers a spurious `[Error: stream idle timeout]`"
        );
    }
}

#[test]
fn bridge_build_provider_threads_stream_timeout() {
    assert_threaded(
        "atomcode-bridge",
        "src/runtime.rs",
        &[
            "ac.idle_timeout = cfg.stream_timeout;",
            "oc.idle_timeout = cfg.stream_timeout;",
            "pc.idle_timeout = cfg.stream_timeout;",
        ],
    );
}

#[test]
fn coding_assemble_threads_stream_timeout() {
    assert_threaded(
        "atomcode-coding",
        "src/assemble.rs",
        &["provider_cfg.idle_timeout = cfg.stream_timeout;"],
    );
}

// The remaining sites are near-identical copies of `build_provider` (a known
// duplication) — each must thread the timeout the same way, or thinking models
// are cut off at 120s on that surface.

#[test]
fn acp_build_provider_threads_stream_timeout() {
    assert_threaded(
        "atomcode-cli",
        "src/acp/engine.rs",
        &[
            "ac.idle_timeout = cfg.stream_timeout;",
            "oc.idle_timeout = cfg.stream_timeout;",
            "pc.idle_timeout = cfg.stream_timeout;",
        ],
    );
}

#[test]
fn clix_build_providers_thread_stream_timeout() {
    assert_threaded("atomcode-clix", "src/code.rs", &["pc.idle_timeout = cfg.stream_timeout;"]);
    assert_threaded("atomcode-clix", "src/tel.rs", &["pc.idle_timeout = cfg.stream_timeout;"]);
}

#[test]
fn review_assemble_threads_stream_timeout() {
    assert_threaded(
        "atomcode-review",
        "src/assemble.rs",
        &["provider_cfg.idle_timeout = cfg.stream_timeout;"],
    );
}
