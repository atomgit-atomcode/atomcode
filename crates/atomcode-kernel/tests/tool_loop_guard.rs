//! Regression tests for the opt-in cross-round exact tool-loop guard.
//!
//! The guard is deliberately narrower than the `max_rounds` safety fuse: it only
//! terminates consecutive calls with the same final arguments, cwd, result, and
//! success state after the model has seen a warning and failed to course-correct.
//! Every proposed tool call still executes and is paired with a result before the
//! turn terminates, keeping provider history valid.

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AgentHandle, ToolLoopPolicy};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::{DeferredSteerProvider, RecordingProvider};
use atomcode_kernel::tool::{Tool, ToolCall, ToolContext, ToolRegistry, ToolResult};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OUTER_GUARD: Duration = Duration::from_secs(5);

/// A deterministic read-only tool. Its optional scripted outputs let a test vary
/// the observed result while keeping the model-emitted `(name, arguments)` exact.
struct ReadProbeTool {
    outputs: Mutex<VecDeque<String>>,
    fallback: String,
    read_only: bool,
    is_error: bool,
}

impl ReadProbeTool {
    fn stable(output: &str) -> Self {
        Self {
            outputs: Mutex::new(VecDeque::new()),
            fallback: output.into(),
            read_only: true,
            is_error: false,
        }
    }

    fn scripted(outputs: &[&str]) -> Self {
        Self {
            outputs: Mutex::new(outputs.iter().map(|s| (*s).to_string()).collect()),
            fallback: outputs.last().copied().unwrap_or_default().into(),
            read_only: true,
            is_error: false,
        }
    }

    fn mutating(output: &str) -> Self {
        Self {
            outputs: Mutex::new(VecDeque::new()),
            fallback: output.into(),
            read_only: false,
            is_error: false,
        }
    }

    fn failing_mutation(output: &str) -> Self {
        Self {
            outputs: Mutex::new(VecDeque::new()),
            fallback: output.into(),
            read_only: false,
            is_error: true,
        }
    }
}

#[async_trait]
impl Tool for ReadProbeTool {
    fn name(&self) -> &str {
        "read_probe"
    }

    fn description(&self) -> &str {
        "Returns deterministic read-only probe output"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } }
        })
    }

    fn read_only_hint(&self) -> bool {
        self.read_only
    }

    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        let content = self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone());
        ToolResult {
            call_id: String::new(),
            content,
            is_error: self.is_error,
            images: vec![],
        }
    }
}

fn tool_call(id: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read_probe".into(),
        arguments: args.into(),
    }
}

fn tool_round(id: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(tool_call(id, args)),
        StreamEvent::Done { truncated: false },
    ]
}

fn tool_batch_round(round: usize) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(tool_call(&format!("c{round}-a"), r#"{"path":"first"}"#)),
        StreamEvent::ToolCall(tool_call(&format!("c{round}-b"), r#"{"path":"second"}"#)),
        StreamEvent::Done { truncated: false },
    ]
}

fn stop_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::Done { truncated: false },
    ]
}

fn user(text: &str) -> AgentCommand {
    AgentCommand::SendMessage {
        text: text.into(),
        images: vec![],
    }
}

async fn drive_turn(handle: &mut AgentHandle, command: AgentCommand) -> Vec<AgentEvent> {
    handle.commands.send(command).unwrap();
    tokio::time::timeout(OUTER_GUARD, async {
        let mut events = Vec::new();
        while let Some(event) = handle.events.recv().await {
            let terminal = matches!(event, AgentEvent::TurnComplete { .. });
            events.push(event);
            if terminal {
                return events;
            }
        }
        panic!("agent event channel closed before TurnComplete");
    })
    .await
    .expect("turn must complete instead of hanging")
}

async fn shutdown(handle: AgentHandle) {
    handle.commands.send(AgentCommand::Shutdown).unwrap();
    handle.task.await.unwrap();
}

fn terminal_reason(events: &[AgentEvent]) -> Option<StopReason> {
    events.iter().find_map(|event| match event {
        AgentEvent::TurnComplete { reason } => Some(*reason),
        _ => None,
    })
}

fn warnings(events: &[AgentEvent]) -> Vec<(usize, &str)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            AgentEvent::Warning(message) => Some((index, message.as_str())),
            _ => None,
        })
        .collect()
}

fn started_ids(events: &[AgentEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolStarted { call } => Some(call.id.as_str()),
            _ => None,
        })
        .collect()
}

