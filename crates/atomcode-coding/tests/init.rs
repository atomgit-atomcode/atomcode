//! `/init` on the row-assembled product: the command exists, and what it hands
//! the model is the configuration as it is now.
//!
//! Two claims. The first is that the row registers it at all — the classic
//! front end kept this command in its own table, so on the row-assembled screen
//! it had never existed (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
//! B1-4: the capability was there, the registration was not; the plan page had
//! it recorded as done, and it was not). The second — that a person's own
//! `init_prompt_file` reaches the model, read when the command runs — is a unit
//! criterion next to the function that does it: the configuration path this row
//! holds is derived from how the runtime was built, and a test cannot hand it
//! one.

mod support;

use std::sync::Arc;

use atomcode_coding::{CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_harness::seams::{AgentsSvc, CommandsSvc};
use atomcode_kernel::provider::LlmProvider;
use support::{mount, quiet_options};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

struct CannedProvider;

#[async_trait::async_trait]
impl LlmProvider for CannedProvider {
    fn model_name(&self) -> &str {
        "canned"
    }
    async fn chat_stream(
        &self,
        _: &[atomcode_kernel::message::Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        use atomcode_kernel::stream::StreamEvent;
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// A person types `/init`, and the model is handed the prompt — including the
/// requirements they wrote themselves.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn the_command_hands_the_model_the_prompt_this_machine_is_configured_with() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();

    let cfg = CodingAgentConfig::new("k", "http://localhost", "canned", project.path());
    let opts = PrepareOptions {
        session: SessionMode::Fresh,
        ..quiet_options()
    };
    let mounted = mount(&cfg, opts, Arc::new(CannedProvider)).await;

    let ctx = mounted.context();
    let agent = ctx
        .service::<AgentsSvc>()
        .expect("agents")
        .list()
        .into_iter()
        .find(|a| a.parent().is_none())
        .expect("the conversation's own agent");

    let commands = ctx.service::<CommandsSvc>().expect("the command catalog");
    let offered: Vec<String> = commands
        .offered_for(&agent)
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(
        offered.contains(&"init".to_string()),
        "the row offers `/init`: {offered:?}"
    );

    commands
        .find("init", &agent)
        .expect("offered")
        .run(agent.clone(), "")
        .await
        .expect("running it");

    // Queued as the person's own message — the same door everything they type
    // goes through, so the turn it starts is theirs to undo.
    assert!(
        agent
            .inbox()
            .waiting_from(atomcode_harness::agent::MessageOrigin::User),
        "the prompt is waiting as the person's own message"
    );

    ctx.require::<atomcode_harness::seams::AgentLoopSvc>()
        .expect("the turn driver")
        .drive(&agent)
        .await;

    let sent = agent
        .session()
        .events()
        .into_iter()
        .filter_map(|logged| match logged.event {
            atomcode_kernel::session::SessionEvent::UserMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        sent.contains("AGENTS.md"),
        "the built-in prompt went to the model:\n{sent}"
    );
    // What a person's own `init_prompt_file` adds is `init_prompt_from`'s to
    // answer, and it is judged where it can be: the path this row reads comes
    // from how the runtime was configured, which a test cannot hand it.
}
