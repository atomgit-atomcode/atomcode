//! Prompt-cache prefix stability through the FULL coding assembly.
//!
//! The kernel proves the wire prefix is byte-stable / append-only with NoopHooks
//! (`atomcode-kernel/tests/cache_prefix.rs`). This test re-proves it through the
//! REAL assembly — every coding hook (MemoryHook rewriting the leading-system run,
//! Snapshot/Transcript, CurrentDate's tail projection, VerifyCadence, Telemetry)
//! plus the full toolset — which is exactly where the project's historical cache
//! breaks lived: the system/persona rebuilt per round, skill/tool reorder, or memory
//! re-injected each turn.

use std::sync::Arc;
use std::time::Duration;

use atomcode_coding::{assemble, prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolDef;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn cfg(working_dir: &std::path::Path) -> CodingAgentConfig {
    let mut c = CodingAgentConfig::new("k", "http://unused", "test-model", working_dir);
    c.stream_timeout = Duration::from_secs(5);
    c.request_timeout = Some(Duration::from_secs(5));
    c
}

// --- canonical wire serialization (mirrors the kernel's cache_prefix.rs) ---------
fn tool_block_repr(tools: &[ToolDef]) -> String {
    let mut s = String::new();
    for t in tools {
        s.push_str(&t.name);
        s.push('\u{1}');
        s.push_str(&t.description);
        s.push('\u{1}');
        s.push_str(&serde_json::to_string(&t.parameters).unwrap());
        s.push('\u{2}');
    }
    s
}

fn history_repr(messages: &[Message]) -> String {
    let mut s = String::new();
    for m in messages {
        s.push_str(&format!("{:?}", m.role));
        s.push('\u{1}');
        s.push_str(&m.text);
        s.push('\u{1}');
        for c in &m.tool_calls {
            s.push_str(&c.id);
            s.push('|');
            s.push_str(&c.name);
            s.push('|');
            s.push_str(&c.arguments);
        }
        s.push('\u{1}');
        s.push_str(m.tool_call_id.as_deref().unwrap_or(""));
        s.push('\u{2}');
    }
    s
}

fn is_strict_prefix(a: &str, b: &str) -> bool {
    a.len() < b.len() && b.as_bytes().starts_with(a.as_bytes())
}

/// Leading run of System messages — the persona plus any MemoryHook-injected block.
fn leading_system(messages: &[Message]) -> &[Message] {
    let n = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    &messages[..n]
}

/// Strip the CurrentDateHook tail: its `pre_request` appends exactly one trailing
/// `user` "Current date: …" message — ephemeral (not stored, not part of the cached
/// prefix). Everything before it is the byte-stable prefix.
fn without_date_tail(messages: &[Message]) -> &[Message] {
    match messages.last() {
        Some(m) if m.text.starts_with("Current date:") => &messages[..messages.len() - 1],
        _ => messages,
    }
}

fn text_turn(t: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(t.into()),
        StreamEvent::Done { truncated: false },
    ]
}