fn result_ids(events: &[AgentEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult { result } => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect()
}

fn mounted_probe(tool: ReadProbeTool) -> atomcode_kernel::tool::MountedTools {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(tool));
    registry.mount(&["read_probe"])
}

#[test]
fn policy_thresholds_are_validated_and_observable() {
    let policy = ToolLoopPolicy::new(10, 12).expect("ordered thresholds are valid");
    assert_eq!(policy.warning_threshold(), 10);
    assert_eq!(policy.stop_threshold(), 12);
    assert!(ToolLoopPolicy::new(1, 4).is_err());
    assert!(ToolLoopPolicy::new(4, 4).is_err());
    assert!(ToolLoopPolicy::new(5, 4).is_err());
}

#[tokio::test]
async fn intentional_exact_repetition_can_raise_the_policy_threshold() {
    let mut rounds = (1..=10)
        .map(|round| tool_round(&format!("c{round}"), r#"{"path":"same"}"#))
        .collect::<Vec<_>>();
    rounds.push(stop_round("completed ten intentional repetitions"));
    let provider = Arc::new(RecordingProvider::new(rounds));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::mutating(
            "same visible result",
        )))
        .tool_loop_policy(ToolLoopPolicy::new(10, 12).unwrap())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(
        &mut handle,
        user("perform this operation exactly ten times"),
    )
    .await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::Stopped));
    assert_eq!(calls.lock().unwrap().len(), 11);
    assert_eq!(started_ids(&events).len(), 10);
    assert_eq!(result_ids(&events).len(), 10);
    let diagnostics = warnings(&events);
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0].1.contains("10"),
        "a custom policy must report its real warning threshold"
    );
}

#[tokio::test]
async fn coarse_repeat_fuse_remains_on_when_exact_policy_is_disabled() {
    let mut rounds = (1..=7)
        .map(|round| tool_round(&format!("c{round}"), r#"{"path":"same"}"#))
        .collect::<Vec<_>>();
    rounds.push(stop_round("must never be requested"));
    let provider = Arc::new(RecordingProvider::new(rounds));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("repeat forever")).await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::RepeatLoop));
    assert_eq!(calls.lock().unwrap().len(), 6);
    assert_eq!(started_ids(&events).len(), 6);
    assert_eq!(result_ids(&events).len(), 6);
}

#[tokio::test]
async fn repeated_call_with_changing_results_hits_the_coarse_repeat_fuse() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_round("c1", r#"{"path":"same"}"#),
        tool_round("c2", r#"{"path":"same"}"#),
        tool_round("c3", r#"{"path":"same"}"#),
        tool_round("c4", r#"{"path":"same"}"#),
        tool_round("c5", r#"{"path":"same"}"#),
        tool_round("c6", r#"{"path":"same"}"#),
        stop_round("must never be requested"),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::scripted(&[
            "version-1",
            "version-2",
            "version-3",
            "version-4",
            "version-5",
            "version-6",
        ])))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("wait until the probe changes")).await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::RepeatLoop));
    assert_eq!(calls.lock().unwrap().len(), 6);
    assert_eq!(started_ids(&events).len(), 6);
    assert_eq!(result_ids(&events).len(), 6);
}

#[tokio::test]
async fn exact_stable_loop_warns_on_third_and_stops_after_fourth_without_fifth_request() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_round("c1", r#"{"path":"same"}"#),
        tool_round("c2", r#"{"path":"same"}"#),
        tool_round("c3", r#"{"path":"same"}"#),
        tool_round("c4", r#"{"path":"same"}"#),
        stop_round("must never be requested"),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("inspect repeatedly")).await;
    shutdown(handle).await;

    assert_eq!(
        terminal_reason(&events),
        Some(StopReason::ToolLoopDetected),
        "the fourth unchanged execution must end the turn as a detected tool loop"
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        4,
        "termination after the fourth result must prevent a fifth provider request"
    );

    let started = started_ids(&events);
    let results = result_ids(&events);
    assert_eq!(started, vec!["c1", "c2", "c3", "c4"]);
    assert_eq!(
        results, started,
        "all four executed calls must have exactly one matching ToolResult"
    );

    let loop_warnings = warnings(&events);
    assert_eq!(
        loop_warnings.len(),
        2,
        "the guard must emit one course-correction and one terminal diagnostic"
    );
    let results_before_warning = |warning_index| {
        events[..warning_index]
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolResult { .. }))
            .count()
    };
    assert_eq!(
        results_before_warning(loop_warnings[0].0),
        3,
        "the warning must be emitted only after the third real result is applied"
    );
    assert_eq!(
        results_before_warning(loop_warnings[1].0),
        4,
        "the terminal diagnostic must be emitted only after the fourth result is applied"
    );
}

