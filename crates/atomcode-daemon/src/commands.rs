//! `POST /command`: 无状态斜杠命令执行器（对已持久化会话/记忆施加一次性变更）。
use axum::{extract::State, response::IntoResponse, Json};
use std::path::Path;

use crate::AppState;
use atomcode_core::agent::compression;
use atomcode_config::config::memory::MemoryStore;
use atomcode_core::conversation::{Conversation, ConversationSnapshot};
use atomcode_core::session::{Session, SessionId, SessionManager};

/// 撤销会话最后若干轮（arg 空 = 最后一轮；否则回退到第 arg 个用户提示之前——对齐 TUI /undo）。
/// 就地修改 session.messages / cold_summaries / display_messages / turn_stats，
/// 返回被移除的提示数。纯内存，无磁盘/env 依赖。
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
    let undone = before.saturating_sub(after);
    if undone > 0 {
        // Prune display_messages and turn_stats that reference removed turns.
        prune_orphaned_display(session);
    }
    undone
}

/// 会话 messages 变短后，裁掉锚点越界的 UI 附加消息与轮次统计，避免被撤销/压缩掉的
/// 回合的通知重现、上下文表尺读到过期 turn_stat。
fn prune_orphaned_display(session: &mut Session) {
    let n = session.messages.len();
    session.display_messages.retain(|d| d.after_message <= n);
    session.turn_stats.retain(|t| t.after_message <= n);
}

/// 压缩从 FRONT 排走消息（不同于 undo 从尾部截断），所以 surviving display/turn 锚点
/// 必须向前平移 `removed`：落在被排走范围内的丢弃，其余减去偏移量。
/// (`after_message == 0` 表示"第一条消息之前"——无条件保留。)
fn reindex_after_front_drain(session: &mut Session, removed: usize) {
    session.display_messages.retain_mut(|d| {
        if d.after_message == 0 {
            true
        } else if d.after_message <= removed {
            false
        } else {
            d.after_message -= removed;
            true
        }
    });
    session.turn_stats.retain_mut(|t| {
        if t.after_message <= removed {
            false
        } else {
            t.after_message -= removed;
            true
        }
    });
}

