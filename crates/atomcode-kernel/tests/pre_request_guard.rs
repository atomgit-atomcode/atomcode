//! pre_request is documented APPEND-ONLY: it may add ephemeral TAIL reminders but must
//! not mutate / insert / delete WITHIN the stored history, or this round's outgoing wire
//! prefix diverges from prior rounds and the provider's prefix cache misses. The hook
//! runs on a per-request clone (storage is always safe), so a non-append projection is
//! invisible to history-byte tests for a THIRD-PARTY hook — the kernel surfaces it at
//! runtime as a Warning instead.

use async_trait::async_trait;
use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{MockProvider, TailReminderHook};
use atomcode_kernel::tool::ToolRegistry;
use std::sync::Arc;

/// Rewrites the FIRST message every round — a PREFIX mutation that poisons the wire cache.
struct PrefixMutatingHook;
#[async_trait]
impl LifecycleHooks for PrefixMutatingHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        if let Some(first) = messages.first_mut() {
            first.text.push_str(" [rebuilt]");
        }
    }
}

async fn warnings_for(hook: Arc<dyn LifecycleHooks>) -> Vec<String> {
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::Done { truncated: false },
    ]]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(ToolRegistry::new().mount(&[]))
        .hooks(hook)
        .build()
        .spawn();
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();
    let mut warnings = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Warning(w) => warnings.push(w),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;
    warnings
}

#[tokio::test]
async fn non_append_pre_request_emits_cache_prefix_warning() {
    let warnings = warnings_for(Arc::new(PrefixMutatingHook)).await;
    assert!(
        warnings.iter().any(|w| w.contains("not append-only")),
        "a prefix-mutating pre_request must emit a cache-prefix Warning; got {warnings:?}"
    );
}

#[tokio::test]
async fn append_only_pre_request_emits_no_warning() {
    // A tail reminder (the canonical append-only projection) must NOT warn.
    let warnings = warnings_for(Arc::new(TailReminderHook::new("tail"))).await;
    assert!(
        !warnings.iter().any(|w| w.contains("not append-only")),
        "an append-only pre_request (tail reminder) must NOT warn; got {warnings:?}"
    );
}
