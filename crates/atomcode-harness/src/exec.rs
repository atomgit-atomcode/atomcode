//! Running one tool call, as a free function.
//!
//! It lives outside the loop so a scheduling plugin can reach it. The loop's own
//! terminal calls it in a line; `tool-exec-parallel` calls it from inside a
//! `FuturesOrdered`; a remote executor would call it on the other side of a
//! socket. All three go through the same `tools/execute` waterfall, so approval,
//! permission rules, plan mode and the loop guard apply identically no matter
//! who scheduled the call.

use std::path::PathBuf;

use atomcode_kernel::tool::{ProgressSink, ToolContext, ToolResult};
use atomcode_plexus::Context;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::events::{ToolExec, ToolsExecute};
use crate::seams::ToolsSvc;

/// Run one call through the `tools/execute` waterfall.
///
/// A denial is a *result*, not an error: the model has to see why its call did
/// not run, and the history has to stay pairable.
pub async fn execute_one(
    agent_ctx: &Context,
    cancel: CancellationToken,
    working_dir: PathBuf,
    mut exec: ToolExec,
) -> ToolResult {
    let ctx = agent_ctx.clone();
    let session = crate::agent::scoped(agent_ctx).service::<crate::seams::SessionSvc>();
    let recovery = agent_ctx.service::<crate::plugins::recovery::Progress>();
    if cancel.is_cancelled() {
        return error_result(&exec.call.id, "(cancelled before it started)");
    }
    if let (Some(progress), Some(log), Some(tool)) = (
        &recovery,
        &session,
        agent_ctx
            .service::<ToolsSvc>()
            .and_then(|t| t.get(&exec.call.name)),
    ) {
        if let Some(receipt) = progress.reuse(log, &tool, &exec.call, &working_dir) {
            // No execution: the committed receipt already passed gates and transforms.
            return receipt;
        }
    }
    // Only the execution terminal can certify success. A policy's synthetic
    // result must never become proof that an operation actually ran.
    let proof = std::sync::Arc::new(std::sync::Mutex::new(None));
    let executed = proof.clone();
    let round = exec.round;
    let result = agent_ctx
        .waterfall::<ToolsExecute, _>(&mut exec, move |exec| {
            let ctx = ctx.clone();
            let executed = executed.clone();
            let cancel = cancel.clone();
            let working_dir = working_dir.clone();
            // The call as it stands AFTER the gates, arguments and all.
            let call = exec.call.clone();
            let (exec_turn, exec_round) = (exec.turn, exec.round);
            Box::pin(async move {
                let Some(toolbox) = ctx.service::<ToolsSvc>() else {
                    return error_result(&call.id, "no tool catalog is mounted");
                };
                let Some(tool) = toolbox.get(&call.name) else {
                    return error_result(
                        &call.id,
                        &unknown_tool_message(&call.name, &toolbox.names()),
                    );
                };
                if let (Some(progress), Some(log)) = (
                    ctx.service::<crate::plugins::recovery::Progress>(),
                    crate::agent::scoped(&ctx).service::<crate::seams::SessionSvc>(),
                ) {
                    if let Some(receipt) = progress.reuse(&log, &tool, &call, &working_dir) {
                        // A gate repaired the arguments into a completed mutation.
                        // Fail closed here; outer result hooks must not observe a
                        // fabricated second successful execution.
                        return error_result(
                            &call.id,
                            &format!(
                                "already completed during this turn; do not execute again. {}",
                                receipt.content
                            ),
                        );
                    }
                }
                // The agent's own token, not a fresh one: a long-running tool
                // that polls `ctx.cancel` has to actually see a stop, or a
                // cancel only takes effect after it finishes.
                //
                // Progress and questions reach the person through whoever drives
                // this agent, if anyone does.
                let session = crate::agent::scoped(&ctx).service::<crate::seams::SessionSvc>();
                let (progress, requester) =
                    match (ctx.service::<crate::seams::ToolDriverSvc>(), &session) {
                        (Some(driver), Some(log)) => (
                            driver.progress(log.id(), &call.id),
                            driver.requester(log.id()),
                        ),
                        _ => (ProgressSink::noop(), None),
                    };
                let tool_ctx = ToolContext {
                    working_dir: working_dir.clone(),
                    cancel,
                    progress,
                    requester,
                };
                // The one moment "this call is running" becomes true: every
                // waterfall listener delegated, so no gate refused it and no
                // scheduler held it back. Committed here rather than derived
                // from the assistant message, which is a fact about what the
                // model ASKED for and lands long before any of them decided.
                //
                // After the unknown-tool check above, so a tool nobody mounted
                // never announces a start it cannot have — the guard a driver
                // used to keep for itself.
                if let Some(session) = session {
                    crate::session::commit(
                        &ctx,
                        &session,
                        crate::session::SessionEvent::ToolStarted {
                            turn: exec_turn,
                            round: exec_round,
                            call: call.clone(),
                        },
                    );
                }
                let mut result = tool.execute(&call.arguments, &tool_ctx).await;
                // Tools mint their own id-less results; pairing is the caller's.
                result.call_id = call.id.clone();
                if !result.is_error && result.images.is_empty() {
                    *executed.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((tool, call, working_dir));
                }
                result
            }) as BoxFuture<'_, ToolResult>
        })
        .await;
    if !result.is_error && result.images.is_empty() {
        if let (Some(progress), Some(log), Some((tool, call, cwd))) = (
            recovery,
            session,
            proof.lock().unwrap_or_else(|e| e.into_inner()).take(),
        ) {
            progress.record(&log, round, tool, call, cwd, result.clone());
        }
    }
    result
}