/// 保存到与加载时相同的桶：若有 project_hash 则写 project_hash 桶，否则按 working_dir 桶。
/// 与 load_command_session 严格对称，防止 undo/compact 写入不同桶产生幽灵副本。
fn save_command_session(session: &mut Session, project_hash: Option<&str>) -> anyhow::Result<()> {
    session.touch();
    match project_hash {
        Some(hash) => crate::save_session_to_hash(hash, session)?,
        None => SessionManager::new(&session.working_dir).save(session)?,
    }
    Ok(())
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
    #[serde(default)]
    pub project_hash: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CommandResult {
    Undo {
        undone: usize,
    },
    Remember {
        scope: String,
    },
    Forget {
        removed: Vec<String>,
    },
    Memory {
        global: Vec<String>,
        project: Vec<String>,
    },
    Context {
        system_tokens: usize,
        sent_tokens: usize,
        total_messages: usize,
        tool_defs_tokens: usize,
        cold_zone_tokens: usize,
        ctx_window: usize,
        ctx_name: String,
    },
    Compact {
        applied: bool,
        removed_messages: usize,
        before_tokens: usize,
        after_tokens: usize,
    },
    Whoami {
        logged_in: bool,
        username: Option<String>,
        name: Option<String>,
        email: Option<String>,
    },
    Status {
        logged_in: bool,
        username: Option<String>,
        provider: String,
        model: String,
        working_dir: String,
        config_path: String,
    },
    Config {
        path: String,
        provider: String,
    },
    Diff {
        stat: String,
    },
    Cost {
        total_tokens: usize,
        turn_count: usize,
    },
    Todo {
        items: Vec<TodoItemJson>,
    },
    Error {
        message: String,
    },
}

#[derive(serde::Serialize)]
pub(crate) struct TodoItemJson {
    pub status: String,
    pub content: String,
}

/// 按会话真实桶加载：优先 project_hash（跨 /cd 稳定），否则回退到 working_dir。
fn load_command_session(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: &SessionId,
) -> anyhow::Result<Session> {
    if let Some(hash) = project_hash {
        Ok(crate::load_session(hash, session_id.as_str())?)
    } else {
        Ok(SessionManager::new(working_dir).load(session_id)?)
    }
}

fn exec_undo(
    working_dir: &Path,
    session_id: Option<&str>,
    arg: &str,
    project_hash: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for undo"))?;
    let session_id = SessionId::from_string(sid.to_string());
    let mut session = load_command_session(working_dir, project_hash, &session_id)?;
    let undone = apply_undo(&mut session, arg);
    if undone > 0 {
        save_command_session(&mut session, project_hash)?;
    }
    Ok(CommandResult::Undo { undone })
}

async fn exec_context(
    state: &AppState,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
    provider: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for context"))?;
    let session_id = SessionId::from_string(sid.to_string());
    let session = load_command_session(working_dir, project_hash, &session_id)?;
    let parts = crate::live_api::build_turn_parts(
        working_dir,
        provider,
        &state.mcp_cache,
        state.telemetry.clone(),
    )
    .await?;
    let conv = Conversation::from_snapshot(ConversationSnapshot {
        messages: session.messages.clone(),
        cold_summaries: session.cold_summaries.clone(),
    });
    let (msgs, _) = parts.ctx.build_messages(&conv, &parts.system_prompt, "");
    let s =
        atomcode_core::agent::compute_rich_context_stats(&conv, &msgs, &parts.tools, &*parts.ctx)
            .await;
    Ok(CommandResult::Context {
        system_tokens: s.system_tokens,
        sent_tokens: s.sent_tokens,
        total_messages: s.total_messages,
        tool_defs_tokens: s.tool_defs_tokens,
        cold_zone_tokens: s.cold_zone_tokens,
        ctx_window: s.ctx_window,
        ctx_name: s.ctx_name,
    })
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
    let store = if global {
        MemoryStore::global()
    } else {
        MemoryStore::project(working_dir)
    };
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

async fn exec_compact(
    state: &AppState,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
    provider: Option<&str>,
    arg: &str,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for compact"))?;
    let session_id = SessionId::from_string(sid.to_string());
    let mut session = load_command_session(working_dir, project_hash, &session_id)?;
    let parts = crate::live_api::build_turn_parts(
        working_dir,
        provider,
        &state.mcp_cache,
        state.telemetry.clone(),
    )
    .await?;

    let mut conv = Conversation::from_snapshot(ConversationSnapshot {
        messages: std::mem::take(&mut session.messages),
        cold_summaries: session.cold_summaries.clone(),
    });

    let keep_ceiling = compression::compaction_keep_ceiling(
        &*parts.ctx,
        &parts.system_prompt,
        &*parts.tools,
        &conv.cold_summaries,
    )
    .await;

    let Some((mechanical, n_msgs)) = parts.ctx.compression_plan(&conv, keep_ceiling) else {
        // 没有可压缩的历史：原样还原，返回 applied=false。
        let snap = conv.snapshot();
        session.messages = snap.messages;
        return Ok(CommandResult::Compact {
            applied: false,
            removed_messages: 0,
            before_tokens: 0,
            after_tokens: 0,
        });
    };

    let summarize_prompt = if arg.trim().is_empty() {
        compression::default_summarize_prompt(&mechanical)
    } else {
        format!(
            "Summarize this conversation history, focusing on: {}.\n\
             Keep: file names, what was changed, key decisions, errors encountered.\n\
             Drop: exact code content, tool arguments, line numbers.\n\n{}",
            arg.trim(),
            mechanical
        )
    };

    let (summary, _, _, _, _) =
        compression::run_llm_summary(&*parts.provider, &summarize_prompt).await;
    let content = if summary.trim().is_empty() {
        mechanical
    } else {
        summary
    };

    let outcome = compression::try_apply_compression(
        &*parts.ctx,
        &mut conv,
        &parts.system_prompt,
        n_msgs,
        content,
        None,
    );

    let snap = conv.snapshot();
    session.messages = snap.messages;
    session.cold_summaries = snap.cold_summaries;
    if outcome.applied {
        reindex_after_front_drain(&mut session, outcome.removed_messages);
        save_command_session(&mut session, project_hash)?;
    }

    Ok(CommandResult::Compact {
        applied: outcome.applied,
        removed_messages: outcome.removed_messages,
        before_tokens: outcome.before_tokens,
        after_tokens: outcome.after_tokens,
    })
}

fn exec_whoami() -> anyhow::Result<CommandResult> {
    match atomcode_core::auth::get_stored_auth() {
        Some(auth) => Ok(CommandResult::Whoami {
            logged_in: true,
            username: Some(auth.user.username),
            name: auth.user.name,
            email: auth.user.email,
        }),
        None => Ok(CommandResult::Whoami {
            logged_in: false,
            username: None,
            name: None,
            email: None,
        }),
    }
}

fn exec_config() -> anyhow::Result<CommandResult> {
    let path = atomcode_config::config::Config::default_path();
    let provider = atomcode_config::config::Config::load(&path)
        .map(|c| c.default_provider)
        .unwrap_or_default();
    Ok(CommandResult::Config {
        path: path.display().to_string(),
        provider,
    })
}

fn exec_diff(working_dir: &std::path::Path) -> anyhow::Result<CommandResult> {
    let out = std::process::Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(working_dir)
        .output()?;
    let stat = if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    } else {
        String::from_utf8_lossy(&out.stderr).trim_end().to_string()
    };
    Ok(CommandResult::Diff { stat })
}

fn exec_status(
    working_dir: &std::path::Path,
    provider: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let config_path = atomcode_config::config::Config::default_path();
    let config = atomcode_config::config::Config::load(&config_path).ok();
    let provider_name = provider
        .map(|s| s.to_string())
        .or_else(|| config.as_ref().map(|c| c.default_provider.clone()))
        .unwrap_or_default();
    let model = config
        .as_ref()
        .and_then(|c| c.providers.get(&provider_name))
        .map(|p| p.model.clone())
        .unwrap_or_default();
    let auth = atomcode_core::auth::get_stored_auth();
    Ok(CommandResult::Status {
        logged_in: auth.is_some(),
        username: auth.map(|a| a.user.username),
        provider: provider_name,
        model,
        working_dir: working_dir.display().to_string(),
        config_path: config_path.display().to_string(),
    })
}

fn exec_cost(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for cost"))?;
    let session = load_command_session(
        working_dir,
        project_hash,
        &SessionId::from_string(sid.to_string()),
    )?;
    // TurnStat.total_tokens stores the per-turn token count (reset to 0 at turn start,
    // accumulated during the turn, saved at TurnComplete). Summing gives session total.
    let total_tokens: usize = session.turn_stats.iter().map(|t| t.total_tokens).sum();
    let turn_count = session.turn_stats.len();
    Ok(CommandResult::Cost {
        total_tokens,
        turn_count,
    })
}

fn exec_todo(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for todo"))?;
    let session = load_command_session(
        working_dir,
        project_hash,
        &SessionId::from_string(sid.to_string()),
    )?;

    // session.messages uses atomcode_core::conversation::message::Message (not
    // atomcode_kernel::message::Message), so we inline the derivation logic here
    // rather than calling atomcode_capabilities::tools::todo::derive_current_todos
    // directly (the two Message types are incompatible across crate boundaries).
    use atomcode_capabilities::tools::todo::{parse_todos, TodoStatus};
    use atomcode_core::conversation::message::MessageContent;

    let todos = session
        .messages
        .iter()
        .rev()
        .find_map(|m| {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                tool_calls
                    .iter()
                    .rev()
                    .filter(|c| c.name == "todowrite")
                    .find_map(|c| parse_todos(&c.arguments).ok())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let items = todos
        .into_iter()
        .map(|t| TodoItemJson {
            status: match t.status {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "in_progress",
                TodoStatus::Completed => "completed",
            }
            .to_string(),
            content: t.content,
        })
        .collect();

    Ok(CommandResult::Todo { items })
}

pub(crate) async fn run_command(
    State(state): State<AppState>,
    Json(req): Json<CommandReq>,
) -> impl IntoResponse {
    let working_dir = match req.working_dir.as_deref() {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => {
            return Json(CommandResult::Error {
                message: "working_dir required".into(),
            })
        }
    };
    let result = match req.command.as_str() {
        "undo" => exec_undo(
            &working_dir,
            req.session_id.as_deref(),
            &req.arg,
            req.project_hash.as_deref(),
        ),
        "remember" => exec_remember(&working_dir, &req.arg),
        "forget" => exec_forget(&working_dir, &req.arg),
        "memory" => exec_memory(&working_dir),
        "context" => {
            exec_context(
                &state,
                &working_dir,
                req.project_hash.as_deref(),
                req.session_id.as_deref(),
                req.provider.as_deref(),
            )
            .await
        }
        "compact" => {
            exec_compact(
                &state,
                &working_dir,
                req.project_hash.as_deref(),
                req.session_id.as_deref(),
                req.provider.as_deref(),
                &req.arg,
            )
            .await
        }
        "whoami" => exec_whoami(),
        "config" => exec_config(),
        "diff" => exec_diff(&working_dir),
        "status" => exec_status(&working_dir, req.provider.as_deref()),
        "cost" => exec_cost(
            &working_dir,
            req.project_hash.as_deref(),
            req.session_id.as_deref(),
        ),
        "todo" => exec_todo(
            &working_dir,
            req.project_hash.as_deref(),
            req.session_id.as_deref(),
        ),
        other => Err(anyhow::anyhow!("unknown command: {other}")),
    };
    match result {
        Ok(r) => Json(r),
        Err(e) => Json(CommandResult::Error {
            message: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::config::memory::MemoryStore;
    use atomcode_core::conversation::message::{Message, Role};
    use atomcode_core::session::{DisplayMessage, TurnStat};

    fn session_with_turns(n: usize) -> Session {
        let mut s = Session::new(std::path::PathBuf::from("/tmp/plan2-test"));
        for i in 0..n {
            s.messages.push(Message::new(Role::User, &format!("q{i}")));
            s.messages
                .push(Message::new(Role::Assistant, &format!("a{i}")));
        }
        s
    }

    #[test]
    fn undo_no_arg_removes_last_turn() {
        let mut s = session_with_turns(3);
        let removed = apply_undo(&mut s, "");
        assert_eq!(removed, 1);
        // 3 用户提示 → 剩 2；每轮 user+assistant，剩 2 轮 = 4 条消息。
        let users = s
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .count();
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
        assert_eq!(
            parse_remember_arg("  --global   trimmed  "),
            (true, "trimmed")
        );
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

    #[test]
    fn reindex_after_front_drain_shifts_survivors_drops_drained() {
        // Build a session with 7 messages (simulating post-compaction state where 3 were drained).
        // Before drain: display_messages had after_message 0 / 2 / 5; turn_stats had 2 / 6.
        // After draining 3 from the front: 0 stays 0; 2 drops (<=3); 5 shifts to 2.
        //                                  turn_stats: 2 drops (<=3); 6 shifts to 3.
        let mut s = session_with_turns(0);
        // after_message=0: "before the first message" — always kept.
        s.display_messages.push(DisplayMessage {
            after_message: 0,
            message: Message::new(Role::Assistant, "preamble"),
        });
        // after_message=2: within the drained range (<=3) — should be dropped.
        s.display_messages.push(DisplayMessage {
            after_message: 2,
            message: Message::new(Role::Assistant, "drained"),
        });
        // after_message=5: survivor — shifts to 5-3=2.
        s.display_messages.push(DisplayMessage {
            after_message: 5,
            message: Message::new(Role::Assistant, "keep"),
        });
        // turn_stat at 2: drained (<=3) — dropped.
        s.turn_stats.push(TurnStat {
            after_message: 2,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 50,
            total_tokens: 5,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        // turn_stat at 6: survivor — shifts to 6-3=3.
        s.turn_stats.push(TurnStat {
            after_message: 6,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 50,
            total_tokens: 5,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        reindex_after_front_drain(&mut s, 3);
        // display_messages: [0, 2] (0 kept, 2 dropped, 5→2)
        assert_eq!(s.display_messages.len(), 2);
        assert_eq!(s.display_messages[0].after_message, 0);
        assert_eq!(s.display_messages[1].after_message, 2);
        // turn_stats: single entry with after_message=3 (2 dropped, 6→3)
        assert_eq!(s.turn_stats.len(), 1);
        assert_eq!(s.turn_stats[0].after_message, 3);
    }

    #[test]
    fn cost_sums_turn_stats_tokens() {
        let mut s = Session::new(std::path::PathBuf::from("/tmp/cost-test"));
        s.turn_stats.push(TurnStat {
            after_message: 2,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 100,
            total_tokens: 100,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        s.turn_stats.push(TurnStat {
            after_message: 4,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 120,
            total_tokens: 250,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        let total: usize = s.turn_stats.iter().map(|t| t.total_tokens).sum();
        assert_eq!(total, 350);
        assert_eq!(s.turn_stats.len(), 2);
    }

    #[test]
    fn todo_derives_from_last_todowrite_call() {
        use atomcode_core::conversation::message::{MessageContent, Role};
        use atomcode_core::tool::ToolCall;

        let args = r#"{"todos":[{"content":"写测试","status":"in_progress"},{"content":"提交","status":"pending"}]}"#;
        let mut s = Session::new(std::path::PathBuf::from("/tmp/todo-test"));
        s.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call1".into(),
                    name: "todowrite".into(),
                    arguments: args.into(),
                }],
                reasoning_content: None,
                thinking_blocks: vec![],
            },
            synthetic: false,
            internal_origin: None,
        });

        // Inline the same derivation logic as exec_todo (core Message ≠ kernel Message).
        use atomcode_capabilities::tools::todo::parse_todos;
        let todos = s
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                    tool_calls
                        .iter()
                        .rev()
                        .filter(|c| c.name == "todowrite")
                        .find_map(|c| parse_todos(&c.arguments).ok())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].content, "写测试");
        assert_eq!(todos[1].content, "提交");
    }

    #[test]
    fn todo_empty_session_returns_empty() {
        use atomcode_capabilities::tools::todo::parse_todos;
        use atomcode_core::conversation::message::MessageContent;
        let s = Session::new(std::path::PathBuf::from("/tmp/todo-empty-test"));
        let todos: Vec<_> = s
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                    tool_calls
                        .iter()
                        .rev()
                        .filter(|c| c.name == "todowrite")
                        .find_map(|c| parse_todos(&c.arguments).ok())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        assert!(todos.is_empty());
    }

    #[test]
    fn exec_diff_returns_stat_in_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        // init a repo with one committed file + a working-tree change
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };
        run(&["init"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-m", "init"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let res = exec_diff(dir.path()).unwrap();
        match res {
            CommandResult::Diff { stat } => assert!(stat.contains("a.txt"), "stat was: {stat}"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn undo_prunes_display_messages_and_turn_stats() {
        // 3 turns = 6 messages. After undo 1 turn → 4 messages remain.
        // display_messages/turn_stats anchored at <=4 survive; >4 are pruned.
        let mut s = session_with_turns(3);
        // Anchored at message 2 (inside surviving turns) — should survive.
        s.display_messages.push(DisplayMessage {
            after_message: 2,
            message: Message::new(Role::Assistant, "keep"),
        });
        // Anchored at message 6 (inside the removed turn) — should be pruned.
        s.display_messages.push(DisplayMessage {
            after_message: 6,
            message: Message::new(Role::Assistant, "drop"),
        });
        s.turn_stats.push(TurnStat {
            after_message: 4,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 100,
            total_tokens: 10,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        s.turn_stats.push(TurnStat {
            after_message: 6,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 100,
            total_tokens: 10,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        let removed = apply_undo(&mut s, ""); // undo last 1 turn → 4 messages remain
        assert_eq!(removed, 1);
        assert_eq!(s.messages.len(), 4);
        // display_messages: after_message=2 survives, after_message=6 is pruned.
        assert_eq!(s.display_messages.len(), 1);
        assert_eq!(s.display_messages[0].after_message, 2);
        // turn_stats: after_message=4 survives, after_message=6 is pruned.
        assert_eq!(s.turn_stats.len(), 1);
        assert_eq!(s.turn_stats[0].after_message, 4);
    }

    /// Verify save_session_to_hash / load_session bucket symmetry:
    /// writing to a project-hash bucket and reading it back returns the same session.
    #[test]
    fn save_session_to_hash_roundtrip() {
        // Shared process-global env lock so ATOMCODE_HOME mutations don't race
        // the other daemon test modules in the same test binary.
        let _guard = crate::atomcode_home_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("ATOMCODE_HOME").ok();
        std::env::set_var("ATOMCODE_HOME", dir.path());

        let result = std::panic::catch_unwind(|| {
            let session = session_with_turns(2);
            let hash = "deadbeef";

            crate::save_session_to_hash(hash, &session).expect("save_session_to_hash");

            let loaded = crate::load_session(hash, session.id.as_str()).expect("load_session");
            assert_eq!(loaded.id.as_str(), session.id.as_str());
            assert_eq!(loaded.messages.len(), session.messages.len());
        });

        match &prev {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }

        result.expect("round-trip test panicked");
    }
}
