//! 进程内「活动会话总线」（Phase 1.5 阶段①）。
//!
//! 模型：一个活动会话，多个视图（TUI / webui）。`LiveSession` 是单一数据源
//! （`Conversation`）+ 事件扇出（`broadcast`）+ 单一输入入口（`mpsc`）+ 单写者
//! turn 守卫。turn 的实际执行通过 [`TurnExecutor`] 注入，便于单测（`FakeExecutor`）
//! 与后续接真实 `TurnRunner`（阶段②）。

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::conversation::message::{ImagePart, Message, MessageContent, Role};
use crate::conversation::Conversation;
use crate::turn::event::TurnEvent;

/// 广播容量。视图滞后超过此值会丢最旧事件（视图可重新 `join()` 拉快照恢复）。
const BROADCAST_CAPACITY: usize = 1024;

/// 任一视图提交的用户输入。
#[derive(Debug, Clone)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<ImagePart>,
}

/// 单写者守卫状态：同一会话同一时刻只跑一个 turn。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Idle,
    Running,
}

/// 广播给所有视图的事件。把「用户消息」「turn 事件」「turn 状态变化」统一成一个
/// 信封，视图据此渲染与启停输入框。
#[derive(Debug, Clone)]
pub enum LiveEvent {
    /// 某视图刚提交的用户消息（让其他视图也能渲染出这条用户气泡）。
    UserMessage { text: String, images: Vec<ImagePart> },
    /// 一次 turn 执行产生的事件（文本增量、工具、审批请求等）。
    Turn(TurnEvent),
    /// turn 状态变化（视图据此禁用/启用输入框）。
    StateChanged(TurnState),
}

/// turn 执行策略。实现者负责对 `conv` 跑一次完整 turn（含工具循环），并把过程
/// 事件作为 [`LiveEvent::Turn`] 发到 `events`。本阶段单测用 `FakeExecutor`；
/// 阶段② 提供包裹 `TurnRunner` 的真实实现。
#[async_trait]
pub trait TurnExecutor: Send + Sync {
    async fn run_turn(
        &self,
        conv: &Arc<Mutex<Conversation>>,
        events: broadcast::Sender<LiveEvent>,
        cancel: CancellationToken,
    );
}
