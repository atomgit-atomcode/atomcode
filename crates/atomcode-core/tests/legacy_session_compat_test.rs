use atomcode_core::conversation::message::{MessageContent, Role};
use atomcode_core::session::{Session, SessionId, SessionManager};
use std::path::{Path, PathBuf};

#[ctor::ctor]
fn isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn fixture(name: &str) -> Session {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/session")
        .join(name);
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&json).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

#[test]
fn full_legacy_fixture_preserves_persisted_fields() {
    let session = fixture("legacy_full.json");

    assert_eq!(session.id.as_str(), "11111111-1111-4111-8111-111111111111");
    assert_eq!(session.name, "legacy-full");
    assert_eq!(
        session.working_dir,
        Path::new("/tmp/atomcode-fixture-project")
    );
    assert_eq!(
        (session.created_at, session.updated_at),
        (1_700_000_000, 1_700_000_123)
    );
    assert_eq!(session.messages.len(), 7);

    match &session.messages[1].content {
        MessageContent::MultiPart { text, images } => {
            assert_eq!(text.as_deref(), Some("inspect this image"));
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].media_type, "image/png");
            assert_eq!(images[0].data, "aW1hZ2UtZml4dHVyZQ==");
        }
        other => panic!("expected multipart message, got {other:?}"),
    }

    match &session.messages[2].content {
        MessageContent::AssistantWithToolCalls {
            text,
            tool_calls,
            reasoning_content,
            thinking_blocks,
        } => {
            assert_eq!(text.as_deref(), Some("I will inspect it."));
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call-inline");
            assert_eq!(tool_calls[0].name, "read_file");
            assert_eq!(tool_calls[0].arguments, r#"{"path":"README.md"}"#);
            assert_eq!(reasoning_content.as_deref(), Some("plain reasoning"));
            assert_eq!(thinking_blocks.len(), 1);
            assert_eq!(thinking_blocks[0].text, "signed reasoning");
            assert_eq!(thinking_blocks[0].signature, "anthropic-signature");
        }
        other => panic!("expected assistant tool call, got {other:?}"),
    }

    match &session.messages[5].content {
        MessageContent::ToolResultRef(result) => {
            assert_eq!(result.call_id, "call-ref");
            assert_eq!(result.summary, "cached failure summary");
            assert_eq!(result.byte_size, 4096);
            assert!(!result.success);
        }
        other => panic!("expected referenced tool result, got {other:?}"),
    }

    assert!(session.messages[6].synthetic);
    assert_eq!(
        session.messages[6].internal_origin.as_deref(),
        Some("verify_cadence")
    );
    assert_eq!(session.display_messages.len(), 2);
    assert_eq!(session.display_messages[0].after_message, 0);
    assert_eq!(session.display_messages[0].message.role, Role::Assistant);
    assert_eq!(
        session.display_messages[0].message.text(),
        Some("local preamble")
    );
    assert_eq!(session.display_messages[1].after_message, 7);
    assert_eq!(session.display_messages[1].message.role, Role::User);
    assert_eq!(
        session.cold_summaries,
        ["older summary one", "older summary two"]
    );
    assert!(session.user_renamed);
    assert!(session.ai_named);
    assert_eq!(session.turn_stats.len(), 2);
    assert_eq!(session.turn_stats[0].turn_count, 2);
    assert_eq!(session.turn_stats[0].used_tokens, 1234);
    assert_eq!(session.turn_stats[0].ctx_window, 8192);
    assert!(session.turn_stats[1].errored);
}

#[test]
fn minimal_legacy_fixture_applies_additive_defaults() {
    let session = fixture("legacy_minimal.json");

    assert_eq!(session.id.as_str(), "legacy-session-001");
    assert_eq!(session.messages.len(), 2);
    assert!(!session.messages[0].synthetic);
    assert_eq!(session.messages[0].internal_origin, None);
    assert!(session.display_messages.is_empty());
    assert!(session.cold_summaries.is_empty());
    assert!(!session.user_renamed);
    assert!(!session.ai_named);
    assert!(session.turn_stats.is_empty());

    match &session.messages[1].content {
        MessageContent::AssistantWithToolCalls {
            reasoning_content,
            thinking_blocks,
            ..
        } => {
            assert_eq!(reasoning_content, &None);
            assert!(thinking_blocks.is_empty());
        }
        other => panic!("expected old assistant shape, got {other:?}"),
    }
}

#[test]
fn load_any_finds_legacy_json_across_project_buckets() {
    let project_a = PathBuf::from("/tmp/atomcode-load-any-a");
    let project_b = PathBuf::from("/tmp/atomcode-load-any-b");
    let mut session = fixture("legacy_minimal.json");
    session.id = SessionId::from_string("legacy-load-any-fixture".to_string());
    session.working_dir = project_b.clone();

    SessionManager::new(&project_b).save(&session).unwrap();
    assert!(SessionManager::new(&project_a).load(&session.id).is_err());

    let loaded = SessionManager::load_any(&session.id).unwrap();
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.working_dir, project_b);
}
