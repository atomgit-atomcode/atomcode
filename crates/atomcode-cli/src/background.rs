//! 后台会话:一个屏幕、一个前台 runtime、至多 [`MOST`] 个后台 runtime
//! (`docs/plans/2026-09-25-bg-design.md`)。
//!
//! **屏幕仍然只跟一个会话。** 握多个 runtime 的是这里:屏幕拿到的
//! [`HostConnection`] 是转手的——命令、事件、宿主事件都转给**前台**那一个;换前台
//! 就是换屏幕跟的那条流,屏幕看到的是一次 `SessionChanged`,和 `/resume` 一样。
//!
//! **这里不是第二个生命周期 owner。** 每个 runtime 是它自己的 `CodingRuntime`,
//! 由它自己的 [`RuntimeControl`](crate::host) 驱动,各有各的 `FrontEnd`、App 与
//! 会话租约。这里只管槽位表与转发,以及在丢弃、退出时调用那个 runtime 自己的
//! `cancel` / `shutdown`。
//!
//! 事件不在这里缓存:放到后台的会话回来时,屏幕从它自己的日志重放
//! (`Subscribe { from: 0 }`)。唯一记着的是「挂着没答的那个 `Request`」——它不是
//! 日志里的事实,重订阅不会再发一次。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::runtime::{RuntimePhase, UserInput};
use atomcode_coding::{CodingAgentConfig, CodingRuntime};
use atomcode_harness::feed::Feed;
use atomcode_host_api::{
    BackgroundSession, BackgroundState, HostCommand, HostConnection, HostControl, HostError,
    HostEvent, HostReply,
};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::session::SessionEvent;
use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::host::{HostConfig, RuntimeControl};

/// 后台最多放这么多个会话。与 tuix 一致。
pub const MOST: usize = 16;

/// 一个新起来的 runtime,和它喂的那个前端。
pub struct Spawned {
    pub runtime: CodingRuntime,
    /// 这个 runtime **起的时候**就带着的那一个——它的 App 往这里喂。
    pub front_end: Arc<FrontEnd>,
    pub config: CodingAgentConfig,
}

/// 在某个目录里起一个新会话的 runtime。与启动时同一条装配路径(`main.rs` 用
/// `spawn_native_cli_runtime`),这里不另建一套。
pub type Spawn = Arc<dyn Fn(PathBuf) -> BoxFuture<'static, Result<Spawned, String>> + Send + Sync>;

/// 一个接上了的 runtime。
struct Live {
    id: u64,
    control: Arc<RuntimeControl>,
    commands: mpsc::UnboundedSender<AgentCommand>,
    /// 现在是不是屏幕上那一个——共享只发它的事件。
    shown: Arc<AtomicBool>,
    track: Arc<Mutex<Track>>,
    /// 放到后台的时刻,unix 毫秒。
    since: u64,
}

/// 从一个 runtime 的事件里记下的、面板要说的那几件事。
#[derive(Default)]
struct Track {
    /// 在等人回答的那个请求。回到前台、屏幕重订阅之后再发一次。
    pending: Option<AgentEvent>,
    /// 上一个回合怎么结束的。新回合开始(由这里提交的,或放到后台时正在跑的)时清掉。
    ended: Option<BackgroundState>,
    /// 最近一次出的错,给面板那一行。
    error: Option<String>,
}

impl Track {
    /// 记下一件事。返回面板要不要重画。
    fn saw(&mut self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::Request { .. } => {
                self.pending = Some(event.clone());
                true
            }
            AgentEvent::TurnComplete { reason, .. } => {
                self.pending = None;
                self.ended = Some(match reason {
                    StopReason::Cancelled => BackgroundState::Cancelled,
                    StopReason::ProviderError
                    | StopReason::Timeout
                    | StopReason::PromptRejected
                    | StopReason::RateLimited
                    | StopReason::InvariantViolated => BackgroundState::Failed,
                    _ => BackgroundState::Done,
                });
                true
            }
            AgentEvent::Error { message, .. } => {
                self.error = Some(message.clone());
                false
            }
            _ => false,
        }
    }
}

