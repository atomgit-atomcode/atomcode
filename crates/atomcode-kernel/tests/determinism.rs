//! Injecting a `FixedClock` makes the turn `elapsed_ms` sidecar deterministic (always 0),
//! so a run's snapshots are byte-reproducible for eval / replay — the kernel's only
//! non-deterministic value is time (ids are counters, cwd is pinnable, no randomness).

use atomcode_kernel::agent::Agent;
use atomcode_kernel::clock::FixedClock;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

#[tokio::test]
async fn fixed_clock_makes_elapsed_ms_deterministic() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .clock(Arc::new(FixedClock(12_345)))
        .build()
        .spawn();

    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut elapsed = None;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Usage(meta) => elapsed = Some(meta.elapsed_ms),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(elapsed, Some(0), "a FixedClock makes elapsed_ms reproducibly 0");
}
