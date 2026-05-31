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

/// 进程内活动会话总线。克隆廉价（内部全是 `Arc`）。
pub struct LiveSession {
    /// 单一数据源：turn 期间被执行器持锁修改。
    #[allow(dead_code)]
    conversation: Arc<Mutex<Conversation>>,
    /// turn 边界更新的「已提交快照」，供 `join()` 不阻塞于运行中的 turn。
    snapshot: Arc<Mutex<Vec<Message>>>,
    events: broadcast::Sender<LiveEvent>,
    input_tx: mpsc::UnboundedSender<UserInput>,
    turn_state: Arc<Mutex<TurnState>>,
}

impl LiveSession {
    /// 用注入的执行器与初始消息建会话，并 spawn 协调器任务。返回 `Arc` 以便多视图共享。
    pub fn new(executor: Arc<dyn TurnExecutor>, initial: Vec<Message>) -> Arc<Self> {
        let conversation = Arc::new(Mutex::new({
            let mut c = Conversation::new();
            c.messages = initial.clone();
            c
        }));
        let snapshot = Arc::new(Mutex::new(initial));
        let (events, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let turn_state = Arc::new(Mutex::new(TurnState::Idle));

        let session = Arc::new(Self {
            conversation: conversation.clone(),
            snapshot: snapshot.clone(),
            events: events.clone(),
            input_tx,
            turn_state: turn_state.clone(),
        });

        tokio::spawn(coordinator(
            executor,
            conversation,
            snapshot,
            events,
            input_rx,
            turn_state,
        ));

        session
    }

    /// 订阅实时事件（不含历史快照）。
    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.events.subscribe()
    }

    /// 晚加入：原子地拿「已提交快照 + 实时订阅」。快照取自 turn 边界更新的副本，
    /// 故运行中的长 turn 不会阻塞 join；新订阅者随后从广播接当前 turn 的增量。
    pub async fn join(&self) -> (Vec<Message>, broadcast::Receiver<LiveEvent>) {
        let snap = self.snapshot.lock().await.clone();
        let rx = self.events.subscribe();
        (snap, rx)
    }

    /// 已提交消息快照（turn 边界更新）。
    pub async fn snapshot(&self) -> Vec<Message> {
        self.snapshot.lock().await.clone()
    }

    /// 当前 turn 状态。
    pub async fn state(&self) -> TurnState {
        *self.turn_state.lock().await
    }

    /// 投递一条用户输入。返回 `false` 表示总线已关闭（协调器任务已退出）。
    /// 注意：是否在「运行中」被忽略由协调器判定（见 `coordinator`）。
    pub fn send_input(&self, input: UserInput) -> bool {
        self.input_tx.send(input).is_ok()
    }
}