struct State {
    front: Live,
    slots: Vec<Live>,
    /// 屏幕订阅了前台那个会话没有。换前台之后、屏幕重订阅之前,前台的事件不转:
    /// 那些是上一个视图不认识的东西,而它们都在日志里,订阅时会重放。
    gate: bool,
}

/// 一个屏幕背后的所有 runtime。
pub struct Background {
    state: Mutex<State>,
    spawn: Spawn,
    host_config: Option<Arc<dyn HostConfig>>,
    /// 屏幕读的那条事件流。
    screen: mpsc::UnboundedSender<AgentEvent>,
    watchers: Mutex<Vec<mpsc::UnboundedSender<HostEvent>>>,
    next: AtomicU64,
    /// 换前台、放后台、丢弃一次只做一件:两件交错,槽位表会被写成两者都没想要的样子。
    op: tokio::sync::Mutex<()>,
    me: Weak<Background>,
    /// The sessions stopped because the screen went away, in slot order — what
    /// the launcher prints a `resume` line for after the terminal is back.
    left: Mutex<Vec<String>>,
    /// The session in front when the screen went away, if it had anything said
    /// in it — `None` inside until the exit is recorded, `Some(None)` for an
    /// empty one, which gets no `resume` line.
    exit_front: Mutex<Option<Option<String>>>,
}

/// [`crate::host::connect`],外加后台会话。
pub fn connect(
    runtime: CodingRuntime,
    front_end: Arc<FrontEnd>,
    config: CodingAgentConfig,
    host_config: Option<Arc<dyn HostConfig>>,
    spawn: Spawn,
) -> Result<(HostConnection, Arc<Background>), String> {
    let (screen, events) = mpsc::unbounded_channel();
    let (commands, mut command_rx) = mpsc::unbounded_channel::<AgentCommand>();
    let (front, parts) = attach(
        0,
        Spawned {
            runtime,
            front_end,
            config,
        },
        host_config.clone(),
        true,
    )?;
    let background = Arc::new_cyclic(|me: &Weak<Background>| Background {
        state: Mutex::new(State {
            front,
            slots: Vec::new(),
            gate: true,
        }),
        spawn,
        host_config,
        screen,
        watchers: Mutex::new(Vec::new()),
        next: AtomicU64::new(1),
        op: tokio::sync::Mutex::new(()),
        me: me.clone(),
        left: Mutex::new(Vec::new()),
        exit_front: Mutex::new(None),
    });
    // 泵在 `Arc` 建好之后才起:起早了,第一条事件升级不了弱引用,泵就停了。
    parts.pump(Arc::downgrade(&background));
    crate::tui_share::remember(background.front_control());

    let routing = background.clone();
    tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            routing.route(command).await;
        }
    });
    let session = background.front_control().session_id();
    Ok((
        HostConnection {
            session,
            commands,
            events,
            control: background.clone(),
        },
        background,
    ))
}

/// 接上了、泵还没起的那一半。
struct Pumps {
    id: u64,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    said: mpsc::UnboundedReceiver<HostEvent>,
    track: Arc<Mutex<Track>>,
}

impl Pumps {
    /// 事件与宿主事件各一条泵,都交给 `background` 决定转不转。
    fn pump(self, background: Weak<Background>) {
        let Pumps {
            id,
            mut events,
            mut said,
            track,
        } = self;
        {
            let background = background.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    let changed = track.lock().expect("track poisoned").saw(&event);
                    let Some(background) = background.upgrade() else {
                        break;
                    };
                    background.arrived(id, event, changed);
                }
            });
        }
        tokio::spawn(async move {
            while let Some(event) = said.recv().await {
                let Some(background) = background.upgrade() else {
                    break;
                };
                background.announced(id, event);
            }
        });
    }
}

