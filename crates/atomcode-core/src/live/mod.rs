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
use crate::tool::PermissionDecision;
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
    /// 任一视图（webui 下拉框 / TUI /model）切换了模型。其余视图据此同步显示与选中项。
    /// 仅作显示/状态广播——实际下一轮用哪个 provider 由进程级选择（daemon 侧 LIVE_PROVIDER）决定。
    ProviderChanged(String),
    /// 任一视图（webui /cd）切换了工作目录/项目。其余视图据此跟随：同进程 TUI
    /// 会切到新目录并开一个全新 session（见 event_loop/live_sync 转发）。仅广播
    /// 路径，不触碰 turn 状态。
    WorkingDirChanged(std::path::PathBuf),
    /// 任一视图（webui）创建了新会话并切换到它。其余视图据此同步创建新会话。
    /// 参数为新会话 ID。
    SessionSwitched(String),
    /// daemon AI 给某个会话自动命名了。携带 `session_id` 以便每个 webui tab 只在
    /// 自己正在查看的会话被改名时更新标题（进程级广播会到达所有 tab，无 id 会串台）。
    SessionRenamed { session_id: String, name: String },
    /// 手机 App 请求在桌面 TUI 执行一条斜杠命令（如 `/status`）。仅进程内广播
    /// （to_wire 跳过）；TUI 侧白名单校验后执行，输出经
    /// [`LiveEvent::CommandOutput`] 广播回所有视图。
    RemoteCommand(String),
    /// 斜杠命令的文本输出（如 `/status` 的状态报告）。TUI 执行命令时广播，
    /// 手机/webui 据此显示一条系统消息。不进 turn 回放缓冲（非对话内容）。
    CommandOutput(String),
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
        approver: Arc<Mutex<Option<mpsc::UnboundedSender<PermissionDecision>>>>,
        cancel: CancellationToken,
    );

    /// 投递前对用户输入做预处理。默认直通；需要视觉降级的执行器覆盖——主模型不支持
    /// 视觉且带图时，用 VL 模型把图片转成文字拼进 `text`（原图保留）。在 coordinator
    /// 追加用户消息前调用，故 TUI / webui 等所有入口共享同一套预处理（修复同步重构把
    /// live 路径从 `Agent::run` 切到本协调器后漏掉 VL 预处理的回归）。
    async fn preprocess_input(&self, input: UserInput) -> UserInput {
        input
    }
}

/// 进程内活动会话总线。克隆廉价（内部全是 `Arc`）。
pub struct LiveSession {
    // 锁序：协调器始终先锁 `conversation`、再锁 `snapshot`；视图侧（join/snapshot）只锁 `snapshot`。
    // 新增方法务必遵守此顺序以避免死锁。
    /// turn 边界更新的「已提交快照」，供 `join()` 不阻塞于运行中的 turn。
    snapshot: Arc<Mutex<Vec<Message>>>,
    events: broadcast::Sender<LiveEvent>,
    input_tx: mpsc::UnboundedSender<UserInput>,
    turn_state: Arc<Mutex<TurnState>>,
    /// 当前 turn 的审批响应通道。执行器在需要交互审批的 turn 开始时注册（经 run_turn
    /// 的 approver 参数）；任一视图调用 `approve` 先到先得地投递决定。
    approver: Arc<Mutex<Option<mpsc::UnboundedSender<PermissionDecision>>>>,
    /// 当前运行中 turn 的取消令牌。协调器在 turn 开始时填入、结束时清空；
    /// 任一视图调用 [`LiveSession::cancel_turn`] 即可中断正在跑的回合(停止生成)。
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// 进行中 turn 的事件回放缓冲：`StateChanged(Running)` + 本回合全部 Turn 事件
    /// （文本增量/工具/审批…）。turn 边界清空——此时内容已并入 `snapshot`。
    /// 供晚加入的视图（手机回前台重连、新开 webui tab）在 snapshot 之后重放，
    /// 恢复进行中回合的执行过程；snapshot 只在 turn 边界更新，没有这个缓冲，
    /// 中途断开重连就会丢掉当前回合已发生的一切。
    /// 不含 `UserMessage`（用户消息在投递时即写入 snapshot，重放会重复）。
    /// 锁序：`conversation` → `snapshot` → `turn_buffer`。
    turn_buffer: Arc<std::sync::Mutex<Vec<LiveEvent>>>,
}

