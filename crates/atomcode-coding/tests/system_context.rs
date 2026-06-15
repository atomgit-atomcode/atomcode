//! e2e: the SessionContextHook block (env + project instructions + git snapshot) actually
//! reaches the provider as a leading system message when a coding agent is assembled.

use atomcode_coding::{build_coding_agent_with, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;
use atomcode_kernel::message::Role;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use std::sync::Arc;

#[tokio::test]
async fn session_context_block_reaches_the_provider() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("AGENTS.md"), "PROJECT RULE: always foo").unwrap();
    // A git repo so the git-status snapshot section is included.
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
    ] {
        std::process::Command::new("git").args(&args).current_dir(d.path()).output().unwrap();
    }

    let provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let calls = provider.calls(); // capture the shared handle before moving the provider

    let cfg = CodingAgentConfig::new("k", "http://localhost", "test-model", d.path());
    let agent = build_coding_agent_with(&cfg, provider);
    let _ = agent.run_to_completion("hello", AutoRespond::AllowAll).await;

    let recorded = calls.lock().unwrap();
    assert!(!recorded.is_empty(), "the provider must have been called");
    let (messages, _, _) = &recorded[0];
    let sys: String = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n\n");

    assert!(sys.contains("SESSION CONTEXT"), "context block present:\n{sys}");
    assert!(sys.contains("Working directory:"), "env block present");
    assert!(
        sys.contains("PROJECT INSTRUCTIONS") && sys.contains("PROJECT RULE: always foo"),
        "AGENTS.md instructions injected"
    );
    assert!(sys.contains("GIT STATUS"), "git snapshot present");
    // The persona's static parity sections ride along too.
    assert!(sys.contains("## GIT COMMITS:"), "persona git-commit trailer rule present");
}