async fn drive(handle: &mut atomcode_kernel::agent::AgentHandle, text: &str) {
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .unwrap();
    while let Some(ev) = handle.events.recv().await {
        if matches!(ev, AgentEvent::TurnComplete { .. }) {
            break;
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn full_assembly_wire_prefix_is_cacheable_across_turns() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    // A memory the MemoryHook injects into the leading-system run — the run that must
    // then stay FROZEN across every subsequent round/turn.
    std::fs::write(home.path().join("memory.md"), "- the user prefers tabs\n").unwrap();

    let cfg = cfg(project.path());
    // Deterministic toolset: pin skills to an (empty) temp dir, MCP off, web off.
    // Memory ON — the hook that rewrites the leading-system run is the main risk.
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        skill_dirs: Some(vec![project.path().join("skills")]),
        plugin_skill_dirs: Vec::new(),
        mcp: false,
        memory: true,
        web: false,
        review: false,
        rate_limit_source: None,
    };
    let mut parts = prepare(&cfg, opts).await.unwrap();

    // Two text-only turns → one recorded provider call each.
    let provider = Arc::new(RecordingProvider::new(vec![
        text_turn("answer one"),
        text_turn("answer two"),
    ]));
    let calls = provider.calls();
    let mut h = assemble(&mut parts, &cfg, provider).unwrap().spawn();
    drive(&mut h, "first task").await;
    drive(&mut h, "second task").await;
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;

    let calls = calls.lock().unwrap();
    assert!(
        calls.len() >= 2,
        "expected >=2 recorded calls, got {}",
        calls.len()
    );

    let frozen_tools = tool_block_repr(&calls[0].1);
    let leading0 = history_repr(leading_system(&calls[0].0));
    assert!(!leading0.is_empty(), "there must be a leading system run");
    // Sanity: memory really did land in the frozen leading run (else we'd be
    // "verifying" a trivial persona-only system).
    assert!(
        calls[0]
            .0
            .iter()
            .take_while(|m| m.role == Role::System)
            .count()
            >= 2,
        "MemoryHook should add a system block after the persona"
    );

    for (i, (msgs, tools, _opts)) in calls.iter().enumerate() {
        // (1) tool block frozen byte-for-byte — no skill/MCP/tool reorder across calls.
        assert_eq!(
            tool_block_repr(tools),
            frozen_tools,
            "tool block must be byte-identical on call {i} (frozen, no reorder)"
        );
        // (2) leading system run (persona + MEMORY block) byte-identical — NOT rebuilt
        //     or re-injected per round/turn (the historical cache break).
        assert_eq!(
            history_repr(leading_system(msgs)),
            leading0,
            "leading system run must be byte-identical on call {i}"
        );
    }

    // (3) append-only: each call's stored history (minus the ephemeral date tail) is a
    //     STRICT byte prefix of the next — no head mutation, no mid-session rewrite.
    for w in calls.windows(2) {
        let prev = history_repr(without_date_tail(&w[0].0));
        let next = history_repr(without_date_tail(&w[1].0));
        assert!(
            is_strict_prefix(&prev, &next),
            "history must be append-only (strict byte prefix).\n  prev = {prev:?}\n  next = {next:?}"
        );
    }
}

/// CROSS-ASSEMBLY determinism: two INDEPENDENT assemblies (a fresh `HashMap` seed
/// each, the way two separate PROCESSES would differ) must emit a byte-identical
/// tool block + leading-system run. This is the cross-process system-hash property —
/// the guard against the historical skill/tool `HashMap`-iteration-order cache break
/// (the fix was ordered registration). Without it, a second process keys a different
/// prefix and never hits the first's cache.
#[tokio::test]
#[serial_test::serial]
async fn tool_block_and_system_are_deterministic_across_independent_assemblies() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    std::fs::write(home.path().join("memory.md"), "- prefers tabs\n").unwrap();
    let cfg = cfg(project.path());
    let opts = || PrepareOptions {
        session: SessionMode::Disabled,
        skill_dirs: Some(vec![project.path().join("skills")]),
        plugin_skill_dirs: Vec::new(),
        mcp: false,
        memory: true,
        web: true, // include web tools too — more tools = a stronger ordering check
        review: false,
        rate_limit_source: None,
    };

    async fn first_call(cfg: &CodingAgentConfig, opts: PrepareOptions) -> (String, String) {
        let mut parts = prepare(cfg, opts).await.unwrap();
        let provider = Arc::new(RecordingProvider::new(vec![text_turn("ok")]));
        let calls = provider.calls();
        let mut h = assemble(&mut parts, cfg, provider).unwrap().spawn();
        drive(&mut h, "task").await;
        h.commands.send(AgentCommand::Shutdown).unwrap();
        let _ = h.task.await;
        let calls = calls.lock().unwrap();
        (
            tool_block_repr(&calls[0].1),
            history_repr(leading_system(&calls[0].0)),
        )
    }

    let (tools_a, system_a) = first_call(&cfg, opts()).await;
    let (tools_b, system_b) = first_call(&cfg, opts()).await;

    assert_eq!(
        tools_a, tools_b,
        "tool block must be byte-identical across independent assemblies"
    );
    assert_eq!(
        system_a, system_b,
        "leading system run must be byte-identical across assemblies"
    );
}