impl LiveSession {
    /// 用注入的执行器与初始消息建会话，并 spawn 协调器任务。返回 `Arc` 以便多视图共享。
    pub fn new(executor: Arc<dyn TurnExecutor>, initial: Vec<Message>) -> Arc<Self> {
        Self::new_with_cold_summaries(executor, initial, Vec::new())
    }

    pub fn new_with_cold_summaries(
        executor: Arc<dyn TurnExecutor>,
        initial: Vec<Message>,
        cold_summaries: Vec<String>,
    ) -> Arc<Self> {
        let conversation = Arc::new(Mutex::new({
            Conversation::from_messages_and_cold_summaries(initial.clone(), cold_summaries)
        }));
        let snapshot = Arc::new(Mutex::new(initial));
        let (events, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let turn_state = Arc::new(Mutex::new(TurnState::Idle));
        let current_cancel = Arc::new(Mutex::new(None));
        let approver = Arc::new(Mutex::new(None));
        let turn_buffer = Arc::new(std::sync::Mutex::new(Vec::new()));

        let session = Arc::new(Self {
            snapshot: snapshot.clone(),
            events: events.clone(),
            input_tx,
            turn_state: turn_state.clone(),
            current_cancel: current_cancel.clone(),
            approver: approver.clone(),
            turn_buffer: turn_buffer.clone(),
        });

        tokio::spawn(coordinator(
            executor,
            conversation,
            snapshot,
            events,
            input_rx,
            turn_state,
            current_cancel,
            approver,
            turn_buffer,
        ));

        session
    }

    /// 订阅实时事件（不含历史快照）。滞后丢事件（`RecvError::Lagged`）时改调
    /// [`LiveSession::join`] 重取快照恢复，而非重新 `subscribe`。
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

    /// 晚加入（带回放）：原子地拿「已提交快照 + 进行中回合的事件回放 + 实时订阅」。
    /// 回放区只含 snapshot 里没有的部分（当前回合的 `StateChanged(Running)` 与
    /// Turn 事件）；视图按 snapshot → replay → rx 顺序消费即可无缝恢复进行中的
    /// 回合（工具卡片、流式文本、待审批请求）。无进行中回合时 replay 为空。
    ///
    /// 不丢不重的保证：转发器「入缓冲+上广播」与这里「订阅+克隆缓冲」持同一把
    /// `turn_buffer` 锁——任一事件要么已在克隆里、要么会从 rx 收到，二者互斥。
    pub async fn join_with_replay(
        &self,
    ) -> (Vec<Message>, Vec<LiveEvent>, broadcast::Receiver<LiveEvent>) {
        let snap = self.snapshot.lock().await.clone();
        let buf = self
            .turn_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let rx = self.events.subscribe();
        let replay = buf.clone();
        drop(buf);
        (snap, replay, rx)
    }

    /// 已提交消息快照（turn 边界更新）。
    pub async fn snapshot(&self) -> Vec<Message> {
        self.snapshot.lock().await.clone()
    }

    /// 当前 turn 状态。
    pub async fn state(&self) -> TurnState {
        *self.turn_state.lock().await
    }

    /// Cancel the currently running turn. Returns `false` when the session is idle.
    pub async fn cancel_current_turn(&self) -> bool {
        let cancel = self.current_cancel.lock().await.clone();
        if let Some(cancel) = cancel {
            cancel.cancel();
            true
        } else {
            false
        }
    }

    /// 投递一条用户输入。返回 `false` 表示总线已关闭（协调器任务已退出）。
    /// 注意：是否在「运行中」被忽略由协调器判定（见 `coordinator`）。
    pub fn send_input(&self, input: UserInput) -> bool {
        self.input_tx.send(input).is_ok()
    }

    /// 执行器在需要交互审批时注册响应通道（也供测试直接用）。
    pub async fn register_approver(&self, tx: mpsc::UnboundedSender<PermissionDecision>) {
        *self.approver.lock().await = Some(tx);
    }

    /// 广播一次模型切换给所有视图（不触碰 turn 状态）。任一端切换模型时调用，
    /// 让另一端的下拉框 / 头部显示实时跟随。返回 false 表示当前无订阅者（无妨）。
    pub fn notify_provider_changed(&self, provider: String) -> bool {
        self.events.send(LiveEvent::ProviderChanged(provider)).is_ok()
    }

    /// 广播一次工作目录切换给所有视图（不触碰 turn 状态）。webui /cd 时调用，
    /// 让同进程 TUI 跟随切目录并开一个全新会话。返回 false 表示当前无订阅者（无妨）。
    pub fn notify_working_dir_changed(&self, dir: std::path::PathBuf) -> bool {
        self.events.send(LiveEvent::WorkingDirChanged(dir)).is_ok()
    }

    /// 广播一次会话切换给所有视图（不触碰 turn 状态）。webui 新建对话时调用，
    /// 让同进程 TUI 跟随切换到新会话。返回 false 表示当前无订阅者（无妨）。
    pub fn notify_session_switched(&self, session_id: String) -> bool {
        self.events.send(LiveEvent::SessionSwitched(session_id)).is_ok()
    }

    /// 广播「请在桌面 TUI 执行这条斜杠命令」（手机 App 发起）。返回 false 表示
    /// 无订阅者——通常意味着没有 TUI 附着（headless daemon），命令无人执行。
    pub fn notify_remote_command(&self, line: String) -> bool {
        self.events.send(LiveEvent::RemoteCommand(line)).is_ok()
    }

    /// 广播一条斜杠命令的文本输出给所有视图（TUI 执行 `/status` 等命令时调用）。
    /// 返回 false 表示当前无订阅者（无妨）。
    pub fn notify_command_output(&self, text: String) -> bool {
        self.events.send(LiveEvent::CommandOutput(text)).is_ok()
    }

    /// 任一视图批准/拒绝，投递决定到执行器持有的审批通道。
    ///
    /// 通道在整个回合内保持注册（**不**取走 sender）：同一回合里 TurnRunner 可能
    /// 顺序触发多个工具审批，它们复用执行器在 `run_turn` 起始注册的同一个
    /// `perm_resp_rx`。早先这里用 `.take()` 投递一次后即 drop sender，导致通道
    /// 关闭，回合内第二个及之后的工具在 `InteractivePermissionDecider::decide()`
    /// 里 `recv()` 立刻得到 `None` → 被秒拒（webui 发起、tuix 审批时尤为明显）。
    /// 回合结束由 coordinator 将槽清空，故不会跨回合误用。
    ///
    /// 槽为空（回合外）或通道已关闭时返回 false。
    pub async fn approve(&self, decision: PermissionDecision) -> bool {
        if let Some(tx) = self.approver.lock().await.as_ref() {
            tx.send(decision).is_ok()
        } else {
            false
        }
    }

    /// 取消当前正在运行的 turn(停止生成)。无运行中 turn 时返回 false。
    /// 真正的中断由协调器为本回合登记的 [`CancellationToken`] 驱动 —— 执行器
    /// 的 `run_turn` 把它透传给 TurnRunner,模型流随之中止。任一视图(手机 /
    /// webui / TUI)都可调用,先到先停。
    pub async fn cancel_turn(&self) -> bool {
        if let Some(tok) = self.current_cancel.lock().await.as_ref() {
            tok.cancel();
            true
        } else {
            false
        }
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
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
    approver: Arc<Mutex<Option<mpsc::UnboundedSender<PermissionDecision>>>>,
    turn_buffer: Arc<std::sync::Mutex<Vec<LiveEvent>>>,
) {
    // Diagnostics via `ctrace!` (file sink, gated by ATOMCODE_TRACE), never
    // eprintln: under /webui this coordinator runs in the TUI process, so a
    // stray stderr write lands on the raw-mode terminal and corrupts the
    // display (DEC-graphics mojibake / flooding). See trace.rs.
    crate::ctrace!("LIVE", "coordinator START");
    while let Some(input) = input_rx.recv().await {
        crate::ctrace!("LIVE", "coordinator received input: {} chars", input.text.len());
        // 单写者守卫：运行中直接忽略本次输入（不排队，避免乱序）。
        {
            let mut st = turn_state.lock().await;
            if *st == TurnState::Running {
                crate::ctrace!("LIVE", "coordinator REJECTED input: already running");
                let _ = events.send(LiveEvent::Turn(TurnEvent::Warning(
                    "对方正在对话，已忽略本次输入".to_string(),
                )));
                continue;
            }
            *st = TurnState::Running;
        }
        crate::ctrace!("LIVE", "coordinator ACCEPTED input, broadcasting StateChanged(Running)");
        // 回合开始：清回放缓冲并记入 Running——锁内一并广播，与
        // join_with_replay 的「订阅+克隆」互斥，保证晚加入者不丢不重。
        {
            let mut buf = turn_buffer.lock().unwrap_or_else(|e| e.into_inner());
            buf.clear();
            buf.push(LiveEvent::StateChanged(TurnState::Running));
            let _ = events.send(LiveEvent::StateChanged(TurnState::Running));
        }

        // 先即时回显用户消息（原始文本 + 原图），让发送方立刻看到自己的输入，不被随后
        // 可能较慢的 VL 预处理阻塞——否则带图发送会「发出后很久没反应」。
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
            text: input.text.clone(),
            images: input.images.clone(),
        });

        // 回显之后再做（可能较慢的）VL 预处理：非视觉主模型 + 带图时由执行器把图转文字。
        // 只改写刚追加的这条用户消息的文本（原图保留用于缩略图），显示侧已回显原文、不再
        // 重播。所有入口（TUI / webui）共享此步；期间 state=Running，新输入按守卫被忽略。
        if !input.images.is_empty() {
            let processed = executor.preprocess_input(input.clone()).await;
            if processed.text != input.text {
                let mut conv = conversation.lock().await;
                if let Some(last) = conv.messages.last_mut() {
                    if let MessageContent::MultiPart { text, .. } = &mut last.content {
                        *text = if processed.text.is_empty() {
                            None
                        } else {
                            Some(processed.text)
                        };
                    }
                }
                *snapshot.lock().await = conv.messages.clone();
            }
        }

        // 跑一次 turn（执行器内部持 conv 锁修改并广播 Turn 事件）。
        // 为本回合建取消令牌并登记,供 cancel_turn 中断;turn 结束后清空。
        //
        // 执行器的事件经 tap 通道转发：先记入回放缓冲、再上公共广播——与
        // join_with_replay 持同一把 turn_buffer 锁，晚加入者拿到的「缓冲克隆 +
        // 订阅」不丢不重。run_turn 返回前会排干自己的内部转发（见
        // DaemonTurnExecutor），届时 tap 发送端全部释放、本转发器随之退出。
        let (tap_tx, mut tap_rx) = broadcast::channel::<LiveEvent>(BROADCAST_CAPACITY);
        let fwd_buffer = turn_buffer.clone();
        let fwd_events = events.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                match tap_rx.recv().await {
                    Ok(ev) => {
                        let mut buf = fwd_buffer.lock().unwrap_or_else(|e| e.into_inner());
                        buf.push(ev.clone());
                        let _ = fwd_events.send(ev);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let cancel = CancellationToken::new();
        *current_cancel.lock().await = Some(cancel.clone());
        executor
            .run_turn(&conversation, tap_tx.clone(), approver.clone(), cancel)
            .await;
        *current_cancel.lock().await = None;
        // 关闭 tap 并等转发器排干：保证本回合事件都已入缓冲/广播，
        // 再做下面的边界清空，否则迟到事件会留在缓冲里被永远重放。
        drop(tap_tx);
        let _ = forwarder.await;

        // turn 结束：清空审批槽（防止旧通道被下一个 turn 误用）。
        *approver.lock().await = None;

        // turn 结束：刷新已提交快照、清回放缓冲（同临界区——回合内容此刻起
        // 由 snapshot 承载，晚加入者拿到「全量快照 + 空回放」，不会重复）。
        {
            let conv = conversation.lock().await;
            let mut snap = snapshot.lock().await;
            let mut buf = turn_buffer.lock().unwrap_or_else(|e| e.into_inner());
            *snap = conv.messages.clone();
            buf.clear();
        }
        // 排空 turn 执行期间堆积的输入（不排队，避免乱序）；逐条给出忽略提示，
        // 与运行中守卫的反馈一致，避免静默丢弃。
        while input_rx.try_recv().is_ok() {
            let _ = events.send(LiveEvent::Turn(TurnEvent::Warning(
                "对方正在对话，已忽略本次输入".to_string(),
            )));
        }
        *turn_state.lock().await = TurnState::Idle;
        crate::ctrace!("LIVE", "coordinator turn done, broadcasting StateChanged(Idle)");
        let _ = events.send(LiveEvent::StateChanged(TurnState::Idle));
    }
    crate::ctrace!("LIVE", "coordinator EXIT (input_rx closed)");
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

    struct CancellableExecutor;

    #[async_trait]
    impl TurnExecutor for CancellableExecutor {
        async fn run_turn(
            &self,
            _conv: &Arc<Mutex<Conversation>>,
            _events: broadcast::Sender<LiveEvent>,
            _approver: Arc<Mutex<Option<mpsc::UnboundedSender<PermissionDecision>>>>,
            cancel: CancellationToken,
        ) {
            cancel.cancelled().await;
        }
    }

    #[async_trait]
    impl TurnExecutor for FakeExecutor {
        async fn run_turn(
            &self,
            conv: &Arc<Mutex<Conversation>>,
            events: broadcast::Sender<LiveEvent>,
            _approver: Arc<Mutex<Option<mpsc::UnboundedSender<PermissionDecision>>>>,
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

        // 第一条：开始一个长 turn。
        assert!(session.send_input(UserInput { text: "first".into(), images: vec![] }));
        loop {
            if let Ok(LiveEvent::StateChanged(TurnState::Running)) = rx.recv().await {
                break;
            }
        }
        // 运行中投第二条：应被忽略（不排队、不作为新 turn 执行）。
        session.send_input(UserInput { text: "second".into(), images: vec![] });
        let _ = drain_until_idle(&mut rx).await; // 第一条 turn 收尾

        // 再投第三条合法输入并等其收尾：若 "second" 被正确丢弃，calls 应为 2（first+third）；
        // 若 "second" 漏跑成了第二个 turn，calls 会是 3。事件驱动、无 sleep。
        assert!(session.send_input(UserInput { text: "third".into(), images: vec![] }));
        let _ = drain_until_idle(&mut rx).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "运行中投的第二条应被丢弃，仅 first+third 执行");
    }

    #[tokio::test]
    async fn cancel_current_turn_stops_running_executor() {
        let session = LiveSession::new(Arc::new(CancellableExecutor), Vec::new());
        let mut rx = session.subscribe();

        assert!(session.send_input(UserInput { text: "cancel me".into(), images: vec![] }));
        loop {
            if let Ok(LiveEvent::StateChanged(TurnState::Running)) = rx.recv().await {
                break;
            }
        }

        assert!(session.cancel_current_turn().await);
        let events = tokio::time::timeout(std::time::Duration::from_secs(1), drain_until_idle(&mut rx))
            .await
            .expect("cancelled turn should become idle promptly");
        assert!(matches!(events.last(), Some(LiveEvent::StateChanged(TurnState::Idle))));
        assert!(!session.cancel_current_turn().await);
    }

    #[tokio::test]
    async fn join_returns_committed_snapshot_and_live_receiver() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = LiveSession::new(fake(calls.clone()), vec![Message::new(Role::User, "seed")]);

        let (snap, _rx) = session.join().await;
        assert_eq!(snap.len(), 1, "晚加入应拿到既有快照");
        assert_eq!(snap[0].text(), Some("seed"));
    }

    /// 中途加入（手机退后台回来重连）：join_with_replay 必须能补回当前回合
    /// 已发生的执行过程（Running + 已广播的 Turn 事件），且回合结束后缓冲清空。
    #[tokio::test]
    async fn join_with_replay_recovers_in_flight_turn_and_clears_at_boundary() {
        let calls = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn TurnExecutor> =
            Arc::new(FakeExecutor { calls: calls.clone(), reply: "hi".into(), delay_ms: 120 });
        let session = LiveSession::new(exec, Vec::new());
        let mut rx = session.subscribe();

        assert!(session.send_input(UserInput { text: "你好".into(), images: vec![] }));
        // 等 TextDelta 上广播（FakeExecutor 先发增量再 sleep），此刻回合仍在进行。
        loop {
            if let Ok(LiveEvent::Turn(TurnEvent::TextDelta(_))) = rx.recv().await {
                break;
            }
        }

        // 模拟断线后重连：此刻加入应拿到 Running + TextDelta 的回放。
        let (snap, replay, _rx2) = session.join_with_replay().await;
        assert_eq!(snap.len(), 1, "进行中：快照只含用户消息（turn 未到边界）");
        assert!(
            matches!(replay.first(), Some(LiveEvent::StateChanged(TurnState::Running))),
            "回放首条应为 Running，让重连方恢复 streaming 状态"
        );
        assert!(
            replay.iter().any(|e| matches!(e, LiveEvent::Turn(TurnEvent::TextDelta(t)) if t == "hi")),
            "回放应包含已发生的文本增量"
        );
        assert!(
            !replay.iter().any(|e| matches!(e, LiveEvent::UserMessage { .. })),
            "用户消息已在快照里，回放不应重复携带"
        );

        // 回合收尾后：内容并入快照，回放缓冲应清空。
        let _ = drain_until_idle(&mut rx).await;
        let (snap, replay, _rx3) = session.join_with_replay().await;
        assert_eq!(snap.len(), 2, "收尾后快照含完整回合");
        assert!(replay.is_empty(), "边界处缓冲应已清空，避免重复回放");
    }

    #[tokio::test]
    async fn approval_channel_survives_multiple_tools_in_one_turn() {
        use crate::tool::PermissionDecision;
        let calls = Arc::new(AtomicUsize::new(0));
        let session = LiveSession::new(fake(calls), Vec::new());

        // 回合外、未注册审批通道：投递失败。
        assert!(!session.approve(PermissionDecision::Allow).await);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PermissionDecision>();
        session.register_approver(tx).await;

        // 同一回合内连续多个工具审批，每次都必须成功投递。回归用例：早先 approve()
        // 用 .take() 在第一次投递后 drop 掉唯一的 sender，关闭了执行器持有的
        // 审批通道，使第二个及之后的工具被 InteractivePermissionDecider 秒拒。
        assert!(session.approve(PermissionDecision::Allow).await);
        assert!(session.approve(PermissionDecision::Deny).await);
        assert!(session.approve(PermissionDecision::Allow).await);

        assert!(matches!(rx.recv().await, Some(PermissionDecision::Allow)));
        assert!(matches!(rx.recv().await, Some(PermissionDecision::Deny)));
        assert!(matches!(rx.recv().await, Some(PermissionDecision::Allow)));
    }
}