#[tokio::test]
async fn exact_stable_read_only_batch_is_detected_without_a_coarse_signature_guard() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_batch_round(1),
        tool_batch_round(2),
        tool_batch_round(3),
        tool_batch_round(4),
        stop_round("must never be requested"),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("inspect both paths repeatedly")).await;
    shutdown(handle).await;

    assert_eq!(
        terminal_reason(&events),
        Some(StopReason::ToolLoopDetected),
        "an unchanged all-read-only batch must not bypass loop detection"
    );
    assert_eq!(calls.lock().unwrap().len(), 4);
    assert_eq!(started_ids(&events).len(), 8);
    assert_eq!(result_ids(&events).len(), 8);
}

#[tokio::test]
async fn repeated_mutating_call_with_identical_observation_is_detected() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_round("c1", r#"{"path":"same"}"#),
        tool_round("c2", r#"{"path":"same"}"#),
        tool_round("c3", r#"{"path":"same"}"#),
        tool_round("c4", r#"{"path":"same"}"#),
        stop_round("must never be requested"),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::mutating(
            "same visible result",
        )))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("perform the requested mutation")).await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::ToolLoopDetected));
    assert_eq!(calls.lock().unwrap().len(), 4);
    assert_eq!(warnings(&events).len(), 2);
}

#[tokio::test]
async fn repeated_failed_mutating_call_is_also_detected() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_round("c1", r#"{"command":"same"}"#),
        tool_round("c2", r#"{"command":"same"}"#),
        tool_round("c3", r#"{"command":"same"}"#),
        tool_round("c4", r#"{"command":"same"}"#),
        stop_round("must never be requested"),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::failing_mutation(
            "command exited with status 1",
        )))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("retry the failing command")).await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::ToolLoopDetected));
    assert_eq!(calls.lock().unwrap().len(), 4);
    assert_eq!(warnings(&events).len(), 2);
}

#[tokio::test]
async fn changed_observed_output_breaks_the_exact_stable_streak() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_round("c1", r#"{"path":"same"}"#),
        tool_round("c2", r#"{"path":"same"}"#),
        tool_round("c3", r#"{"path":"same"}"#),
        tool_round("c4", r#"{"path":"same"}"#),
        stop_round("done"),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::scripted(&[
            "version-1",
            "version-1",
            "version-2",
            "version-2",
        ])))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("wait for a change")).await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::Stopped));
    assert_eq!(calls.lock().unwrap().len(), 5);
    assert_eq!(started_ids(&events).len(), 4);
    assert_eq!(result_ids(&events).len(), 4);
    assert!(
        warnings(&events).is_empty(),
        "a changed model-visible result is progress, not an exact stable loop"
    );
}

#[tokio::test]
async fn real_user_steer_resets_the_consecutive_streak() {
    let deferred_commands = Arc::new(Mutex::new(None));
    let provider = Arc::new(DeferredSteerProvider::new(
        vec![
            tool_round("c1", r#"{"path":"same"}"#),
            tool_round("c2", r#"{"path":"same"}"#),
            tool_round("c3", r#"{"path":"same"}"#),
            tool_round("c4", r#"{"path":"same"}"#),
            stop_round("done after steer"),
        ],
        2,
        "the user changed the task",
        deferred_commands.clone(),
    ));
    let received = provider.received();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();
    *deferred_commands.lock().unwrap() = Some(handle.commands.clone());

    let events = drive_turn(&mut handle, user("initial task")).await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::Stopped));
    assert_eq!(received.lock().unwrap().len(), 5);
    assert_eq!(started_ids(&events).len(), 4);
    assert_eq!(result_ids(&events).len(), 4);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Steered { count: 1 })),
        "the injected real user prompt must have been folded in as steer"
    );
    assert!(
        warnings(&events).is_empty(),
        "real user intent arriving between the second and third requests resets the streak"
    );
}

