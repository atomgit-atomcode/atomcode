//! `POST /command`: 无状态斜杠命令执行器（对已持久化会话/记忆施加一次性变更）。
use axum::{response::IntoResponse, Json};
use std::path::Path;

use atomcode_core::conversation::{Conversation, ConversationSnapshot};
use atomcode_core::session::{Session, SessionId, SessionManager};

/// 撤销会话最后若干轮（arg 空 = 最后一轮；否则回退到第 arg 个用户提示之前——对齐 TUI /undo）。
/// 就地修改 session.messages / cold_summaries，返回被移除的提示数。纯内存，无磁盘/env 依赖。
pub(crate) fn apply_undo(session: &mut Session, arg: &str) -> usize {
    let snapshot = ConversationSnapshot {
        messages: std::mem::take(&mut session.messages),
        cold_summaries: session.cold_summaries.clone(),
    };
    let mut conv = Conversation::from_snapshot(snapshot);
    let available = conv.prompt_count();
    if available == 0 {
        let s = conv.snapshot();
        session.messages = s.messages;
        return 0;
    }
    let target = arg.trim().parse::<usize>().ok().unwrap_or(available);
    let before = conv.prompt_count();
    conv.undo_to_prompt(target);
    let after = conv.prompt_count();
    let s = conv.snapshot();
    session.messages = s.messages;
    session.cold_summaries = s.cold_summaries;
    before.saturating_sub(after)
}

#[derive(serde::Deserialize)]
pub(crate) struct CommandReq {
    pub command: String,
    #[serde(default)]
    pub arg: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CommandResult {
    Undo { undone: usize },
    Remember { scope: String },
    Forget { removed: Vec<String> },
    Memory { global: Vec<String>, project: Vec<String> },
    Error { message: String },
}

fn exec_undo(working_dir: &Path, session_id: Option<&str>, arg: &str) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for undo"))?;
    let session_id = SessionId::from_string(sid.to_string());
    let manager = SessionManager::new(working_dir);
    let mut session = manager.load(&session_id)?;
    let undone = apply_undo(&mut session, arg);
    if undone > 0 {
        session.touch();
        manager.save(&session)?;
    }
    Ok(CommandResult::Undo { undone })
}

pub(crate) async fn run_command(Json(req): Json<CommandReq>) -> impl IntoResponse {
    let working_dir = match req.working_dir.as_deref() {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => {
            return Json(CommandResult::Error { message: "working_dir required".into() });
        }
    };
    let result = match req.command.as_str() {
        "undo" => exec_undo(&working_dir, req.session_id.as_deref(), &req.arg),
        other => Err(anyhow::anyhow!("unknown command: {other}")),
    };
    match result {
        Ok(r) => Json(r),
        Err(e) => Json(CommandResult::Error { message: e.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::conversation::message::{Message, Role};

    fn session_with_turns(n: usize) -> Session {
        let mut s = Session::new(std::path::PathBuf::from("/tmp/plan2-test"));
        for i in 0..n {
            s.messages.push(Message::new(Role::User, &format!("q{i}")));
            s.messages.push(Message::new(Role::Assistant, &format!("a{i}")));
        }
        s
    }

    #[test]
    fn undo_no_arg_removes_last_turn() {
        let mut s = session_with_turns(3);
        let removed = apply_undo(&mut s, "");
        assert_eq!(removed, 1);
        // 3 用户提示 → 剩 2；每轮 user+assistant，剩 2 轮 = 4 条消息。
        let users = s.messages.iter().filter(|m| matches!(m.role, Role::User)).count();
        assert_eq!(users, 2);
    }

    #[test]
    fn undo_to_prompt_1_removes_all() {
        let mut s = session_with_turns(3);
        let removed = apply_undo(&mut s, "1");
        assert_eq!(removed, 3);
        assert!(s.messages.is_empty());
    }

    #[test]
    fn undo_on_empty_session_is_noop() {
        let mut s = session_with_turns(0);
        assert_eq!(apply_undo(&mut s, ""), 0);
        assert!(s.messages.is_empty());
    }
}