/// 接上一个 runtime。
fn attach(
    id: u64,
    spawned: Spawned,
    host_config: Option<Arc<dyn HostConfig>>,
    shown: bool,
) -> Result<(Live, Pumps), String> {
    let flag = Arc::new(AtomicBool::new(shown));
    let (connection, control) = crate::host::attach(
        spawned.runtime,
        spawned.front_end,
        spawned.config,
        host_config,
        Some(flag.clone()),
    )?;
    let HostConnection {
        commands, events, ..
    } = connection;
    let track = Arc::new(Mutex::new(Track::default()));
    let said = control.subscribe();
    Ok((
        Live {
            id,
            control,
            commands,
            shown: flag,
            track: track.clone(),
            since: now_ms(),
        },
        Pumps {
            id,
            events,
            said,
            track,
        },
    ))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 一个 runtime 现在在干什么,照面板要的样子。
fn state_of(live: &Live) -> BackgroundState {
    let handle = live.control.runtime();
    if handle.is_stopped() {
        return BackgroundState::Failed;
    }
    let ended = live.track.lock().expect("track poisoned").ended;
    match handle.status().phase {
        RuntimePhase::WaitingApproval => BackgroundState::Waiting,
        RuntimePhase::Failed | RuntimePhase::Stopped | RuntimePhase::ShuttingDown => {
            BackgroundState::Failed
        }
        // 回合结束的事件先到、阶段后落的那一小段里,信事件。
        RuntimePhase::InTurn => ended.unwrap_or(BackgroundState::Running),
        _ => ended.unwrap_or(BackgroundState::Idle),
    }
}

fn in_turn(live: &Live) -> bool {
    matches!(
        live.control.runtime().status().phase,
        RuntimePhase::InTurn | RuntimePhase::WaitingApproval
    )
}

/// 这个会话的日志,从它自己的 App 里读。
fn log_of(live: &Live) -> Vec<SessionEvent> {
    let session = live.control.session_id();
    live.control
        .front_end()
        .app()
        .and_then(|app| Feed::find(&app, &session))
        .map(|agent| {
            agent
                .session()
                .events()
                .into_iter()
                .map(|logged| logged.event)
                .collect()
        })
        .unwrap_or_default()
}

/// 一行里放得下的一句:第一行非空的,压掉首尾空白。
fn one_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(240).collect())
}

fn describe(live: &Live) -> BackgroundSession {
    let state = state_of(live);
    let log = log_of(live);
    let title = log.iter().rev().find_map(|event| match event {
        SessionEvent::Titled { title, .. } if !title.trim().is_empty() => Some(title.clone()),
        _ => None,
    });
    let asked = || {
        log.iter().rev().find_map(|event| match event {
            SessionEvent::Asked { question, .. } => one_line(&question.prompt),
            _ => None,
        })
    };
    let said = || {
        log.iter().rev().find_map(|event| match event {
            SessionEvent::AssistantMessage { text, .. }
            | SessionEvent::PartialReply { text, .. } => one_line(text),
            _ => None,
        })
    };
    let first_words = || {
        log.iter().find_map(|event| match event {
            SessionEvent::UserMessage { text, .. } => one_line(text),
            _ => None,
        })
    };
    // 在等的那个请求本身说了什么:不是每种问法都先在日志里记一条 `Asked`。
    let waiting_on = || {
        let track = live.track.lock().expect("track poisoned");
        let Some(AgentEvent::Request { payload, .. }) = track.pending.as_ref() else {
            return None;
        };
        let words = |key: &str| payload.get(key).and_then(|v| v.as_str()).and_then(one_line);
        words("question").or_else(|| words("prompt")).or_else(|| {
            words("tool").map(|tool| match payload.get("args") {
                Some(args) => one_line(&format!("{tool} {args}")).unwrap_or(tool),
                None => tool,
            })
        })
    };
    let last = match state {
        BackgroundState::Waiting => asked().or_else(waiting_on).or_else(said),
        BackgroundState::Failed => live
            .track
            .lock()
            .expect("track poisoned")
            .error
            .clone()
            .and_then(|error| one_line(&error))
            .or_else(said),
        _ => said(),
    };
    BackgroundSession {
        session: live.control.session_id(),
        // 还没起名的会话,用它的第一句话当名字——面板那一列总得有点什么可认。
        title: title.or_else(first_words),
        state,
        created_at: live.since,
        last,
    }
}

