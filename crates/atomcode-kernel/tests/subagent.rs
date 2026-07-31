//! CLAIM 31: SUBAGENTS BY COMPOSITION — the kernel supports a parent agent
//! spawning a child agent for an isolated sub-task with NO new kernel concept,
//! using ONLY `Agent` + `Tool` + `run_to_completion` plus the two small builder
//! seams added for this spike:
//!
//!   * SEAM 1 `AgentBuilder::working_dir(PathBuf)` — pins the agent's tool
//!     `ToolContext::working_dir` per-agent instead of reading the process-global
//!     `current_dir()`, so a child can be dir-scoped independently of its parent.
//!   * SEAM 2 `AgentBuilder::cancel_token(CancellationToken)` — derives the agent's
//!     per-turn tokens as CHILDREN of an external cancel source, so a parent's
//!     cancel propagates into a DETACHED child session.
//!
//! A SUBAGENT here is purely an L2 PATTERN realized by `testkit::SubAgentTool`: a
//! `Tool` whose `execute` BUILDS and RUNS a child `Agent` via the one-shot adapter.
//! The kernel itself gained NO "subagent" type.
//!
//! Each test is wrapped in an OUTER `tokio::time::timeout` so a propagation FAILURE
//! FAILS the test (with a clear message) rather than hanging the suite forever.

use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{
    BlockUntilCancelTool, RecordingProvider, SubAgentTool, WorkingDirProbeTool,
};
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Outer guard: far longer than any deterministic work here, short enough that a
/// real propagation failure (a hang) fails the test promptly instead of stalling.
const OUTER_GUARD: Duration = Duration::from_secs(5);

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

fn send(text: &str) -> AgentCommand {
    AgentCommand::SendMessage {
        text: text.into(),
        images: vec![],
    }
}

// ── CLAIM 31a: COMPOSITION ───────────────────────────────────────────────────
//
// A parent mounts a `SubAgentTool`. The parent's provider returns a tool_call to
// "subagent" (round 1), then stops (round 2). The CHILD has its OWN scripted
// provider that produces text. We assert the parent received the CHILD's text as
// the tool RESULT — proving the child Agent actually ran and its Outcome flowed
// back up. Uses ONLY Agent + Tool + run_to_completion (inside SubAgentTool).
#[tokio::test]
async fn subagent_composition_parent_runs_child_and_gets_result() {
    // The child agent: a scripted provider that produces a distinctive text, no
    // tools, and stops in one round.
    let child_output = "CHILD-DID-THE-SUBTASK";
    let sub = SubAgentTool::new(
        "subagent",
        move || {
            Arc::new(RecordingProvider::new(vec![vec![
                StreamEvent::TextDelta(child_output.into()),
                StreamEvent::Done { truncated: false },
            ]])) as Arc<_>
        },
        || ToolRegistry::new().mount(&[] as &[&str]),
        "child persona",
    );

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(sub));

    // The PARENT: round 1 calls the subagent; round 2 (after the tool result) stops.
    let parent_provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call(
                "c_sub",
                "subagent",
                "{\"task\":\"do the thing\"}",
            )),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("parent wraps up".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut handle = Agent::builder()
        .provider(parent_provider)
        .tools(reg.mount(&["subagent"]))
        .build()
        .spawn();

    handle.commands.send(send("delegate the subtask")).unwrap();

    let child_result = tokio::time::timeout(OUTER_GUARD, async {
        let mut child_result: Option<String> = None;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                AgentEvent::ToolResult { result } => child_result = Some(result.content),
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        child_result
    })
    .await
    .expect("subagent composition must not hang");

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(
        child_result.as_deref(),
        Some(child_output),
        "the parent's tool result must equal the CHILD agent's output — proving the child \
         ran via run_to_completion and its Outcome flowed back as the tool result"
    );
}

