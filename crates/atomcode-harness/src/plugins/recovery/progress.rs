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
    bytes: usize,
}
// Images in the event log are inline, not paths to expiring temporary files.
// Reuse only payloads that retained their data and type after batch transforms.
fn retained_images(images: &[atomcode_kernel::message::ImageContent]) -> bool {
    images
        .iter()
        .all(|image| image.media_type.starts_with("image/") && !image.data.trim().is_empty())
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
        result: &ToolResult,
    ) {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = all.entry(session.id().to_owned()).or_default();
        if state.number != session.current_turn() {
            return;
        }
        let bytes = state.receipts.iter().map(|r| r.bytes).sum::<usize>();
        let result_bytes = result.content.len()
            + call.arguments.len()
            + result
                .images
                .iter()
                .map(|i| i.data.len() + i.media_type.len())
                .sum::<usize>();
        if state.overflow || state.receipts.len() >= 128 || bytes + result_bytes > 8 * 1024 * 1024 {
            state.overflow = true;
            return;
        }
        state.receipts.push(Receipt {
            tool,
            call,
            round,
            cwd,
            bytes: result_bytes,
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
                    if *turn == state.number && *r == round && call_id == &call.id && retained_images(images)))
        })
    }
    /// A restart must not bypass an uncertain earlier tool batch merely
    /// because the latest failed attempt contains only reasoning.
    pub(crate) fn restart_safe(&self, session: &SessionLog) -> bool {
        let events = session.events();
        let all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = all.get(session.id()) else {
            return false;
        };
        if state.number != session.current_turn() || state.overflow {
            return false;
        }
        events.iter().all(|e| match &e.event {
            SessionEvent::AssistantMessage { turn, round, tool_calls, .. }
                if *turn == state.number => tool_calls.iter().all(|call| {
                    state.receipts.iter().any(|r| r.round == *round && r.call.id == call.id)
                        && events.iter().any(|e| matches!(&e.event,
                            SessionEvent::ToolResultLogged { turn, round: r, call_id, is_error: false, images, .. }
                            if *turn == state.number && r == round && call_id == &call.id && retained_images(images)))
                }),
            _ => true,
        })
    }
    pub(crate) fn recover(&self, session: &SessionLog, max: u32, protect: bool) -> Option<u32> {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = all.entry(session.id().to_owned()).or_default();
        if state.number != session.current_turn() || state.used >= max {
            return None;
        }
        state.used += 1;
        state.recovering |= protect;
        Some(state.used)
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
        let (content, images) = session
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
                    && retained_images(&images) =>
                {
                    Some((content, images))
                }
                _ => None,
            })?;
        // Batch transforms may cap or redact AFTER execute_one. The committed
        // result is authoritative, never the pre-batch payload from execute.
        let content = format!(
            "[Recovery: confirmed result reused; tool not executed again]\n{}",
            content
        );
        Some(ToolResult {
            call_id: call.id.clone(),
            content,
            is_error: false,
            images,
        })
    }
}