/// 一个会话里有没有人说过话。没有的话它不值得占一个槽位。
fn has_conversation(live: &Live) -> bool {
    log_of(live)
        .iter()
        .any(|event| matches!(event, SessionEvent::UserMessage { .. }))
}

async fn stop(live: Live) {
    let handle = live.control.runtime().clone();
    if in_turn(&live) {
        // 取消失败不挡停:停本身也会把回合带走,取消只是让它以「被打断」落盘。
        let _ = handle.cancel().await;
    }
    let _ = handle.shutdown().await;
}

impl Background {
    fn front_control(&self) -> Arc<RuntimeControl> {
        self.state
            .lock()
            .expect("background poisoned")
            .front
            .control
            .clone()
    }

    /// 一个 runtime 的事件到了。
    fn arrived(&self, id: u64, event: AgentEvent, changed: bool) {
        let front = {
            let state = self.state.lock().expect("background poisoned");
            if state.front.id == id {
                if state.gate {
                    let _ = self.screen.send(event);
                }
                true
            } else {
                false
            }
        };
        if changed && !front {
            self.announce_list();
        }
    }

    /// 一个 runtime 的宿主事件到了。只有前台那个的才是屏幕上那个会话的事。
    fn announced(&self, id: u64, event: HostEvent) {
        let front = self.state.lock().expect("background poisoned").front.id == id;
        if front {
            self.announce(event);
        }
    }

    fn announce(&self, event: HostEvent) {
        self.watchers
            .lock()
            .expect("watchers poisoned")
            .retain(|watcher| watcher.send(event.clone()).is_ok());
    }

    fn list(&self) -> Vec<BackgroundSession> {
        let state = self.state.lock().expect("background poisoned");
        state.slots.iter().map(describe).collect()
    }

    fn announce_list(&self) {
        let sessions = self.list();
        self.announce(HostEvent::BackgroundChanged { sessions });
    }

    /// 屏幕发来一条命令。
    async fn route(&self, command: AgentCommand) {
        /// 这条命令要这里做点什么,照它不带回执时的样子看。
        enum Kind {
            Unsubscribe,
            Subscribe { session: String, from: u64 },
            Respond { id: u64 },
            Send,
            Shutdown,
            Other,
        }
        let kind = match &command {
            AgentCommand::Tagged { command, .. } => match command.as_ref() {
                AgentCommand::Unsubscribe { .. } => Kind::Unsubscribe,
                AgentCommand::Respond { id, .. } => Kind::Respond { id: *id },
                AgentCommand::SendMessage { .. } | AgentCommand::SendMessageWithContext { .. } => {
                    Kind::Send
                }
                // 带回执的订阅与退出照常转:回执要那一个 runtime 答。
                _ => Kind::Other,
            },
            AgentCommand::Unsubscribe { .. } => Kind::Unsubscribe,
            AgentCommand::Subscribe { session, from } => Kind::Subscribe {
                session: session.clone(),
                from: *from,
            },
            AgentCommand::Respond { id, .. } => Kind::Respond { id: *id },
            AgentCommand::SendMessage { .. } | AgentCommand::SendMessageWithContext { .. } => {
                Kind::Send
            }
            AgentCommand::Shutdown => Kind::Shutdown,
            _ => Kind::Other,
        };
        match kind {
            // 旧的那个会话在哪个 runtime 里,屏幕不知道——它刚被换走。都发一遍:
            // 不认识这个会话的 feed 什么也不做。
            Kind::Unsubscribe => {
                let state = self.state.lock().expect("background poisoned");
                for slot in &state.slots {
                    let _ = slot.commands.send(command.clone());
                }
                let _ = state.front.commands.send(command);
            }
            Kind::Subscribe { session, from } => {
                let (commands, pending) = {
                    let mut state = self.state.lock().expect("background poisoned");
                    let lead = state.front.control.session_id() == session;
                    if lead {
                        state.gate = true;
                    }
                    let waiting = lead
                        && state.front.control.runtime().status().phase
                            == RuntimePhase::WaitingApproval;
                    let pending = waiting
                        .then(|| {
                            state
                                .front
                                .track
                                .lock()
                                .expect("track poisoned")
                                .pending
                                .clone()
                        })
                        .flatten();
                    (
                        state.front.commands.clone(),
                        pending.map(|pending| (state.front.control.clone(), pending)),
                    )
                };
                let Some((control, pending)) = pending else {
                    let _ = commands.send(command);
                    return;
                };
                // 挂着一个问题:自己订阅,再把问题接在重放后面发——同一条流、同一个
                // 顺序,屏幕画问题时那条 `Asked` 已经在它的日志里了。
                let front_end = control.front_end();
                let subscribed = front_end
                    .app()
                    .map(|app| front_end.feed().subscribe_to(&app, &session, from));
                match subscribed {
                    Some(Ok(())) => {
                        let _ = front_end.events().send(pending);
                    }
                    _ => {
                        let _ = commands.send(command);
                    }
                }
            }
            Kind::Respond { id } => {
                let state = self.state.lock().expect("background poisoned");
                {
                    let mut track = state.front.track.lock().expect("track poisoned");
                    if matches!(&track.pending, Some(AgentEvent::Request { id: waiting, .. }) if *waiting == id)
                    {
                        track.pending = None;
                    }
                }
                let _ = state.front.commands.send(command);
            }
            Kind::Send => {
                let state = self.state.lock().expect("background poisoned");
                state.front.track.lock().expect("track poisoned").ended = None;
                let _ = state.front.commands.send(command);
            }
            // 退出:前台照旧,后台的一个个停下来(`tui_front::run` 等它们停完)。
            Kind::Shutdown => {
                // Which session is in front, read **before** the stop goes out:
                // after it the App is gone and its log with it.
                self.record_exit_front();
                let front = self.front_commands();
                let _ = front.send(command);
                if let Some(me) = self.me.upgrade() {
                    tokio::spawn(async move { me.shutdown_all().await });
                }
            }
            Kind::Other => {
                let front = self.front_commands();
                let _ = front.send(command);
            }
        }
    }

