//! `/worklog` on the row-assembled product: the command exists, and the day it
//! hands the model is the person's real day.
//!
//! Two claims, and the second is the one worth a test. The first is that the
//! row registers it at all — the classic front end kept this command in its own
//! table, so on the row-assembled screen it had never existed
//! (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` B1-5: the
//! capability was there, the registration was not). The second is that running
//! it puts *this session's own turn* in front of the model, which is what makes
//! it a recap of the day rather than a template.

mod support;

use std::sync::Arc;

use atomcode_coding::{CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_harness::seams::{AgentsSvc, CommandsSvc};
use atomcode_kernel::provider::LlmProvider;
use support::{allow, mount, quiet_options, turn};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// Says something, so the turn it belongs to is a turn a day recap can find.
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

/// The whole point: a person types `/worklog`, and what the model is given is
/// the day this session was part of.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn the_command_recaps_the_day_this_session_actually_worked() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let cfg = CodingAgentConfig::new("k", "http://localhost", "canned", project.path());

    // A store-keeping session, which is what makes the row mount at all.
    let opts = PrepareOptions {
        session: SessionMode::Fresh,
        ..quiet_options()
    };
    let mut mounted = mount(&cfg, opts, Arc::new(CannedProvider)).await;

    // One real turn, so the day has something in it. `run_turn` through the
    // handle is what creates the conversation's own agent.
    turn(
        &mut mounted.handle,
        "port the quantizer to the NPU",
        allow(),
    )
    .await;

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
        offered.contains(&"worklog".to_string()),
        "the row offers `/worklog`: {offered:?}"
    );

    let said = commands
        .find("worklog", &agent)
        .expect("offered")
        .run(agent.clone(), "")
        .await
        .expect("running it");
    assert!(
        said.contains("复盘") || said.contains("recap"),
        "it says what it did: {said}"
    );

    // It queued the recap as the person's own message — the same door everything
    // they type goes through, so the turn it starts is theirs to undo. Not yet in
    // the log: a queued message is committed by the turn that claims it, which is
    // exactly the behaviour a command handing work to the model should have.
    assert!(
        agent
            .inbox()
            .waiting_from(atomcode_harness::agent::MessageOrigin::User),
        "the recap is waiting as the person's own message"
    );

    // Drive that turn, so the recap the model actually worked from is in the log.
    ctx.require::<atomcode_harness::seams::AgentLoopSvc>()
        .expect("the turn driver")
        .drive(&agent)
        .await;

    let recap = agent
        .session()
        .events()
        .into_iter()
        .filter_map(|logged| match logged.event {
            atomcode_kernel::session::SessionEvent::UserMessage { text, .. } => Some(text),
            _ => None,
        })
        .find(|text| text.contains("工作复盘") || text.contains("Work recap"))
        .expect("the recap is in the log as a user turn");
    assert!(
        recap.contains("port the quantizer to the NPU"),
        "the day's own work is in it: {recap}"
    );

    mounted.shutdown().await;
}

/// A runtime with no store keeps no sessions, so a day recap has nothing to read
/// — and saying "no work today" over a history it never looked at is worse than
/// not offering the command.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn a_runtime_that_keeps_no_sessions_does_not_offer_it() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let cfg = CodingAgentConfig::new("k", "http://localhost", "canned", project.path());

    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        ..quiet_options()
    };
    let mut mounted = mount(&cfg, opts, Arc::new(CannedProvider)).await;
    turn(&mut mounted.handle, "hello", allow()).await;

    let ctx = mounted.context();
    let agent = ctx
        .service::<AgentsSvc>()
        .expect("agents")
        .list()
        .into_iter()
        .find(|a| a.parent().is_none())
        .expect("the conversation's own agent");
    let offered: Vec<String> = ctx
        .service::<CommandsSvc>()
        .expect("the command catalog")
        .offered_for(&agent)
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(
        !offered.contains(&"worklog".to_string()),
        "no store, no recap: {offered:?}"
    );

    mounted.shutdown().await;
}
