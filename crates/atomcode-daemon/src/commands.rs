//! `POST /command`: 无状态斜杠命令执行器（对已持久化会话/记忆施加一次性变更）。
use axum::{response::IntoResponse, Json};
use std::path::Path;

use atomcode_core::config::memory::MemoryStore;
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

/// 解析 `/remember` 参数：可选前缀 `--global`。返回 (是否全局, 去掉前缀并 trim 后的内容)。
pub(crate) fn parse_remember_arg(arg: &str) -> (bool, &str) {
    let arg = arg.trim();
    if let Some(rest) = arg.strip_prefix("--global") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return (true, rest.trim());
        }
    }
    (false, arg)
}

fn exec_remember(working_dir: &Path, arg: &str) -> anyhow::Result<CommandResult> {
    let (global, content) = parse_remember_arg(arg);
    if content.is_empty() {
        anyhow::bail!("remember needs content");
    }
    let store = if global { MemoryStore::global() } else { MemoryStore::project(working_dir) };
    store.append(content)?;
    Ok(CommandResult::Remember {
        scope: if global { "global" } else { "project" }.to_string(),
    })
}

fn exec_forget(working_dir: &Path, arg: &str) -> anyhow::Result<CommandResult> {
    let keyword = arg.trim();
    if keyword.is_empty() {
        anyhow::bail!("forget needs a keyword");
    }
    let mut removed = MemoryStore::global().remove_matching(keyword)?;
    removed.extend(MemoryStore::project(working_dir).remove_matching(keyword)?);
    Ok(CommandResult::Forget { removed })
}

fn exec_memory(working_dir: &Path) -> anyhow::Result<CommandResult> {
    Ok(CommandResult::Memory {
        global: MemoryStore::global().load(),
        project: MemoryStore::project(working_dir).load(),
    })
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
        "remember" => exec_remember(&working_dir, &req.arg),
        "forget" => exec_forget(&working_dir, &req.arg),
        "memory" => exec_memory(&working_dir),
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
    use atomcode_core::config::memory::MemoryStore;
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

    #[test]
    fn parse_remember_arg_detects_global() {
        assert_eq!(parse_remember_arg("--global 记住这个"), (true, "记住这个"));
        assert_eq!(parse_remember_arg("普通事实"), (false, "普通事实"));
        assert_eq!(parse_remember_arg("  --global   trimmed  "), (true, "trimmed"));
        assert_eq!(parse_remember_arg("--globalfoo"), (false, "--globalfoo"));
    }

    #[test]
    fn remember_then_memory_roundtrip_project_scope() {
        // hermetic：project 作用域写到 working_dir/.atomcode/memory.md
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        exec_remember(wd, "阿童木用 Rust 写").unwrap();
        let store = MemoryStore::project(wd);
        assert!(store.load().iter().any(|e| e.contains("阿童木用 Rust 写")));
    }

    #[test]
    fn forget_removes_project_entry() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        exec_remember(wd, "delete-me fact").unwrap();
        exec_remember(wd, "keep-me fact").unwrap();
        // exec_forget 也会扫全局，但全局此刻应无匹配；断言项目侧被删。
        let _ = exec_forget(wd, "delete-me");
        let remaining = MemoryStore::project(wd).load();
        assert!(!remaining.iter().any(|e| e.contains("delete-me")));
        assert!(remaining.iter().any(|e| e.contains("keep-me")));
    }
}
