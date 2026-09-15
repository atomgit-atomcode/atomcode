//! C1 — the FULL assembly, end to end against a scripted provider and an isolated
//! `$ATOMCODE_HOME`. One sequential test fn (the env var is process-global; parallel
//! tests would race it) walking the whole lifecycle:
//!
//!   fresh prepare → memory injected → turn persists snapshot/meta/jsonl →
//!   resume continues the SAME session (turn_id monotonic across processes) →
//!   respawn on the SAME parts preserves allow-always approval grants.

use std::sync::Arc;
use std::time::Duration;

mod support;

use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::Role;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolCall;
use support::mount_parts;

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

fn text_turn(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::Done { truncated: false },
    ]
}

/// Drive one user turn; answer every approval Request with `decision`.
async fn drive(
    handle: &mut atomcode_kernel::agent::AgentHandle,
    text: &str,
    decision: Option<&str>,
) -> (Vec<AgentEvent>, usize) {
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: vec![],
        })
        .unwrap();
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
async fn the_assembly_lifecycle() {
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
    let parts = prepare(&cfg, opts()).await.unwrap();

    let session_id = parts.session.as_ref().unwrap().id.clone();
    let sessions_root = parts.session.as_ref().unwrap().manager.root().to_path_buf();

    let provider1 = Arc::new(RecordingProvider::new(vec![text_turn("first answer")]));
    let calls1 = provider1.calls();
    let mut mounted_h1 = mount_parts(&parts, &cfg, &opts(), provider1).await;
    let h1 = &mut mounted_h1.handle;
    let (_, reqs) = drive(h1, "the first task", None).await;
    assert_eq!(reqs, 0, "a text-only turn asks no approval");
    mounted_h1.shutdown().await;

    // The FULL toolset rode the wire: core + codeintel + web + skills + recall + review.
    {
        let calls = calls1.lock().unwrap();
        let defs: Vec<&str> = calls[0].1.iter().map(|d| d.name.as_str()).collect();
        for expected in [
            "bash",
            "read_file",
            "list_symbols",
            "web_fetch",
            "use_skill",
            "recall",
            "code_review",
        ] {
            assert!(
                defs.contains(&expected),
                "missing tool {expected}: {defs:?}"
            );
        }
    }

    // Leading-system run order: persona → SESSION CONTEXT → MEMORY, all BEFORE the user.
    {
        let calls = calls1.lock().unwrap();
        let first = &calls[0].0;
        let shape = || {
            first
                .iter()
                .map(|m| (&m.role, m.text[..m.text.len().min(30)].to_string()))
                .collect::<Vec<_>>()
        };
        // The prompt is one composed system message — the persona first, then the
        // fragments the rows contribute. Memory is not a fragment: it is injected
        // as a logged fact, so it arrives as its own message, in front of the ask.
        assert_eq!(first[0].role, Role::System, "the prompt leads");
        let composed = &first[0].text;
        let persona = composed
            .find("You are AtomCode")
            .unwrap_or_else(|| panic!("no persona: {:?}", shape()));
        let context = composed
            .find("=== SESSION CONTEXT ===")
            .unwrap_or_else(|| panic!("no session context: {:?}", shape()));
        assert!(
            persona < context,
            "the persona leads and the session context follows it: {:?}",
            shape()
        );
        let memory = first
            .iter()
            .position(|m| m.text.starts_with("=== MEMORY ==="))
            .unwrap_or_else(|| panic!("no memory: {:?}", shape()));
        let ask = first
            .iter()
            .position(|m| m.role == Role::User && m.text == "the first task")
            .unwrap_or_else(|| panic!("no ask: {:?}", shape()));
        assert!(memory < ask, "memory is in front of the ask: {:?}", shape());
        assert!(first[memory].text.contains("prefers tabs"));
        // No per-round status/date <system-reminder> tail rides any request: the date lives in
        // the frozen persona anchor and the per-round StatusReminderHook was removed from the
        // production hook chain, so the last message here is the user turn, not a reminder.
        assert_eq!(
            first.last().unwrap().role,
            Role::User,
            "round 1 ends at the user turn"
        );
        // Scope to USER messages: the reminder is a user-role tail. (The persona — a System
        // message — legitimately *mentions* the `<system-reminder>` tag to explain it, so a
        // blanket text search would false-positive on the persona.)
        assert!(
            !first
                .iter()
                .any(|m| m.role == Role::User && m.text.contains("<system-reminder>")),
            "no status reminder (user tail) on a turn's round 1: {:?}",
            shape()
        );
    }

    // The turn persisted all three session files.
    assert!(
        sessions_root
            .join(format!("{session_id}.snapshot"))
            .exists(),
        "snapshot persisted"
    );
    assert!(
        sessions_root.join(format!("{session_id}.meta")).exists(),
        "meta persisted"
    );
    assert!(
        sessions_root.join(format!("{session_id}.jsonl")).exists(),
        "transcript persisted"
    );

    // ===== Phase 2: RESPAWN on the SAME parts (model swap) continues the session ====
    // assemble() reloads the latest on-disk snapshot for a session-bound parts — a
    // plain re-assemble can never rewind a live session (the review's must-fix).
    let provider1b = Arc::new(RecordingProvider::new(vec![text_turn("post-swap answer")]));
    let calls1b = provider1b.calls();
    let mut mounted_h1b = mount_parts(&parts, &cfg, &opts(), provider1b).await;
    let h1b = &mut mounted_h1b.handle;
    let _ = drive(h1b, "the swap task", None).await;
    mounted_h1b.shutdown().await;
    {
        let calls = calls1b.lock().unwrap();
        let first = &calls[0].0;
        assert!(
            first.iter().any(|m| m.text == "the first task"),
            "respawn on the same parts carries the conversation"
        );
    }

    // ================= Phase 3: resume continues the SAME session =================
    // This phase models a new process. Dropping the previous parts releases its
    // active-session lease before the new owner resumes the persisted session.
    drop(parts);
    let resumed = PrepareOptions {
        session: SessionMode::Resume(session_id.clone()),
        ..opts()
    };
    let parts2 = prepare(&cfg, resumed.clone()).await.unwrap();
    let provider2 = Arc::new(RecordingProvider::new(vec![text_turn("second answer")]));
    let calls2 = provider2.calls();
    let mut mounted_h2 = mount_parts(&parts2, &cfg, &resumed, provider2).await;
    let h2 = &mut mounted_h2.handle;
    let _ = drive(h2, "the second task", None).await;
    mounted_h2.shutdown().await;

    // History continued: the resumed provider saw the first turn's exchange.
    {
        let calls = calls2.lock().unwrap();
        let first = &calls[0].0;
        assert!(
            first.iter().any(|m| m.text == "the first task"),
            "resume carries history"
        );
        assert!(
            first.iter().any(|m| m.text == "the swap task"),
            "incl. the respawned turn"
        );
        assert!(first.iter().any(|m| m.text == "the second task"));
        let system: String = first
            .iter()
            .filter(|m| m.role == Role::System && !m.synthetic)
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        for once in [
            "You are AtomCode",
            "=== SESSION CONTEXT ===",
            "=== MEMORY ===",
        ] {
            assert_eq!(
                system.matches(once).count(),
                1,
                "a resumed session composes each block exactly once, not twice: {once}"
            );
        }
    }

    // Transcript turn_ids are MONOTONIC across the resume (the 8c06a9e2 seeding,
    // end-to-end through prepare/assemble): lines say turn 1 then turn 2.
    let jsonl = std::fs::read_to_string(sessions_root.join(format!("{session_id}.jsonl"))).unwrap();
    let turn_ids: Vec<u64> = jsonl
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["turn_id"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(
        turn_ids,
        vec![1, 2, 3],
        "turn ids continue across respawn AND resume — no duplicate keys"
    );

    // ============== Phase 4: respawn on the SAME parts keeps approval grants ======
    let sessionless = PrepareOptions {
        session: SessionMode::Disabled, // independent of the session above
        ..opts()
    };
    let parts3 = prepare(&cfg, sessionless.clone()).await.unwrap();

    let risky = || ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({ "command": "git reset --hard HEAD~3" }).to_string(),
    };
    let provider3 = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(risky()),
            StreamEvent::Done { truncated: false },
        ],
        text_turn("done"),
    ]));
    let mut mounted_h3 = mount_parts(&parts3, &cfg, &sessionless, provider3).await;
    let h3 = &mut mounted_h3.handle;
    let (_, reqs) = drive(h3, "do the risky thing", Some("allow_always")).await;
    assert_eq!(reqs, 1, "the risky call asks once");
    mounted_h3.shutdown().await;

    // RESPAWN (model swap) on the SAME parts: the identical risky call must NOT ask
    // again — the grant store survives because the approval handle lives in parts.
    let provider4 = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(risky()),
            StreamEvent::Done { truncated: false },
        ],
        text_turn("done again"),
    ]));
    let mut mounted_h4 = mount_parts(&parts3, &cfg, &sessionless, provider4).await;
    let h4 = &mut mounted_h4.handle;
    let (_, reqs) = drive(h4, "again", None).await;
    assert_eq!(
        reqs, 0,
        "allow-always grant survives the respawn (parts own the store)"
    );
    mounted_h4.shutdown().await;
}