// ── CLAIM 31b: CANCEL PROPAGATION INTO A DETACHED CHILD ──────────────────────
//
// The CHILD mounts a `BlockUntilCancelTool` (shared `observed` flag) and its
// provider calls it, so the child session parks inside that tool on the CHILD's
// own per-turn cancel token. The parent calls the `SubAgentTool` (so the child is
// running, detached). The test then sends `AgentCommand::Cancel` to the PARENT.
//
// We assert the child's tool observes cancellation (flag flips within the outer
// bound). This PROVES the parent's cancel propagated via `ctx.cancel.child_token()`
// into the DETACHED, `tokio::spawn`-ed child task and stopped it. It CANNOT be
// future-drop doing the work: `run_to_completion` spawns the child session as a
// detached task; dropping the parent's tool future does NOT abort that task — only
// the cancel TOKEN can reach in. The outer `timeout` makes a propagation failure
// FAIL (the child would block forever) rather than hang.
#[tokio::test]
async fn subagent_cancel_propagates_into_detached_child() {
    let observed = Arc::new(AtomicBool::new(false));
    let observed_for_factory = observed.clone();

    // Child: provider calls block_until_cancel; the tool mounts the shared flag.
    let sub = SubAgentTool::new(
        "subagent",
        || {
            Arc::new(RecordingProvider::new(vec![vec![
                StreamEvent::ToolCall(tool_call("c_block", "block_until_cancel", "{}")),
                StreamEvent::Done { truncated: false },
            ]])) as Arc<_>
        },
        move || {
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(BlockUntilCancelTool::new(
                observed_for_factory.clone(),
            )));
            reg.mount(&["block_until_cancel"])
        },
        "child persona",
    );

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(sub));

    // Parent: round 1 calls the subagent (which will block). The parent's turn stays
    // in-flight inside the subagent tool's execute until cancel propagates.
    let parent_provider = Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::ToolCall(tool_call("c_sub", "subagent", "{\"task\":\"long\"}")),
        StreamEvent::Done { truncated: false },
    ]]));

    let mut handle = Agent::builder()
        .provider(parent_provider)
        .tools(reg.mount(&["subagent"]))
        .build()
        .spawn();

    let commands = handle.commands.clone();
    handle
        .commands
        .send(send("delegate a long subtask"))
        .unwrap();

    let (observed_cancel, cancelled, completed) = tokio::time::timeout(OUTER_GUARD, async {
        let mut sent_cancel = false;
        let mut cancelled = false;
        let mut completed = false;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                // The PARENT's subagent tool has started executing → the child is now
                // running (detached) and its blocker is parked (or about to park) on
                // the child cancel token. Cancel the PARENT. `cancelled()` is
                // level-triggered, so even if the child reaches its await AFTER the
                // cancel fires, it returns immediately — race-free.
                AgentEvent::ToolStarted { call } if call.name == "subagent" => {
                    if !sent_cancel {
                        sent_cancel = true;
                        commands.send(AgentCommand::Cancel).unwrap();
                    }
                }
                AgentEvent::Cancelled => cancelled = true,
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }
        // Poll the shared flag (set by the child's tool) under the same outer bound.
        // A spawned task may flip it slightly after the parent's TurnComplete; wait
        // briefly. If propagation FAILED, the child blocks forever and the OUTER
        // timeout below trips → the test FAILS rather than hangs.
        while !observed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        (observed.load(Ordering::SeqCst), cancelled, completed)
    })
    .await
    .expect(
        "cancel must PROPAGATE into the detached child and stop it — a hang here means the \
         parent's cancel never reached the child (future-drop cannot kill a spawned task)",
    );

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert!(
        observed_cancel,
        "the child's BlockUntilCancelTool must observe ctx.cancel"
    );
    assert!(
        cancelled,
        "the parent's cancelled turn must emit AgentEvent::Cancelled"
    );
    assert!(
        completed,
        "the parent's cancelled turn must end with TurnComplete"
    );
}

// ── CLAIM 31c: WORKING_DIR ISOLATION ─────────────────────────────────────────
//
// The child is built with `.working_dir(child_dir)` (a distinct path; no fs access
// needed, just the value). The child mounts a `WorkingDirProbeTool` whose result is
// the dir it saw. We assert the parent received the CHILD dir as the tool result —
// proving SEAM 1 makes working_dir per-agent, not process-global.
#[tokio::test]
async fn subagent_working_dir_isolation() {
    let child_dir = std::path::PathBuf::from("/tmp/child-xyz");
    let child_dir_str = child_dir.display().to_string();

    // Child: provider calls working_dir_probe; the SubAgentTool pins the child dir.
    let sub = SubAgentTool::new(
        "subagent",
        || {
            Arc::new(RecordingProvider::new(vec![
                vec![
                    StreamEvent::ToolCall(tool_call("c_probe", "working_dir_probe", "{}")),
                    StreamEvent::Done { truncated: false },
                ],
                // Round 2: the child stops after the probe as a TRUE tool-only child —
                // no answer text AND no reasoning. It streams an EMPTY reasoning delta
                // purely to mark the provider as having responded (sets
                // `saw_stream_content`, so it is NOT an empty-200 that the child would
                // retry), while leaving BOTH the final assistant text and the reasoning
                // empty. That matters because the loop PROMOTES a reasoning-only final
                // round to the answer (recovering a gateway that misroutes the answer
                // into the reasoning channel); a NON-empty reasoning here would be
                // promoted to the child's text and SubAgentTool would return that instead
                // of falling back to the probe tool RESULT (the working_dir) this test
                // asserts. Empty reasoning ⇒ nothing to promote ⇒ the fallback stands.
                vec![
                    StreamEvent::Reasoning(String::new()),
                    StreamEvent::Done { truncated: false },
                ],
            ])) as Arc<_>
        },
        || {
            let mut reg = ToolRegistry::new();
            reg.register(Arc::new(WorkingDirProbeTool));
            reg.mount(&["working_dir_probe"])
        },
        "child persona",
    )
    .child_dir(child_dir.clone());

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(sub));

    // Parent: round 1 calls the subagent; round 2 stops.
    let parent_provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c_sub", "subagent", "{\"task\":\"probe\"}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut handle = Agent::builder()
        .provider(parent_provider)
        .tools(reg.mount(&["subagent"]))
        .build()
        .spawn();

    handle.commands.send(send("probe the child dir")).unwrap();

    let probe_result = tokio::time::timeout(OUTER_GUARD, async {
        let mut probe_result: Option<String> = None;
        while let Some(ev) = handle.events.recv().await {
            match ev {
                // The parent's tool result carries the child's Outcome.text, which is
                // the child probe tool's content = the child's working_dir.
                AgentEvent::ToolResult { result } => probe_result = Some(result.content),
                AgentEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }
        probe_result
    })
    .await
    .expect("working_dir isolation test must not hang");

    handle.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = handle.task.await;

    assert_eq!(
        probe_result.as_deref(),
        Some(child_dir_str.as_str()),
        "the child tool must report the CHILD working_dir ({child_dir_str}) — proving \
         working_dir is per-agent (SEAM 1), not process-global current_dir()"
    );
    // Sanity: the child dir is NOT the process cwd (so the assertion above is
    // meaningful — it would fail if working_dir were ignored and the process cwd
    // were used instead).
    let process_cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();
    assert_ne!(
        child_dir_str, process_cwd,
        "the test's child dir must differ from the process cwd for the isolation claim to bite"
    );
}