    fn front_commands(&self) -> mpsc::UnboundedSender<AgentCommand> {
        self.state
            .lock()
            .expect("background poisoned")
            .front
            .commands
            .clone()
    }

    /// 停下每一个后台 runtime。退出时用;调多次无害。
    pub async fn shutdown_all(&self) {
        let _op = self.op.lock().await;
        // A screen that closed without saying so never sent the stop; the
        // front is still up, so it can still be read here.
        self.record_exit_front();
        let slots = std::mem::take(&mut self.state.lock().expect("background poisoned").slots);
        self.left.lock().expect("left poisoned").extend(
            slots
                .iter()
                .filter(|slot| has_conversation(slot))
                .map(|slot| slot.control.session_id()),
        );
        futures::future::join_all(slots.into_iter().map(stop)).await;
    }

    /// Note which session is in front as the screen goes, once: `/bg`,
    /// `/bg N`, `/resume` and `/session` have all moved it since start, and
    /// the line printed after exit is for this one. An empty one is noted as
    /// nothing to come back to.
    fn record_exit_front(&self) {
        let mut noted = self.exit_front.lock().expect("exit poisoned");
        if noted.is_some() {
            return;
        }
        let state = self.state.lock().expect("background poisoned");
        let session = state.front.control.session_id();
        *noted = Some((!session.is_empty() && has_conversation(&state.front)).then_some(session));
    }

    /// The background sessions the exit stopped. Each is saved and can be
    /// resumed; the launcher says how, the way it does for the foreground one.
    pub fn left_behind(&self) -> Vec<String> {
        self.left.lock().expect("left poisoned").clone()
    }

    /// The session that was in front when the screen went away, when anything
    /// was said in it.
    pub fn front_at_exit(&self) -> Option<String> {
        self.exit_front
            .lock()
            .expect("exit poisoned")
            .clone()
            .flatten()
    }

    fn busy(reason: String) -> HostError {
        HostError::Busy { reason }
    }

