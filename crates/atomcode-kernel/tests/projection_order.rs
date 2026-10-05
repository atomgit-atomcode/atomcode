//! 日志投影必须让 assistant 的 `tool_calls` 后面紧跟它的全部 tool 结果。
//!
//! 现场（2026-10-06，会话 `b225a509`）：重复调用熔断在工具批执行中、结果落盘之前提交了
//! 一条 `Injected`，日志顺序成了 `assistant(tool_calls) → injected → tool_result`。投影
//! 按日志顺序落消息，于是请求里出现 `assistant(tool_calls) → user → tool`。openrouter 放过
//! 了它，换到 litellm/DeepSeek 后每个请求都被 400（`insufficient tool messages following
//! tool_calls message`），会话从此锁死——每轮请求都由投影重建。
//!
//! 投影的修法与 oh-my-pi 的 `transformMessages` 同向：调用还在等结果时落下的非 tool 消息
//! 先压住，这批结果到齐再放。日志不改（ADR 0024），已经写坏的会话也随之恢复。

use atomcode_kernel::message::{ImageContent, Role};
use atomcode_kernel::session::{derive_messages, InjectionOrigin, LoggedEvent, SessionEvent};
use atomcode_kernel::tool::ToolCall;

fn at(seq: u64, event: SessionEvent) -> LoggedEvent {
    LoggedEvent { seq, at: 0, event }
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "todowrite".into(),
        arguments: "{}".into(),
    }
}

fn user(seq: u64, text: &str) -> LoggedEvent {
    at(
        seq,
        SessionEvent::UserMessage {
            turn: 1,
            text: text.into(),
            images: vec![],
        },
    )
}

fn asks(seq: u64, calls: &[&str]) -> LoggedEvent {
    at(
        seq,
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: calls.iter().map(|id| call(id)).collect(),
            reasoning_blocks: vec![],
            meta: None,
        },
    )
}

fn says(seq: u64, text: &str) -> LoggedEvent {
    at(
        seq,
        SessionEvent::AssistantMessage {
            turn: 1,
            round: 2,
            text: text.into(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_blocks: vec![],
            meta: None,
        },
    )
}

fn result(seq: u64, id: &str, images: Vec<ImageContent>) -> LoggedEvent {
    at(
        seq,
        SessionEvent::ToolResultLogged {
            turn: 1,
            round: 1,
            call_id: id.into(),
            content: format!("result of {id}"),
            is_error: false,
            images,
        },
    )
}

fn nudge(seq: u64, text: &str) -> LoggedEvent {
    at(
        seq,
        SessionEvent::Injected {
            turn: 1,
            text: text.into(),
            origin: InjectionOrigin::Continuation,
        },
    )
}

/// What follows each assistant message that asked for tools must be its tool
/// results, all of them, before anything else.
fn assert_results_follow_their_calls(
    events: &[LoggedEvent],
) -> Vec<atomcode_kernel::message::Message> {
    let messages = derive_messages(events);
    for (i, message) in messages.iter().enumerate() {
        if message.role != Role::Assistant || message.tool_calls.is_empty() {
            continue;
        }
        let answered: Vec<&str> = messages[i + 1..]
            .iter()
            .take_while(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let asked: Vec<&str> = message.tool_calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            answered, asked,
            "assistant(tool_calls) 后面必须紧跟它的全部 tool 结果，中间夹 user 消息就是 \
             provider 拒收的负载（insufficient tool messages following tool_calls message）:\n\
             {messages:#?}"
        );
    }
    messages
}

#[test]
fn a_nudge_logged_before_the_result_reaches_the_model_after_it() {
    let events = vec![
        user(1, "改一下清单"),
        asks(2, &["c0"]),
        nudge(3, "You have issued the SAME tool call"),
        result(4, "c0", vec![]),
    ];
    let messages = assert_results_follow_their_calls(&events);
    let last = messages.last().expect("messages");
    assert_eq!(last.role, Role::User);
    assert!(
        last.text.contains("SAME tool call"),
        "the nudge is not dropped, only moved after the batch: {messages:#?}"
    );
}

#[test]
fn a_note_between_two_results_of_one_batch_waits_for_the_batch_and_its_pictures() {
    let picture = ImageContent {
        media_type: "image/png".into(),
        data: "QUFB".into(),
    };
    let events = vec![
        user(1, "看图"),
        asks(2, &["c0", "c1"]),
        result(3, "c0", vec![picture.clone()]),
        nudge(4, "note"),
        result(5, "c1", vec![]),
    ];
    let messages = assert_results_follow_their_calls(&events);
    let tail: Vec<(Role, bool, &str)> = messages[messages.len() - 2..]
        .iter()
        .map(|m| (m.role.clone(), !m.images.is_empty(), m.text.as_str()))
        .collect();
    assert_eq!(
        tail,
        vec![(Role::User, true, ""), (Role::User, false, "note")],
        "the batch's pictures ride first, right after its results; the note after them"
    );
}

#[test]
fn what_lands_after_a_finished_batch_keeps_its_place() {
    let events = vec![
        user(1, "go"),
        asks(2, &["c0"]),
        result(3, "c0", vec![]),
        nudge(4, "between rounds"),
        says(5, "done"),
    ];
    let messages = assert_results_follow_their_calls(&events);
    let order: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(
        order,
        vec!["go", "", "result of c0", "between rounds", "done"],
        "nothing is reordered when no call is waiting"
    );
}
