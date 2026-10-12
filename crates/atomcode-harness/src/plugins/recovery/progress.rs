//! Ephemeral evidence for the existing stream-recovery row. The session log
//! decides whether a batch committed; receipts only prove which tool ran.
use crate::session::{SessionEvent, SessionLog};
use atomcode_kernel::tool::{Tool, ToolCall, ToolResult};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct Progress(Mutex<BTreeMap<String, Turn>>);
impl atomcode_plexus::ServiceKey for Progress {
    const NAME: &'static str = "stream-recovery-progress";
    const MODE: atomcode_plexus::SeamMode = atomcode_plexus::SeamMode::Core;
    const TITLE: &'static str = "Ephemeral confirmed progress for stream recovery";
    type Face = Self;
}
#[derive(Default)]
struct Turn {
    number: u64,
    used: u32,
    recovering: bool,
    overflow: bool,
    receipts: Vec<Receipt>,
}
struct Receipt {
    tool: Arc<dyn Tool>,
    call: ToolCall,
    round: u32,
    cwd: std::path::PathBuf,
    result: ToolResult,
}
fn arguments(args: &str) -> String {
    // serde_json's default map is ordered, including nested object keys.
    serde_json::from_str::<serde_json::Value>(args)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| args.to_owned())
}
impl Progress {
    pub(crate) fn reset(&self, id: &str, number: u64, end: bool) {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if end {
            all.remove(id);
            return;
        }
        let state = all.entry(id.to_owned()).or_default();
        if state.number != number {
            *state = Turn {
                number,
                ..Default::default()
            };
        }
        // A real user message changes intent, but never restores the retry budget.
        state.receipts.clear();
        state.recovering = false;
        state.overflow = false;
    }
    pub(crate) fn record(
        &self,
        session: &SessionLog,
        round: u32,
        tool: Arc<dyn Tool>,
        call: ToolCall,
        cwd: std::path::PathBuf,
        result: ToolResult,
    ) {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = all.entry(session.id().to_owned()).or_default();
        if state.number != session.current_turn() {
            return;
        }
        let bytes = state
            .receipts
            .iter()
            .map(|r| r.result.content.len() + r.call.arguments.len())
            .sum::<usize>();
        if state.overflow
            || state.receipts.len() >= 128
            || bytes + result.content.len() + call.arguments.len() > 8 * 1024 * 1024
        {
            state.overflow = true;
            return;
        }
        state.receipts.push(Receipt {
            tool,
            call,
            round,
            cwd,
            result,
        });
    }
    pub(crate) fn confirmed(&self, session: &SessionLog) -> bool {
        let events = session.events();
        let Some((round, calls)) = events.iter().rev().find_map(|e| match &e.event {
            SessionEvent::AssistantMessage {
                turn,
                round,
                tool_calls,
                ..
            } if *turn == session.current_turn() => Some((*round, tool_calls)),
            _ => None,
        }) else {
            return false;
        };
        if calls.is_empty() {
            return false;
        }
        let all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = all.get(session.id()) else {
            return false;
        };
        if state.number != session.current_turn() || state.overflow {
            return false;
        }
        calls.iter().all(|call| {
            state.receipts.iter().any(|r| r.round == round && r.call.id == call.id)
                && events.iter().any(|e| matches!(&e.event, SessionEvent::ToolResultLogged { turn, round: r, call_id, is_error: false, images, .. }
                    if *turn == state.number && *r == round && call_id == &call.id && images.is_empty()))
        })
    }
    pub(crate) fn recover(&self, session: &SessionLog, max: u32, protect: bool) -> bool {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = all.entry(session.id().to_owned()).or_default();
        if state.number != session.current_turn() || state.used >= max {
            return false;
        }
        state.used += 1;
        state.recovering = protect;
        true
    }
    pub(crate) fn reuse(
        &self,
        session: &SessionLog,
        tool: &Arc<dyn Tool>,
        call: &ToolCall,
        cwd: &Path,
    ) -> Option<ToolResult> {
        if tool.parallel_safe(&call.arguments) {
            return None;
        }
        let all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = all.get(session.id())?;
        if state.number != session.current_turn() || !state.recovering {
            return None;
        }
        let receipt = state.receipts.iter().rev().find(|r| {
            Arc::ptr_eq(&r.tool, tool)
                && r.cwd == cwd
                && arguments(&r.call.arguments) == arguments(&call.arguments)
        })?;
        // A raw successful execute is insufficient: the result must have committed.
        let content = session
            .events()
            .into_iter()
            .rev()
            .find_map(|e| match e.event {
                SessionEvent::ToolResultLogged {
                    turn,
                    round,
                    call_id,
                    content,
                    is_error: false,
                    images,
                } if turn == state.number
                    && round == receipt.round
                    && call_id == receipt.call.id
                    && images.is_empty() =>
                {
                    Some(content)
                }
                _ => None,
            })?;
        let mut result = receipt.result.clone();
        result.call_id = call.id.clone();
        // Batch transforms may cap or redact AFTER execute_one. The committed
        // result is authoritative, never the pre-batch payload in the receipt.
        result.content = format!(
            "[Recovery: confirmed result reused; tool not executed again]\n{}",
            content
        );
        Some(result)
    }
}