#[tokio::test]
async fn synthetic_continuation_preserves_streak_across_turn_boundary() {
    let provider = Arc::new(RecordingProvider::new(vec![
        // Initial user turn establishes a streak of two and is cut by the round fuse.
        tool_round("c1", r#"{"path":"same"}"#),
        tool_round("c2", r#"{"path":"same"}"#),
        // Goal-mode synthetic continuation retains the streak: c3 warns, c4 stops.
        tool_round("c3", r#"{"path":"same"}"#),
        tool_round("c4", r#"{"path":"same"}"#),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(2)
        .build()
        .spawn();

    let first = drive_turn(&mut handle, user("start goal")).await;
    let second = drive_turn(
        &mut handle,
        AgentCommand::SendSyntheticMessage {
            text: "continue goal".into(),
        },
    )
    .await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&first), Some(StopReason::MaxRounds));
    assert_eq!(
        terminal_reason(&second),
        Some(StopReason::ToolLoopDetected),
        "synthetic goal continuation must not evade the existing loop streak"
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        4,
        "the synthetic turn must terminate after c4 without its fallback request"
    );
    assert_eq!(started_ids(&first), vec!["c1", "c2"]);
    assert_eq!(result_ids(&first), vec!["c1", "c2"]);
    assert_eq!(started_ids(&second), vec!["c3", "c4"]);
    assert_eq!(result_ids(&second), vec!["c3", "c4"]);

    let loop_warnings = warnings(&second);
    assert_eq!(loop_warnings.len(), 2);
    let results_before_warning = |warning_index| {
        second[..warning_index]
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolResult { .. }))
            .count()
    };
    assert_eq!(
        results_before_warning(loop_warnings[0].0),
        1,
        "c3 is the third execution in the preserved streak and must warn after its result"
    );
    assert_eq!(
        results_before_warning(loop_warnings[1].0),
        2,
        "c4 is the fourth execution in the preserved streak and must diagnose after its result"
    );
}

#[tokio::test]
async fn no_tool_reply_breaks_streak_before_synthetic_follow_up() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_round("c1", r#"{"path":"same"}"#),
        tool_round("c2", r#"{"path":"same"}"#),
        stop_round("finished this step without another tool"),
        tool_round("c3", r#"{"path":"same"}"#),
        tool_round("c4", r#"{"path":"same"}"#),
        stop_round("done"),
    ]));
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let first = drive_turn(&mut handle, user("start goal")).await;
    let second = drive_turn(
        &mut handle,
        AgentCommand::SendSyntheticMessage {
            text: "follow up after the completed step".into(),
        },
    )
    .await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&first), Some(StopReason::Stopped));
    assert_eq!(terminal_reason(&second), Some(StopReason::Stopped));
    assert_eq!(started_ids(&second), vec!["c3", "c4"]);
    assert!(
        warnings(&second).is_empty(),
        "a no-tool assistant reply breaks consecutiveness even when the host follows synthetically"
    );
}

#[tokio::test]
async fn varied_arguments_are_bounded_by_max_rounds_not_exact_loop_detection() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_round("c1", r#"{"path":"one"}"#),
        tool_round("c2", r#"{"path":"two"}"#),
        tool_round("c3", r#"{"path":"three"}"#),
        tool_round("c4", r#"{"path":"four"}"#),
        stop_round("must never be requested"),
    ]));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(4)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("keep changing arguments")).await;
    shutdown(handle).await;

    assert_eq!(
        terminal_reason(&events),
        Some(StopReason::MaxRounds),
        "argument drift must bypass the exact guard but remain bounded by max_rounds"
    );
    assert_eq!(calls.lock().unwrap().len(), 4);
    assert_eq!(started_ids(&events).len(), 4);
    assert_eq!(result_ids(&events).len(), 4);
    assert!(
        warnings(&events).is_empty(),
        "different arguments must not trigger the exact-loop warning"
    );
}

#[tokio::test]
async fn coarse_repeat_fuse_does_not_stop_distinct_arguments() {
    let mut rounds = (1..=7)
        .map(|round| {
            tool_round(
                &format!("c{round}"),
                &format!(r#"{{"path":"step-{round}"}}"#),
            )
        })
        .collect::<Vec<_>>();
    rounds.push(stop_round("done"));
    let provider = Arc::new(RecordingProvider::new(rounds));
    let calls = provider.calls();
    let mut handle = Agent::builder()
        .provider(provider)
        .tools(mounted_probe(ReadProbeTool::stable("unchanged")))
        .tool_loop_policy(ToolLoopPolicy::default())
        .max_rounds(20)
        .build()
        .spawn();

    let events = drive_turn(&mut handle, user("inspect seven distinct paths")).await;
    shutdown(handle).await;

    assert_eq!(terminal_reason(&events), Some(StopReason::Stopped));
    assert_eq!(calls.lock().unwrap().len(), 8);
    assert_eq!(started_ids(&events).len(), 7);
    assert_eq!(result_ids(&events).len(), 7);
}
