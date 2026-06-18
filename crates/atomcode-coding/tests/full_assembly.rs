//! C1 — the FULL assembly, end to end against a scripted provider and an isolated
//! `$ATOMCODE_HOME`. One sequential test fn (the env var is process-global; parallel
//! tests would race it) walking the whole lifecycle:
//!
//!   fresh prepare → memory injected → turn persists snapshot/meta/jsonl →
//!   resume continues the SAME session (turn_id monotonic across processes) →
//!   respawn on the SAME parts preserves allow-always approval grants.

use std::sync::Arc;
use std::time::Duration;

use atomcode_coding::{assemble, prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::Role;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolCall;

fn cfg(working_dir: &std::path::Path) -> CodingAgentConfig {
    let mut c = CodingAgentConfig::new("k", "http://unused", "test-model", working_dir);
    c.stream_timeout = Duration::from_secs(5);
    c.request_timeout = Some(Duration::from_secs(5));
    c
}

fn text_turn(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(text.into()), StreamEvent::Done { truncated: false }]
}

/// Drive one user turn; answer every approval Request with `decision`.
async fn drive(
    handle: &mut atomcode_kernel::agent::AgentHandle,
    text: &str,
    decision: Option<&str>,
) -> (Vec<AgentEvent>, usize) {
    handle.commands.send(AgentCommand::SendMessage { text: text.into(), images: vec![] }).unwrap();
    let mut events = Vec::new();
    let mut requests = 0;
    while let Some(ev) = handle.events.recv().await {
        match &ev {
            AgentEvent::Request { id, .. } => {
                requests += 1;
                let d = decision.expect("unexpected approval Request in this phase");
                handle
                    .commands
                    .send(AgentCommand::Respond {
                        id: *id,
                        value: serde_json::json!({ "decision": d }),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { .. } => {
                events.push(ev);
                break;
            }
            _ => {}
        }
        events.push(ev);
    }
    (events, requests)
}

#[tokio::test]
async fn full_assembly_lifecycle() {
    // ---- Isolated world: $ATOMCODE_HOME + a project dir, both temp. Skill dirs
    // are pinned to a temp dir too — the home-based default would scan the HOST
    // machine's real ~/.claude/skills and leak host state into this test.
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cfg = cfg(project.path());
    let opts = || PrepareOptions {
        skill_dirs: Some(vec![project.path().join("skills")]),
        ..Default::default()
    };

    // A global memory the MemoryHook must inject.
    std::fs::write(home.path().join("memory.md"), "- the user prefers tabs\n").unwrap();

    // ================= Phase 1: fresh prepare + first turn =================
    let mut parts = prepare(&cfg, opts()).await.unwrap();

    let session_id = parts.session.as_ref().unwrap().id.clone();
    let sessions_root = parts.session.as_ref().unwrap().manager.root().to_path_buf();

    let provider1 = Arc::new(RecordingProvider::new(vec![text_turn("first answer")]));
    let calls1 = provider1.calls();
    let mut h1 = assemble(&mut parts, &cfg, provider1).unwrap().spawn();
    let (_, reqs) = drive(&mut h1, "the first task", None).await;
    assert_eq!(reqs, 0, "a text-only turn asks no approval");
    h1.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h1.task.await;

    // The FULL toolset rode the wire: core + codeintel + web + skills + recall + review.
    {
        let calls = calls1.lock().unwrap();
        let defs: Vec<&str> = calls[0].1.iter().map(|d| d.name.as_str()).collect();
        for expected in
            ["bash", "read_file", "list_symbols", "web_fetch", "use_skill", "recall", "code_review"]
        {
            assert!(defs.contains(&expected), "missing tool {expected}: {defs:?}");
        }
    }

    // Leading-system run order: persona → SESSION CONTEXT → MEMORY, all BEFORE the user.
    {
        let calls = calls1.lock().unwrap();
        let first = &calls[0].0;
        let shape = || first.iter().map(|m| (&m.role, m.text[..m.text.len().min(30)].to_string())).collect::<Vec<_>>();
        assert_eq!(first[0].role, Role::System, "persona leads");
        assert!(
            first[1].role == Role::System && first[1].text.starts_with("=== SESSION CONTEXT ==="),
            "session-context block injected after persona: {:?}",
            shape()
        );
        assert!(
            first[2].role == Role::System && first[2].text.starts_with("=== MEMORY ==="),
            "memory block injected after the context block: {:?}",
            shape()
        );
        assert!(first[2].text.contains("prefers tabs"));
        // StatusReminderHook is SKIPPED on a turn's round 1 (this is a single-round text
        // turn), so NO status tail rides this request — the last message is the user turn,
        // not a "<system-reminder>". (The reminder appears from round 2 onward; covered by
        // the hook's own unit tests.)
        assert_eq!(first.last().unwrap().role, Role::User, "round 1 ends at the user turn");
        // Scope to USER messages: the reminder is a user-role tail. (The persona — a System
        // message — legitimately *mentions* the `<system-reminder>` tag to explain it, so a
        // blanket text search would false-positive on the persona.)
        assert!(
            !first.iter().any(|m| m.role == Role::User && m.text.contains("<system-reminder>")),
            "no status reminder (user tail) on a turn's round 1: {:?}",
            shape()
        );
    }

    // The turn persisted all three session files.
    assert!(sessions_root.join(format!("{session_id}.snapshot")).exists(), "snapshot persisted");
    assert!(sessions_root.join(format!("{session_id}.meta")).exists(), "meta persisted");
    assert!(sessions_root.join(format!("{session_id}.jsonl")).exists(), "transcript persisted");

    // ===== Phase 2: RESPAWN on the SAME parts (model swap) continues the session ====
    // assemble() reloads the latest on-disk snapshot for a session-bound parts — a
    // plain re-assemble can never rewind a live session (the review's must-fix).
    let provider1b = Arc::new(RecordingProvider::new(vec![text_turn("post-swap answer")]));
    let calls1b = provider1b.calls();
    let mut h1b = assemble(&mut parts, &cfg, provider1b).unwrap().spawn();
    let _ = drive(&mut h1b, "the swap task", None).await;
    h1b.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h1b.task.await;
    {
        let calls = calls1b.lock().unwrap();
        let first = &calls[0].0;
        assert!(
            first.iter().any(|m| m.text == "the first task"),
            "respawn on the same parts carries the conversation"
        );
    }

    // ================= Phase 3: resume continues the SAME session =================
    let mut parts2 = prepare(&cfg, PrepareOptions {
        session: SessionMode::Resume(session_id.clone()),
        ..opts()
    })
    .await
    .unwrap();
    let provider2 = Arc::new(RecordingProvider::new(vec![text_turn("second answer")]));
    let calls2 = provider2.calls();
    let mut h2 = assemble(&mut parts2, &cfg, provider2).unwrap().spawn();
    let _ = drive(&mut h2, "the second task", None).await;
    h2.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h2.task.await;

    // History continued: the resumed provider saw the first turn's exchange.
    {
        let calls = calls2.lock().unwrap();
        let first = &calls[0].0;
        assert!(first.iter().any(|m| m.text == "the first task"), "resume carries history");
        assert!(first.iter().any(|m| m.text == "the swap task"), "incl. the respawned turn");
        assert!(first.iter().any(|m| m.text == "the second task"));
        let system_count = first.iter().filter(|m| m.role == Role::System).count();
        assert_eq!(
            system_count, 3,
            "persona + session-context + memory exactly once each (resume reconciles in place, no double-inject)"
        );
    }

    // Transcript turn_ids are MONOTONIC across the resume (the 8c06a9e2 seeding,
    // end-to-end through prepare/assemble): lines say turn 1 then turn 2.
    let jsonl = std::fs::read_to_string(sessions_root.join(format!("{session_id}.jsonl"))).unwrap();
    let turn_ids: Vec<u64> = jsonl
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["turn_id"].as_u64().unwrap())
        .collect();
    assert_eq!(
        turn_ids,
        vec![1, 2, 3],
        "turn ids continue across respawn AND resume — no duplicate keys"
    );

    // ============== Phase 4: respawn on the SAME parts keeps approval grants ======
    let mut parts3 = prepare(&cfg, PrepareOptions {
        session: SessionMode::Disabled, // independent of the session above
        ..opts()
    })
    .await
    .unwrap();

    let risky = || ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({ "command": "git reset --hard HEAD~3" }).to_string(),
    };
    let provider3 = Arc::new(RecordingProvider::new(vec![
        vec![StreamEvent::ToolCall(risky()), StreamEvent::Done { truncated: false }],
        text_turn("done"),
    ]));
    let mut h3 = assemble(&mut parts3, &cfg, provider3).unwrap().spawn();
    let (_, reqs) = drive(&mut h3, "do the risky thing", Some("allow_always")).await;
    assert_eq!(reqs, 1, "the risky call asks once");
    h3.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h3.task.await;

    // RESPAWN (model swap) on the SAME parts: the identical risky call must NOT ask
    // again — the grant store survives because the approval handle lives in parts.
    let provider4 = Arc::new(RecordingProvider::new(vec![
        vec![StreamEvent::ToolCall(risky()), StreamEvent::Done { truncated: false }],
        text_turn("done again"),
    ]));
    let mut h4 = assemble(&mut parts3, &cfg, provider4).unwrap().spawn();
    let (_, reqs) = drive(&mut h4, "again", None).await;
    assert_eq!(reqs, 0, "allow-always grant survives the respawn (parts own the store)");
    h4.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h4.task.await;

    std::env::remove_var("ATOMCODE_HOME");
}
