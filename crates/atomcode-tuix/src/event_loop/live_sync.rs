//! 同步模式：把 LiveSession 的 LiveEvent 映射成 TUI 既有的 AgentEvent，
//! 投进现有 runtime_event_tx，复用 handle_agent_event 渲染。

use std::sync::Arc;
use tokio::sync::mpsc;
use atomcode_core::agent::AgentEvent;
use atomcode_core::live::{LiveEvent, LiveSession};
use atomcode_core::turn::event::TurnEvent;
use super::bg_runtime::{RuntimeEvent, RuntimeId};

/// TurnEvent → 0/1 AgentEvent. UserMessage/StateChanged handled separately in the forwarder.
pub(crate) fn turn_to_agent_event(te: TurnEvent) -> Option<AgentEvent> {
    Some(match te {
        TurnEvent::TextDelta(s) => AgentEvent::TextDelta(s),
        TurnEvent::ReasoningDelta(s) => AgentEvent::ReasoningDelta(s),
        TurnEvent::ToolCallStarted { id, name, arguments } =>
            AgentEvent::ToolCallStarted { id, name, arguments },
        TurnEvent::ToolOutputChunk { call_id, chunk } =>
            AgentEvent::ToolOutputChunk { call_id, chunk },
        TurnEvent::ToolCallResult { call_id, name, output, success, duration } =>
            AgentEvent::ToolCallResult { call_id, name, output, success, duration },
        TurnEvent::TokenUsage { prompt_tokens, completion_tokens, cached_tokens, .. } =>
            AgentEvent::TokenUsage(atomcode_core::stream::TokenUsage {
                prompt_tokens,
                completion_tokens,
                cached_tokens,
            }),
        // TurnEvent::Error is a tuple variant Error(String)
        TurnEvent::Error(e) => AgentEvent::Error { error: e, messages: Vec::new() },
        TurnEvent::Warning(w) => AgentEvent::Warning(w),
        TurnEvent::ApprovalRequested { tool_name, reason, call, messages } =>
            AgentEvent::ApprovalNeeded { tool_name, reason, call, messages },
        // 不需要的：忽略
        TurnEvent::ToolCallStreaming { .. }
        | TurnEvent::ToolBatchStarted { .. }
        | TurnEvent::ToolBatchCompleted { .. }
        | TurnEvent::ContextStats { .. }
        | TurnEvent::WorkingDirChanged(_) => return None,
    })
}

/// 把 LiveSession 广播转发进 TUI 的 runtime_event_tx（渲染复用现有路径）。
/// 返回 JoinHandle 以便分离同步时 abort。
pub(crate) fn spawn_live_forwarder(
    session: Arc<LiveSession>,
    runtime_id: RuntimeId,
    fan_tx: mpsc::UnboundedSender<RuntimeEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (_snapshot, mut rx) = session.join().await;
        loop {
            match rx.recv().await {
                Ok(LiveEvent::Turn(te)) => {
                    if let Some(ae) = turn_to_agent_event(te) {
                        if fan_tx.send(RuntimeEvent { runtime_id, event: ae }).is_err() {
                            break;
                        }
                    }
                }
                Ok(LiveEvent::UserMessage { text, .. }) => {
                    if fan_tx
                        .send(RuntimeEvent { runtime_id, event: AgentEvent::UserEcho(text) })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(LiveEvent::StateChanged(st)) => {
                    let running = matches!(st, atomcode_core::live::TurnState::Running);
                    if fan_tx
                        .send(RuntimeEvent { runtime_id, event: AgentEvent::PeerBusy(running) })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(LiveEvent::ProviderChanged(provider)) => {
                    if fan_tx
                        .send(RuntimeEvent { runtime_id, event: AgentEvent::ProviderChanged(provider) })
                        .is_err()
                    {
                        break;
                    }
                }
                // webui /cd → follow it: TUI changes cwd + opens a fresh session.
                // Mapped to ProjectSwitched (not WorkingDirChanged) so the agent's
                // own in-turn `cd` never triggers a session reset.
                Ok(LiveEvent::WorkingDirChanged(dir)) => {
                    if fan_tx
                        .send(RuntimeEvent { runtime_id, event: AgentEvent::ProjectSwitched(dir) })
                        .is_err()
                    {
                        break;
                    }
                }
                // 手机 App 点开历史对话（/cd 带 session_id）→ 跟随：cd（如需）
                // 并恢复该会话（加载历史，而非开新会话）。
                Ok(LiveEvent::SessionSwitched { dir, session_id }) => {
                    if fan_tx
                        .send(RuntimeEvent {
                            runtime_id,
                            event: AgentEvent::SessionSwitched { dir, session_id },
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_text_delta() {
        assert!(
            matches!(
                turn_to_agent_event(TurnEvent::TextDelta("hi".into())),
                Some(AgentEvent::TextDelta(s)) if s == "hi"
            )
        );
    }

    #[test]
    fn ignores_context_stats() {
        assert!(turn_to_agent_event(TurnEvent::ContextStats {
            system_tokens: 0,
            sent_tokens: 0,
            dropped_tokens: 0,
            working_set_tokens: 0,
            total_messages: 0,
        })
        .is_none());
    }
}