/// Suggestions describe only the live catalog. They never repair or dispatch a call.
fn unknown_tool_message(name: &str, available: &[String]) -> String {
    let leaf = |name: &str| {
        name.rsplit("__")
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase()
    };
    let requested = leaf(name);
    let mut candidates: Vec<_> = available
        .iter()
        .filter_map(|candidate| {
            nearby_tool_name(&requested, &leaf(candidate)).map(|distance| (distance, candidate))
        })
        .collect();
    candidates.sort_unstable();
    candidates.dedup_by(|a, b| a.1 == b.1);
    let suggestions: Vec<_> = candidates
        .into_iter()
        .take(3)
        .map(|(_, candidate)| format!("`{candidate}`"))
        .collect();
    if suggestions.is_empty() {
        format!("unknown tool `{name}`. Use a name from the available tool definitions.")
    } else {
        format!(
            "unknown tool `{name}`. Related available tools: {}. Check their definitions before calling again.",
            suggestions.join(", ")
        )
    }
}

/// A small edit-distance window handles typos without suggesting unrelated tools.
/// Bound both names so malformed model output cannot cause quadratic unbounded work.
fn nearby_tool_name(requested: &str, candidate: &str) -> Option<usize> {
    let requested: Vec<_> = requested.chars().take(129).collect();
    let candidate: Vec<_> = candidate.chars().take(129).collect();
    if requested.is_empty() || requested.len() > 128 || candidate.len() > 128 {
        return None;
    }
    let limit = if requested.len() < 4 { 1 } else { 2 };
    if requested.len().abs_diff(candidate.len()) > limit {
        return None;
    }
    let mut previous: Vec<_> = (0..=candidate.len()).collect();
    let mut current = vec![0; candidate.len() + 1];
    for (i, left) in requested.iter().enumerate() {
        current[0] = i + 1;
        for (j, right) in candidate.iter().enumerate() {
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(left != right));
        }
        if current.iter().all(|&distance| distance > limit) {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[candidate.len()];
    (distance <= limit).then_some(distance)
}

pub fn error_result(call_id: &str, message: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: message.to_string(),
        is_error: true,
        images: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::unknown_tool_message;

    #[test]
    fn tool_suggestions_are_bounded_and_stable_across_catalog_order() {
        let mut names: Vec<String> = [
            "mcp__gamma__read_file",
            "write_file",
            "mcp__delta__read_file",
            "mcp__beta__read_file",
            "mcp__alpha__read_file",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let message = unknown_tool_message("read_flie", &names);
        names.reverse();
        assert_eq!(message, unknown_tool_message("read_flie", &names));
        assert_eq!(
            names.iter().filter(|name| message.contains(*name)).count(),
            3
        );
        assert!(message.contains("mcp__alpha__read_file"));
        assert!(!message.contains("write_file"));
    }

    #[test]
    fn tool_suggestions_preserve_live_names_and_handle_malformed_names() {
        let names = vec!["read_file".to_string()];
        assert!(unknown_tool_message("READ_FILE", &names).contains("`read_file`"));
        for requested in ["".to_string(), "读取文件".repeat(100), "build".to_string()] {
            let message = unknown_tool_message(&requested, &names);
            assert!(message.contains("unknown tool"));
            assert!(!message.contains("`read_file`"));
        }
    }
}