    /// 前台能不能换走。
    fn may_move(front: &Live) -> Result<(), HostError> {
        use atomcode_i18n::screen::{t as tr, Msg as SMsg};
        if crate::tui_share::sharing() {
            return Err(Self::busy(tr(SMsg::BgRefusedWhileSharing).into_owned()));
        }
        match front.control.runtime().status().phase {
            RuntimePhase::WaitingApproval => {
                Err(Self::busy(tr(SMsg::BgRefusedWhileAsking).into_owned()))
            }
            RuntimePhase::Reconfiguring => Err(Self::busy(
                tr(SMsg::BgRefusedWhileReconfiguring).into_owned(),
            )),
            _ => Ok(()),
        }
    }

    async fn spawn_at(&self, working_dir: PathBuf, shown: bool) -> Result<Live, HostError> {
        let spawned = (self.spawn)(working_dir)
            .await
            .map_err(|message| HostError::Failed { message })?;
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (live, pumps) = attach(id, spawned, self.host_config.clone(), shown)
            .map_err(|message| HostError::Failed { message })?;
        pumps.pump(self.me.clone());
        if live.control.session_id().is_empty() {
            stop(live).await;
            return Err(HostError::Failed {
                message: "the new runtime has no session".into(),
            });
        }
        Ok(live)
    }

    fn full() -> HostError {
        use atomcode_i18n::screen::{t as tr, Msg as SMsg};
        Self::busy(tr(SMsg::BgSlotsFull { most: MOST }).into_owned())
    }

    /// 把 `incoming` 放到前台。返回被换下来的那个。调用方持着 `op`。
    fn put_in_front(&self, state: &mut State, incoming: Live) -> Live {
        incoming.shown.store(true, Ordering::SeqCst);
        let outgoing = std::mem::replace(&mut state.front, incoming);
        outgoing.shown.store(false, Ordering::SeqCst);
        if in_turn(&outgoing) {
            // 正在跑的那个回合还没有结局,上一个回合的结局不算数了。
            outgoing.track.lock().expect("track poisoned").ended = None;
        }
        state.gate = false;
        crate::tui_share::remember(state.front.control.clone());
        outgoing
    }

    async fn background(&self, session: String) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        let working_dir = {
            let state = self.state.lock().expect("background poisoned");
            if state.front.control.session_id() != session {
                return Err(HostError::NotFound);
            }
            Self::may_move(&state.front)?;
            if state.slots.len() >= MOST {
                return Err(Self::full());
            }
            state.front.control.clone()
        }
        .working_dir_now()
        .await;
        // 新 runtime 起好之前不碰槽位表:起不来的话,前台原样不动。
        let fresh = self.spawn_at(working_dir, true).await?;
        let fresh_session = fresh.control.session_id();
        let slot = {
            let mut state = self.state.lock().expect("background poisoned");
            let outgoing = self.put_in_front(&mut state, fresh);
            state.slots.push(outgoing);
            state.slots.len() as u32
        };
        self.announce(HostEvent::SessionChanged {
            session: fresh_session,
            previous: Some(session.clone()),
        });
        self.announce_list();
        Ok(HostReply::Backgrounded { session, slot })
    }

    async fn foreground(&self, session: String, target: String) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        let closed = {
            let mut state = self.state.lock().expect("background poisoned");
            if state.front.control.session_id() != session {
                return Err(HostError::NotFound);
            }
            let at = state
                .slots
                .iter()
                .position(|slot| slot.control.session_id() == target)
                .ok_or(HostError::NotFound)?;
            Self::may_move(&state.front)?;
            let keep = has_conversation(&state.front) || in_turn(&state.front);
            let incoming = state.slots.remove(at);
            let outgoing = self.put_in_front(&mut state, incoming);
            if keep {
                state.slots.insert(at, outgoing);
                None
            } else {
                Some(outgoing)
            }
        };
        self.announce(HostEvent::SessionChanged {
            session: target.clone(),
            previous: Some(session),
        });
        self.announce_list();
        if let Some(empty) = closed {
            stop(empty).await;
        }
        Ok(HostReply::SessionChanged { session: target })
    }

    async fn start(&self, text: String) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        let control = {
            let state = self.state.lock().expect("background poisoned");
            if state.slots.len() >= MOST {
                return Err(Self::full());
            }
            state.front.control.clone()
        };
        let working_dir = control.working_dir_now().await;
        let live = self.spawn_at(working_dir, false).await?;
        if let Err(error) = live.control.runtime().submit(UserInput::from(text)).await {
            // 没接下任务的会话不留槽:它只会是一行什么都不做的空会话。
            stop(live).await;
            return Err(crate::host::refused(error));
        }
        let session = live.control.session_id();
        let slot = {
            let mut state = self.state.lock().expect("background poisoned");
            state.slots.push(live);
            state.slots.len() as u32
        };
        self.announce_list();
        Ok(HostReply::Backgrounded { session, slot })
    }

    async fn tell(&self, target: String, text: String) -> Result<HostReply, HostError> {
        let (handle, track) = {
            let state = self.state.lock().expect("background poisoned");
            let slot = state
                .slots
                .iter()
                .find(|slot| slot.control.session_id() == target)
                .ok_or(HostError::NotFound)?;
            (slot.control.runtime().clone(), slot.track.clone())
        };
        handle
            .submit(UserInput::from(text))
            .await
            .map_err(crate::host::refused)?;
        track.lock().expect("track poisoned").ended = None;
        self.announce_list();
        Ok(HostReply::Done)
    }

    async fn drop_one(&self, target: String) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        let live = {
            let mut state = self.state.lock().expect("background poisoned");
            let at = state
                .slots
                .iter()
                .position(|slot| slot.control.session_id() == target)
                .ok_or(HostError::NotFound)?;
            state.slots.remove(at)
        };
        stop(live).await;
        self.announce_list();
        Ok(HostReply::Done)
    }

    fn in_background(&self, session: &str) -> bool {
        self.state
            .lock()
            .expect("background poisoned")
            .slots
            .iter()
            .any(|slot| slot.control.session_id() == session)
    }
}

