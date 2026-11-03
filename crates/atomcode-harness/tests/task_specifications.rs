//! Explicit source selection, budget refusal and compaction integrity.
use atomcode_harness::session::SessionEvent;
use atomcode_harness::{agent::Agent, bundle, plugins};
use atomcode_kernel::session::retention;
use atomcode_plexus::{App, ConfigTree, Layer};
use std::sync::Arc;

async fn started() -> (App, Arc<Agent>) {
    let rows = Layer::from_toml(
        r#"
[[insert]]
name = "task-specifications"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [{ text = "ok" }] }
"#,
    )
    .unwrap();
    let mut app = App::new(
        plugins::catalog(),
        ConfigTree::from_layers([bundle::infra().unwrap(), rows]).unwrap(),
    );
    app.start().await.unwrap();
    let agent = atomcode_harness::create_agent(&app).await.unwrap();
    (app, agent)
}
#[tokio::test]
async fn explicit_selection_is_scoped_budgeted_and_recoverable_after_corruption() {
    let (_app, agent) = started().await;
    let log = agent.session();
    let text = "20 constraints\r\nMUST NOT rewrite this code\r\nlet exact = 42;";
    let seq = log.append(SessionEvent::UserMessage {
        turn: 1,
        text: text.into(),
        images: vec![],
    });
    let other = log.append(SessionEvent::UserMessage {
        turn: 2,
        text: "continue".into(),
        images: vec![],
    });
    let list = plugins::spec::execute(&agent, "list").unwrap();
    assert_eq!(list["items"][0]["seq"], other);
    plugins::spec::execute(&agent, &format!("pin {seq}")).unwrap();
    assert_eq!(
        retention::validate(&log.events())
            .unwrap()
            .get(&seq)
            .map(String::as_str),
        Some(text)
    );
    assert!(!retention::active_sources(&log.events()).contains_key(&other));
    let before = log.last_seq();
    let huge = log.append(SessionEvent::UserMessage {
        turn: 3,
        text: "x".repeat(70_000),
        images: vec![],
    });
    assert!(plugins::spec::execute(&agent, &format!("pin {huge}"))
        .unwrap_err()
        .contains("budget"));
    assert_eq!(log.last_seq(), before + 1, "refused pin appends no fact");
    assert!(plugins::spec::execute(&agent, "pin 999999").is_err());
    let mut events = log.events();
    if let SessionEvent::UserMessage { text, .. } =
        &mut events.iter_mut().find(|e| e.seq == seq).unwrap().event
    {
        text.push('!');
    }
    log.restore(events);
    assert!(retention::validate_request(&log.events(), &log.derive_messages()).is_err());
    plugins::spec::execute(&agent, &format!("unpin {seq}")).unwrap();
    assert!(retention::validate(&log.events()).unwrap().is_empty());
}

#[tokio::test]
async fn running_mutations_are_refused_and_corrupt_specifications_stop_the_real_loop() {
    use atomcode_harness::seams::AgentLoopSvc;
    let (app, agent) = started().await;
    let log = agent.session();
    let seq = log.append(SessionEvent::UserMessage {
        turn: 1,
        text: "exact specification".into(),
        images: vec![],
    });
    agent.begin_turn();
    let before = log.last_seq();
    assert!(plugins::spec::execute(&agent, &format!("pin {seq}"))
        .unwrap_err()
        .contains("running turn"));
    assert_eq!(log.last_seq(), before);
    agent.end_turn();
    plugins::spec::execute(&agent, &format!("pin {seq}")).unwrap();
    let mut events = log.events();
    if let SessionEvent::ContextRetention { sha256, .. } = &mut events.last_mut().unwrap().event {
        *sha256 = "broken".into();
    }
    log.restore(events);
    agent.send("continue");
    let result = app
        .context()
        .require::<AgentLoopSvc>()
        .unwrap()
        .drive(&agent)
        .await;
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("integrity")),
        "{result:?}"
    );
    assert!(
        !log.events()
            .iter()
            .any(|e| matches!(&e.event, SessionEvent::AssistantMessage { .. })),
        "provider output must not appear after a failed guard"
    );
}

#[tokio::test]
async fn synchronous_commit_listeners_can_cancel_an_idle_edit_without_reentering_its_lock() {
    use atomcode_harness::events::SessionEventCommitted;
    use atomcode_harness::session::Committed;
    let (_app, agent) = started().await;
    let seq = agent.session().append(SessionEvent::UserMessage {
        turn: 1,
        text: "spec".into(),
        images: vec![],
    });
    let weak = Arc::downgrade(&agent);
    let _listener = agent
        .ctx()
        .on_emit::<SessionEventCommitted>(move |fact: &Committed| {
            if matches!(fact.event, SessionEvent::ContextRetention { .. }) {
                if let Some(agent) = weak.upgrade() {
                    agent.cancel();
                }
            }
        });
    plugins::spec::execute(&agent, &format!("pin {seq}")).unwrap();
    assert_eq!(agent.status(), atomcode_kernel::agent::AgentStatus::Idle);
    assert!(
        !agent.cancelled(),
        "idle cancellation cannot poison the next turn"
    );
    assert_eq!(
        retention::active_sources(&agent.session().events()).len(),
        1
    );
}