/// 协调器：单写者跑 turn。
async fn coordinator(
    executor: Arc<dyn TurnExecutor>,
    conversation: Arc<Mutex<Conversation>>,
    snapshot: Arc<Mutex<Vec<Message>>>,
    events: broadcast::Sender<LiveEvent>,
    mut input_rx: mpsc::UnboundedReceiver<UserInput>,
    turn_state: Arc<Mutex<TurnState>>,
) {
    while let Some(input) = input_rx.recv().await {
        // 单写者守卫：运行中直接忽略本次输入（不排队，避免乱序）。
        {
            let mut st = turn_state.lock().await;
            if *st == TurnState::Running {
                let _ = events.send(LiveEvent::Turn(TurnEvent::Warning(
                    "对方正在对话，已忽略本次输入".to_string(),
                )));
                continue;
            }
            *st = TurnState::Running;
        }
        let _ = events.send(LiveEvent::StateChanged(TurnState::Running));

        // 追加用户消息（持 conv 锁），并广播 UserMessage、刷新已提交快照。
        {
            let mut conv = conversation.lock().await;
            if input.images.is_empty() {
                conv.add_user_message(&input.text);
            } else {
                let idx = conv.messages.len();
                conv.messages.push(Message {
                    role: Role::User,
                    content: MessageContent::MultiPart {
                        text: if input.text.is_empty() {
                            None
                        } else {
                            Some(input.text.clone())
                        },
                        images: input.images.clone(),
                    },
                    synthetic: false,
                });
                conv.turn_tracker.on_user_message(idx);
            }
            *snapshot.lock().await = conv.messages.clone();
        }
        let _ = events.send(LiveEvent::UserMessage {
            text: input.text,
            images: input.images,
        });

        // 跑一次 turn（执行器内部持 conv 锁修改并广播 Turn 事件）。
        executor
            .run_turn(&conversation, events.clone(), CancellationToken::new())
            .await;

        // turn 结束：刷新已提交快照、置 Idle。
        {
            let conv = conversation.lock().await;
            *snapshot.lock().await = conv.messages.clone();
        }
        // 排空 turn 执行期间堆积的输入（运行中忽略语义：不排队）。
        while input_rx.try_recv().is_ok() {}
        *turn_state.lock().await = TurnState::Idle;
        let _ = events.send(LiveEvent::StateChanged(TurnState::Idle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 测试用执行器：对 conv 追加一条 assistant 消息，并广播一个 TextDelta +
    /// 一个 TokenUsage 作为「turn 事件」。记录被调用次数。`delay_ms` 用于模拟
    /// 长 turn，验证「运行中拒绝新输入」与「运行中仍可 join 拿快照」。
    struct FakeExecutor {
        calls: Arc<AtomicUsize>,
        reply: String,
        delay_ms: u64,
    }

    #[async_trait]
    impl TurnExecutor for FakeExecutor {
        async fn run_turn(
            &self,
            conv: &Arc<Mutex<Conversation>>,
            events: broadcast::Sender<LiveEvent>,
            _cancel: CancellationToken,
        ) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = events.send(LiveEvent::Turn(TurnEvent::TextDelta(self.reply.clone())));
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            {
                let mut c = conv.lock().await;
                c.messages.push(Message::new(Role::Assistant, self.reply.clone()));
            }
            let _ = events.send(LiveEvent::Turn(TurnEvent::TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
            }));
        }
    }

    fn fake(calls: Arc<AtomicUsize>) -> Arc<dyn TurnExecutor> {
        Arc::new(FakeExecutor { calls, reply: "hi".to_string(), delay_ms: 0 })
    }

    /// 收集广播事件直到收到一个 StateChanged(Idle)（表示一次 turn 收尾）。
    async fn drain_until_idle(rx: &mut broadcast::Receiver<LiveEvent>) -> Vec<LiveEvent> {
        let mut out = Vec::new();
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let done = matches!(ev, LiveEvent::StateChanged(TurnState::Idle));
                    out.push(ev);
                    if done {
                        return out;
                    }
                }
                Err(_) => return out,
            }
        }
    }

    #[tokio::test]
    async fn runs_turn_and_broadcasts_user_then_turn_then_idle() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = LiveSession::new(fake(calls.clone()), Vec::new());
        let mut rx = session.subscribe();

        assert!(session.send_input(UserInput { text: "你好".into(), images: vec![] }));

        let events = drain_until_idle(&mut rx).await;
        assert!(matches!(events.first(), Some(LiveEvent::StateChanged(TurnState::Running))));
        assert!(events.iter().any(|e| matches!(e, LiveEvent::UserMessage { text, .. } if text == "你好")));
        assert!(events.iter().any(|e| matches!(e, LiveEvent::Turn(TurnEvent::TextDelta(t)) if t == "hi")));
        assert!(matches!(events.last(), Some(LiveEvent::StateChanged(TurnState::Idle))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let snap = session.snapshot().await;
        assert_eq!(snap.len(), 2);
    }

    #[tokio::test]
    async fn rejects_input_while_running() {
        let calls = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn TurnExecutor> =
            Arc::new(FakeExecutor { calls: calls.clone(), reply: "x".into(), delay_ms: 80 });
        let session = LiveSession::new(exec, Vec::new());
        let mut rx = session.subscribe();

        assert!(session.send_input(UserInput { text: "first".into(), images: vec![] }));
        loop {
            if let Ok(LiveEvent::StateChanged(TurnState::Running)) = rx.recv().await {
                break;
            }
        }
        session.send_input(UserInput { text: "second".into(), images: vec![] });

        let _ = drain_until_idle(&mut rx).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "运行中投的第二条应被忽略");
    }

    #[tokio::test]
    async fn join_returns_committed_snapshot_and_live_receiver() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = LiveSession::new(fake(calls.clone()), vec![Message::new(Role::User, "seed")]);

        let (snap, _rx) = session.join().await;
        assert_eq!(snap.len(), 1, "晚加入应拿到既有快照");
        assert_eq!(snap[0].text(), Some("seed"));
    }
}