#[async_trait]
impl HostControl for Background {
    async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
        match command {
            HostCommand::Background { session } => self.background(session).await,
            HostCommand::BackgroundSessions => Ok(HostReply::BackgroundSessions {
                sessions: self.list(),
            }),
            HostCommand::Foreground { session, target } => self.foreground(session, target).await,
            HostCommand::StartBackground { text } => self.start(text).await,
            HostCommand::TellBackground { target, text } => self.tell(target, text).await,
            HostCommand::DropBackground { target } => self.drop_one(target).await,
            // 恢复一个正在后台跑的会话,就是把它带回来:租约本来就不让同一个会话
            // 开两份,与其答「在用」,不如照人的意思做。
            HostCommand::Resume { session, target } if self.in_background(&target) => {
                self.foreground(session, target).await
            }
            other => {
                let front = self.front_control();
                front.call(other).await
            }
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<HostEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.watchers.lock().expect("watchers poisoned").push(tx);
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回合怎么结束的,面板就怎么分组——被取消的不算失败,出错的要人看。
    #[test]
    fn how_a_turn_ended_is_what_the_panel_says() {
        let mut track = Track::default();
        let ended = |reason| AgentEvent::TurnComplete { turn: None, reason };
        track.saw(&ended(StopReason::Cancelled));
        assert_eq!(track.ended, Some(BackgroundState::Cancelled));
        track.saw(&ended(StopReason::ProviderError));
        assert_eq!(track.ended, Some(BackgroundState::Failed));
        track.saw(&ended(StopReason::Stopped));
        assert_eq!(track.ended, Some(BackgroundState::Done));
    }

    /// 回合结束,挂着的问题就不再挂着——回到前台时不该再问一次已经作废的问题。
    #[test]
    fn a_question_is_forgotten_when_its_turn_ends() {
        let mut track = Track::default();
        track.saw(&AgentEvent::Request {
            id: 7,
            kind: "approval".into(),
            payload: serde_json::Value::Null,
        });
        assert!(track.pending.is_some());
        track.saw(&AgentEvent::TurnComplete {
            turn: None,
            reason: StopReason::Stopped,
        });
        assert!(track.pending.is_none());
    }

    #[test]
    fn one_line_takes_the_first_line_with_words() {
        assert_eq!(one_line("\n  \n  改完了 \n第二行"), Some("改完了".into()));
        assert_eq!(one_line("   "), None);
    }
}
