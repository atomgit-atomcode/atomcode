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
use atomcode_capabilities::session::manager::SessionOrigin;
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::runtime::{RuntimePhase, UserInput};
use atomcode_coding::{CodingAgentConfig, CodingRuntime};
use atomcode_harness::feed::Feed;
use atomcode_host_api::{
    BackgroundSession, BackgroundState, BackgroundStats, HostCommand, HostConnection, HostControl,
    HostError, HostEvent, HostReply, LoggedFact,
};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::session::{LoggedEvent, SessionEvent};
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

/// 让前台那段对话里的 `code_review` 和 `/review` 走同一条路:开一个后台会话去审,
/// 调用立刻返回,审完结果投回来(`deliver_home`)。
///
/// 端口得在 runtime 建起来**之前**交给它(`PrepareOptions::review_delegate`),而
/// [`Background`] 要等屏幕接上才有 —— 所以是一个晚绑定的格子:启动器建一个,前台
/// runtime 与 `/bg` 起的每个 runtime 都拿它造自己的端口,[`connect`] 之后
/// [`Self::bind`]。没绑上、`Background` 已经没了,都是「不在这里」:就地审。
///
/// 只存 `Weak`:`Background` → `Live` → runtime → 工具 → 端口,存强引用就是一个环。
#[derive(Clone, Debug, Default)]
pub struct ReviewHome(Arc<Mutex<Weak<Background>>>);

impl ReviewHome {
    pub fn new() -> Self {
        Self::default()
    }

    /// 屏幕接上了:从现在起,前台的审查交给它。
    pub fn bind(&self, background: &Arc<Background>) {
        *self.0.lock().expect("review home poisoned") = Arc::downgrade(background);
    }

    /// 一个 runtime 的端口。认的是它起的时候带的那个前端 —— 前台换了人(`/bg`
    /// 把它挪走了),它就不再是替谁开后台会话的那一个。
    pub fn delegate_for(
        &self,
        front_end: &Arc<FrontEnd>,
    ) -> Arc<dyn atomcode_review::ReviewDelegate> {
        Arc::new(ReviewElsewhere {
            home: self.clone(),
            front_end: Arc::downgrade(front_end),
        })
    }
}

#[derive(Debug)]
struct ReviewElsewhere {
    home: ReviewHome,
    front_end: Weak<FrontEnd>,
}

#[async_trait]
impl atomcode_review::ReviewDelegate for ReviewElsewhere {
    async fn delegate(
        &self,
        working_dir: &std::path::Path,
        task: String,
        scope: Option<String>,
    ) -> Option<Result<atomcode_review::DelegatedReview, String>> {
        let background = self
            .home
            .0
            .lock()
            .expect("review home poisoned")
            .upgrade()?;
        let front_end = self.front_end.upgrade()?;
        match background
            .start_for(
                &front_end,
                working_dir.to_path_buf(),
                task,
                scope,
                Some("code-review".to_string()),
            )
            .await
        {
            Ok(None) => None,
            Ok(Some(HostReply::Backgrounded { slot, files, .. })) => {
                Some(Ok(atomcode_review::DelegatedReview { slot, files }))
            }
            Ok(Some(other)) => Some(Err(format!("{other:?}"))),
            Err(error) => Some(Err(format!("{error:?}"))),
        }
    }
}

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
    /// 是替哪个会话干活的(会话 id)。等于它自己 = 不是替谁干活,没有第二个读者 ——
    /// `/bg` 把当前会话放到后台接着跑就是这种。
    origin: String,
    /// 替别的对话干活的会话,在盘上标没标、标到了哪一步。见 [`Mark`]。
    mark: Arc<Mutex<Mark>>,
    /// 发起它的那一方给它起的名字(审查就叫 `code-review`)。有就用它,不等它自己
    /// 起名 —— 面板上那一行认的是「这是哪件活」。
    name: Option<String>,
    /// 交给它的那件事的第一行。日志里还没有它说的第一句话时(刚起、还没落盘),
    /// 面板那一列靠它认,而不是一串 id。
    task: Option<String>,
    /// 上一次描述它时的样子,和那时日志有几条、它在什么状态。见 [`describe`]。
    described: Arc<Mutex<Option<Described>>>,
}

/// 一个后台会话上一次被描述成的样子。
///
/// 列表在有会话在跑时每秒报一次,而描述一个会话要把它整份日志拷出来读 —— 一次审查
/// 的日志带着工具输出,动辄几 MB,每秒乘以槽位数。日志条数和状态都没变,上次那份
/// 就还是对的,只有在跑的那个用时要量到此刻。
#[derive(Clone)]
struct Described {
    len: usize,
    state: BackgroundState,
    /// 在等的那个请求:换了一个问题而日志一条没多(不是每种问法都先记一条),
    /// 那一行要说的话也换了。
    pending: Option<u64>,
    session: BackgroundSession,
    /// 日志第一条的时刻:在跑的用时从它量起。
    first_at: Option<u64>,
}

/// 一个替别的对话干活的后台会话,在盘上怎么记。
///
/// 结果**真的落进**发起它的那段对话的日志之后,才标成 `SessionOrigin::Delegated`
/// (不进 /resume、--continue)。在那之前它就是个普通会话:还在跑、没投到、投了但那段
/// 对话还没收下就退出了、或者进程崩了 —— 它都得在 /resume 里、或退出那一行里找得回来。
/// 投递成功只说明「排上了队」:那段对话正忙时它是一次 steer,退出时可能被丢掉。
#[derive(Debug)]
enum Mark {
    /// 不是替谁干活(`/bg` 挪过来的那种),没什么要标的。
    Own,
    /// 替别的对话干活,结果还没落进那段对话。`PathBuf` 是它的会话库所在的项目。
    Pending(PathBuf),
    /// 结果已在那段对话里,盘上标成了 `Delegated`。
    Home(PathBuf),
    /// 人接手了(叫到前台、对它说话):它是人的对话了,不再标。
    TakenUp,
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

/// 这个范围里有多少个文件在变。
///
/// `None` = 这里问不出来(不在 git 仓库里、没有那个 base、或 git 不在):那就不说数,
/// 而不是说一个谁都没量过的 0。`scope` 就是 `/review` 自己的词汇 —— 一行说"范围是
/// 未提交的改动",另一行说"X 个文件",两处不能各算一半。
fn changed_files(dir: &std::path::Path, scope: &str) -> Option<usize> {
    let mut git = std::process::Command::new("git");
    git.current_dir(dir);
    match scope {
        "staged" => {
            git.args(["diff", "--cached", "--name-only"]);
        }
        "working_tree" => {
            // `status`,不是 `diff`:这次还没提交的包括还没被 git 看见的那些新文件。
            git.args(["status", "--porcelain"]);
        }
        // `-` 开头的是 git 的选项,不是 ref:`/review --output=x` 会让 git 写一个
        // 文件。TUI 已经不把它当范围,这里再挡一次 —— 这条命令收的是任何宿主。
        // 挡在这里而不是加 `--end-of-options`:那是 git 2.24 才有的,更老的 git
        // 会把整条命令拒掉,连正常的 ref 也量不出来。
        base if base.starts_with('-') => return None,
        base => {
            let range = format!("{base}..HEAD");
            git.args(["diff", "--name-only", &range]);
        }
    }
    let out = git.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().filter(|line| !line.trim().is_empty()).count())
}

/// 一个回合结束后,这个会话该被算成什么状态。
///
/// 一处判断两处用:面板那一行(`Track::saw`)与"要不要把结果投回去"看的是同一件事
/// —— 被中断和出错都不是一次结果,投回去只会让人读一段没有结论的话。
///
/// 只有模型自己说完(`Stopped`)才算干完。被熔断(`RepeatLoop` / `ToolLoopDetected` /
/// `RunawayFuse`)、轮数用尽、被策略拦下的,最后那句话往往只是"我来调一下 X"之类的开场白,
/// 不是结论 —— 当成结果投回去,那段对话会被一句没说完的话唤醒。`StopReason` 是
/// `non_exhaustive`,以后新加的结束方式也先算没干完,等有人看过再放进来。
fn ended_state(reason: &StopReason) -> BackgroundState {
    match reason {
        StopReason::Stopped => BackgroundState::Done,
        StopReason::Cancelled => BackgroundState::Cancelled,
        _ => BackgroundState::Failed,
    }
}

/// 最后一个回合说出的结论:最后一个 `TurnStart` 之后、最后一条有字的回复。
///
/// 只看最后一个回合:整份日志里倒着找,一个没出字的回合会把上一回合的旧答案再投一次。
fn last_turn_answer(log: &[LoggedEvent]) -> Option<String> {
    let start = log
        .iter()
        .rposition(|logged| matches!(logged.event, SessionEvent::TurnStart { .. }))
        .unwrap_or(0);
    log[start..]
        .iter()
        .rev()
        .find_map(|logged| match &logged.event {
            SessionEvent::AssistantMessage { text, .. } if !text.trim().is_empty() => {
                Some(text.trim().to_string())
            }
            _ => None,
        })
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
                self.ended = Some(ended_state(reason));
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
    // 后台还有活在跑,就每秒把列表再报一次。事件只在「在等人 / 回合完了」时报,
    // 而面板那一行要跟着走的 —— 它起的名字、说到哪了、用了多久、上下文多大 ——
    // 全在两者之间变:不报,面板就一直停在刚放进后台那一刻(名字是一串 id、没有
    // 用时、没有 token)。没人看(没有订阅者)或没有在跑的,就不报。
    {
        let weak = Arc::downgrade(&background);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(background) = weak.upgrade() else {
                    break;
                };
                if background.someone_running() {
                    background.announce_list();
                }
            }
        });
    }
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
        // Only the session the screen opens on is asked `Readiness` at start.
        id == 0,
    )?;
    let HostConnection {
        commands, events, ..
    } = connection;
    let track = Arc::new(Mutex::new(Track::default()));
    let said = control.subscribe();
    let own = control.session_id();
    Ok((
        Live {
            id,
            control,
            commands,
            shown: flag,
            track: track.clone(),
            since: now_ms(),
            origin: own,
            mark: Arc::new(Mutex::new(Mark::Own)),
            name: None,
            task: None,
            described: Arc::new(Mutex::new(None)),
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
    let (ended, pending) = {
        let track = live.track.lock().expect("track poisoned");
        (track.ended, track.pending.is_some())
    };
    match handle.status().phase {
        // 阶段先翻、pump 记下 `pending` 后到(`Track::saw` 在 `Request` 事件送到时
        // 才写):这一小段里还不能说「在等」——不然报出去的 `Waiting` 没有请求可答,
        // `BackgroundQuestion` 只会落回 `NotFound`。等 `pending` 落地那一刻,
        // `Track::saw` 自己会返回 changed=true 让 `announce_list` 立刻把它翻过来,
        // 不会漏报,只是晚一拍。
        RuntimePhase::WaitingApproval if pending => BackgroundState::Waiting,
        RuntimePhase::Failed | RuntimePhase::Stopped | RuntimePhase::ShuttingDown => {
            BackgroundState::Failed
        }
        // 回合结束的事件先到、阶段后落的那一小段里,信事件;`WaitingApproval` 但
        // `pending` 还没落地的那一小段也按这条走。
        RuntimePhase::InTurn | RuntimePhase::WaitingApproval => {
            ended.unwrap_or(BackgroundState::Running)
        }
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
///
/// 连每条事实的提交时刻一起带回来(`LoggedEvent::at`):面板那一行摘要只看事件本身,
/// 但「这次活干多久」要看首尾两个时刻 —— 而那是**这次活**的跨度,不是这个槽位开了
/// 多久(它可能先空着,也可能中途在等一个回答)。
fn log_of(live: &Live) -> Vec<LoggedEvent> {
    log_of_control(&live.control)
}

/// 它在等的那个请求的 id。
fn pending_id(live: &Live) -> Option<u64> {
    match live.track.lock().expect("track poisoned").pending.as_ref() {
        Some(AgentEvent::Request { id, .. }) => Some(*id),
        _ => None,
    }
}

/// 这个会话的日志有几条 —— 不拷日志。
fn log_len(live: &Live) -> usize {
    let session = live.control.session_id();
    live.control
        .front_end()
        .app()
        .and_then(|app| Feed::find(&app, &session))
        .map_or(0, |agent| agent.session().len())
}

fn log_of_control(control: &RuntimeControl) -> Vec<LoggedEvent> {
    let session = control.session_id();
    control
        .front_end()
        .app()
        .and_then(|app| Feed::find(&app, &session))
        .map(|agent| agent.session().events())
        .unwrap_or_default()
}

fn current_request(log: &[LoggedEvent]) -> Option<String> {
    let turn = log.iter().rev().find_map(|logged| match logged.event {
        SessionEvent::TurnStart { turn } => Some(turn),
        _ => None,
    })?;
    log.iter().rev().find_map(|logged| match &logged.event {
        SessionEvent::UserMessage {
            turn: said_turn,
            text,
            ..
        } if *said_turn == turn => Some(text.clone()),
        _ => None,
    })
}

/// 画一个问询要看的日志尾巴有多长。
///
/// `question_for` 取的是**最新的那条**匹配(`crates/atomcode-tui/src/ask.rs`),
/// 所以尾巴够。64 是预算,不是契约:真要更长的问法,先量再改。
const ASKED_TAIL: usize = 64;

/// 日志的最后 [`ASKED_TAIL`] 条,照过线的形状。
///
/// 搬的是事实本身,不解释:把载荷变成问题是屏幕自己的事(`crate::ask`)——
/// 这一层不认识「问询」这个词。
fn tail_of(log: &[LoggedEvent]) -> Vec<LoggedFact> {
    log[log.len().saturating_sub(ASKED_TAIL)..]
        .iter()
        .map(|logged| LoggedFact {
            seq: logged.seq,
            at: logged.at,
            event: logged.event.clone(),
        })
        .collect()
}

/// 这个后台会话叫什么:它自己起的名字,还没起名就用它第一句话。
fn name_of(log: &[LoggedEvent]) -> String {
    log.iter()
        .rev()
        .find_map(|logged| match &logged.event {
            SessionEvent::Titled { title, .. } if !title.trim().is_empty() => Some(title.clone()),
            _ => None,
        })
        .or_else(|| {
            log.iter().find_map(|logged| match &logged.event {
                SessionEvent::UserMessage { text, .. } => one_line(text),
                _ => None,
            })
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

/// 这次活花掉的:从它自己的日志折出来,和本机那条回合汇总(`content::TurnStats`)同一
/// 套口径 —— 花费与命中率按**每次请求累加**(`sent` / `sent_cached`,每次请求都整段计费,
/// 前缀只在命中时便宜);`prompt` / `cached` 仍是最后一次请求,说的是上下文现在多大。
/// 「出」是每轮新做的工作,累加。
///
/// 一条请求都没发过、也没在跑,就 `None`:没有可说的数,画出来只会是「0 轮  0 工具」。
/// 在跑的,先只有用时。
///
/// `running_at`:还在跑时的此刻(unix 毫秒)。那时用时量到此刻,而不是最后一条事实 ——
/// 模型安静地想一分钟,面板上的用时不该停在一分钟前。
fn stats_of(log: &[LoggedEvent], running_at: Option<u64>) -> Option<BackgroundStats> {
    let mut steps = 0u32;
    let mut tools = 0u32;
    let mut completion = 0u32;
    let mut sent = 0u64;
    let mut sent_cached = 0u64;
    let mut last: Option<atomcode_kernel::stream::TokenUsage> = None;
    for logged in log {
        match &logged.event {
            SessionEvent::StepEnd { tool_calls, .. } => {
                steps += 1;
                tools = tools.saturating_add(*tool_calls);
            }
            SessionEvent::Usage { usage, .. } => {
                completion = completion.saturating_add(usage.completion);
                sent += u64::from(usage.prompt);
                sent_cached += u64::from(usage.cached);
                last = Some(*usage);
            }
            _ => {}
        }
    }
    let elapsed_ms = match (log.first(), running_at.or(log.last().map(|last| last.at))) {
        (Some(first), Some(end)) => end.saturating_sub(first.at),
        _ => 0,
    };
    // 还在跑、第一个回答还没来(一次审查的第一个请求就可能要几十秒):没有可说的
    // token,但用时是有的 —— 面板那一行不该在这几十秒里什么都不说。
    let usage = match (last, running_at) {
        (Some(usage), _) => usage,
        (None, Some(_)) if elapsed_ms > 0 => atomcode_kernel::stream::TokenUsage::default(),
        (None, _) => return None,
    };
    Some(BackgroundStats {
        steps,
        tools,
        prompt: usage.prompt,
        cached: usage.cached,
        completion,
        elapsed_ms,
        sent,
        sent_cached,
    })
}

fn describe(live: &Live) -> BackgroundSession {
    let state = state_of(live);
    let len = log_len(live);
    let running = matches!(state, BackgroundState::Running | BackgroundState::Waiting);
    let pending = pending_id(live);
    let cached = live.described.lock().expect("described poisoned").clone();
    if let Some(cached) =
        cached.filter(|c| c.len == len && c.state == state && c.pending == pending)
    {
        let mut session = cached.session;
        if let (true, Some(first)) = (running, cached.first_at) {
            let elapsed_ms = now_ms().saturating_sub(first);
            match session.stats.as_mut() {
                Some(stats) => stats.elapsed_ms = elapsed_ms,
                None if elapsed_ms > 0 => {
                    session.stats = Some(BackgroundStats {
                        elapsed_ms,
                        ..BackgroundStats::default()
                    })
                }
                None => {}
            }
        }
        return session;
    }
    let log = log_of(live);
    let session = describe_from_log(live, state, &log);
    *live.described.lock().expect("described poisoned") = Some(Described {
        // What was read, not what `len` said a moment ago: a fact that landed in
        // between is in this description, and the next tick must not skip it.
        len: log.len(),
        state,
        pending,
        session: session.clone(),
        first_at: log.first().map(|first| first.at),
    });
    session
}

fn describe_from_log(
    live: &Live,
    state: BackgroundState,
    log: &[LoggedEvent],
) -> BackgroundSession {
    let title = log.iter().rev().find_map(|logged| match &logged.event {
        SessionEvent::Titled { title, .. } if !title.trim().is_empty() => Some(title.clone()),
        _ => None,
    });
    let asked = || {
        log.iter().rev().find_map(|logged| match &logged.event {
            SessionEvent::Asked { question, .. } => one_line(&question.prompt),
            _ => None,
        })
    };
    let said = || {
        log.iter().rev().find_map(|logged| match &logged.event {
            SessionEvent::AssistantMessage { text, .. }
            | SessionEvent::PartialReply { text, .. } => one_line(text),
            _ => None,
        })
    };
    let first_words = || {
        log.iter().find_map(|logged| match &logged.event {
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
        // 发起方起的名字优先;再是它自己起的;还没起名的,用它的第一句话 —— 日志里
        // 还没有时,用交给它的那件事的第一行。面板那一列总得有点什么可认,而一串
        // id 前缀认不出是哪件活。
        title: live
            .name
            .clone()
            .or(title)
            .or_else(first_words)
            .or_else(|| live.task.clone()),
        state,
        created_at: live.since,
        last,
        stats: stats_of(
            &log,
            matches!(state, BackgroundState::Running | BackgroundState::Waiting).then(now_ms),
        ),
        // 替谁干活:等于它自己(`/bg` 把那段对话挪过来的)就是没有别的读者。
        origin: (!live.origin.is_empty() && live.origin != live.control.session_id())
            .then(|| live.origin.clone()),
    }
}

/// Record how `session` was created in its index, in the store of the project
/// at `dir`.
fn set_origin(dir: &std::path::Path, session: &str, origin: SessionOrigin) -> Result<(), String> {
    atomcode_capabilities::session::SessionManager::for_project(
        dir,
        &atomcode_coding::config::product_dirs_from_env(),
    )
    .update_meta(session, |meta| meta.origin = origin)
    .map_err(|error| error.to_string())
}

/// A person took up a session that was working for another conversation: from
/// now on it is one of theirs, offered in `/resume` like any other, and never
/// marked again. Called without the slot table's lock: it may write the index.
fn take_up(mark: &Mutex<Mark>, session: &str) {
    let mut mark = mark.lock().expect("mark poisoned");
    if let Mark::Home(dir) = &*mark {
        if let Err(error) = set_origin(dir, session, SessionOrigin::Manual) {
            tracing::warn!(%session, %error, "a background session taken up is still left out of /resume");
        }
    }
    if !matches!(*mark, Mark::Own) {
        *mark = Mark::TakenUp;
    }
}

/// The result of `session` is in `home`'s log: `home` took the note in.
fn holds_note_from(home: &RuntimeControl, session: &str) -> bool {
    use atomcode_kernel::session::InjectionOrigin;
    log_of_control(home).iter().any(|logged| {
        matches!(
            &logged.event,
            SessionEvent::Injected {
                origin: InjectionOrigin::Peer { from, .. },
                ..
            } if from == session
        )
    })
}

/// The result of `session` reached the conversation it worked for: mark it on
/// disk as that conversation's, not one of the person's own. Only from
/// [`Mark::Pending`] — one a person took up in the meantime stays theirs.
fn came_home_now(mark: &Mutex<Mark>, session: &str) {
    let mut mark = mark.lock().expect("mark poisoned");
    let Mark::Pending(dir) = &*mark else {
        return;
    };
    match set_origin(dir, session, SessionOrigin::Delegated) {
        Ok(()) => *mark = Mark::Home(dir.clone()),
        Err(error) => {
            tracing::warn!(%session, %error, "a background session whose result came home is still offered in /resume")
        }
    }
}

/// Whether the exit has nothing to say about this background session: it
/// worked for another conversation, its result is there, and it is marked so.
/// Anything else — still running, not taken in, taken up by a person — gets
/// its line.
fn came_home(live: &Live) -> bool {
    matches!(*live.mark.lock().expect("mark poisoned"), Mark::Home(_)) && !in_turn(live)
}

/// How long a background session's result is watched for, once handed over,
/// to see it taken in: half-second looks, ten minutes in all — a conversation
/// in a long tool call takes the note in when that call returns.
const SETTLE_TRIES: u32 = 1_200;

/// 一个会话里有没有人说过话。没有的话它不值得占一个槽位。
fn has_conversation(live: &Live) -> bool {
    log_of(live)
        .iter()
        .any(|logged| matches!(logged.event, SessionEvent::UserMessage { .. }))
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
        // 替别的会话干活的那个,干完了把结果投回去 —— 内容落在发起它的那段对话里。
        // 这是"结果回来了"在那段对话里的形状。**只投内容**:要不要接着逐条核实,是那段
        // 对话看了上下文自己决定的事,不由这里替它下令。
        if let AgentEvent::TurnComplete { reason, .. } = &event {
            if ended_state(reason) == BackgroundState::Done {
                self.deliver_home(id);
            }
        }
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

    /// 把一个刚干完的后台会话说的话,投回发起它的那个会话。
    ///
    /// 只在"它是替别的会话干活"时才投(`origin` 不是它自己):`/bg` 把当前会话放到
    /// 后台接着跑,它本来就是那个会话,没有第二个读者。
    ///
    /// 投的是**内容**而不是一句通知。发起时说的是"结果回来后我会逐条核实",而那次
    /// 核实要真发生,就得让那段对话拿到结果 —— "去 /bg 读"是把这件事留给了一个人。
    /// 指令不跟着投:核实与否由那段对话自己定(`Msg::BackgroundResult`)。
    fn deliver_home(&self, id: u64) {
        use atomcode_i18n::screen::{t as tr, Msg as SMsg};
        let (origin, sender, frame, mark) = {
            let state = self.state.lock().expect("background poisoned");
            let Some(live) = state.slots.iter().find(|slot| slot.id == id) else {
                return;
            };
            if live.origin.is_empty() || live.origin == live.control.session_id() {
                return;
            }
            let log = log_of(live);
            let Some(answer) = last_turn_answer(&log) else {
                return;
            };
            let title = name_of(&log);
            let frame = tr(SMsg::BackgroundResult {
                title: &title,
                answer: &answer,
            })
            .into_owned();
            (
                live.origin.clone(),
                live.control.session_id(),
                frame,
                live.mark.clone(),
            )
        };
        // 发起它的那个会话可能已经被丢了、或被换掉了:那就不投 —— 面板里还有它。
        let handle = {
            let state = self.state.lock().expect("background poisoned");
            let live = if state.front.control.session_id() == origin {
                Some(&state.front)
            } else {
                state
                    .slots
                    .iter()
                    .find(|slot| slot.control.session_id() == origin)
            };
            live.map(|live| live.control.clone())
        };
        let Some(home) = handle else { return };
        let handle = home.runtime().clone();
        tokio::spawn(async move {
            // 一条注,不是一次用户提交:日志里说话的是那个后台会话,不是人。屏上的内容
            // 一字不少 —— 变的只是**谁说的**。
            //
            // `Busy` 是那段对话正在收尾一次取消或一次策略介入,过一会儿就收得下:等一等
            // 再投,而不是把结果扔了。别的拒绝(runtime 没了、换了代)不重试 —— 那时
            // 这个 handle 背后未必还是发起它的那段对话;结果仍在 /bg 面板里。
            const TRIES: u32 = 10;
            for attempt in 1..=TRIES {
                match handle.note(sender.clone(), frame.clone()).await {
                    Ok(_) => {
                        // 排上队不等于收下了:那段对话正忙时这是一次 steer。等它真进了
                        // 那段对话的日志再标。等不到(退出了、那段对话没了)就不标 ——
                        // 退出前 `shutdown_all` 还会再看一眼。
                        for _ in 0..SETTLE_TRIES {
                            if holds_note_from(&home, &sender) {
                                came_home_now(&mark, &sender);
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        return;
                    }
                    Err(atomcode_coding::RuntimeError::Busy) if attempt < TRIES => {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    Err(error) => {
                        tracing::warn!(
                            origin = %origin,
                            from = %sender,
                            %error,
                            "background result not delivered to the conversation that started it; it is still in /bg"
                        );
                        return;
                    }
                }
            }
        });
    }

    /// Settle, before the exit reads them, the background sessions whose result
    /// was taken in by the conversation they worked for but not yet marked.
    fn settle_before_exit(&self) {
        let pending: Vec<(Arc<Mutex<Mark>>, String, Arc<RuntimeControl>)> = {
            let state = self.state.lock().expect("background poisoned");
            state
                .slots
                .iter()
                .filter(|slot| {
                    matches!(*slot.mark.lock().expect("mark poisoned"), Mark::Pending(_))
                })
                .filter_map(|slot| {
                    let home = std::iter::once(&state.front)
                        .chain(state.slots.iter())
                        .find(|live| live.control.session_id() == slot.origin)?;
                    Some((
                        slot.mark.clone(),
                        slot.control.session_id(),
                        home.control.clone(),
                    ))
                })
                .collect()
        };
        for (mark, session, home) in pending {
            if holds_note_from(&home, &session) {
                came_home_now(&mark, &session);
            }
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

    /// 有人在看,而且后台有活在跑(在跑一个回合,或在等人回答)。
    fn someone_running(&self) -> bool {
        if self.watchers.lock().expect("watchers poisoned").is_empty() {
            return false;
        }
        let state = self.state.lock().expect("background poisoned");
        state.slots.iter().any(in_turn)
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
        self.settle_before_exit();
        let slots = std::mem::take(&mut self.state.lock().expect("background poisoned").slots);
        // Work done for another conversation whose result is there leaves
        // nothing to come back to (and is not offered in /resume); one still
        // running, or whose result was never taken in, is named.
        self.left.lock().expect("left poisoned").extend(
            slots
                .iter()
                .filter(|slot| has_conversation(slot) && !came_home(slot))
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
        Ok(HostReply::Backgrounded {
            session,
            slot,
            // 这条路是把**当前**会话放到后台接着跑:没有新范围,也没有要量的东西。
            files: None,
        })
    }

    async fn foreground(&self, session: String, target: String) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        let (closed, mark) = {
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
            let mark = incoming.mark.clone();
            let outgoing = self.put_in_front(&mut state, incoming);
            let closed = if keep {
                state.slots.insert(at, outgoing);
                None
            } else {
                Some(outgoing)
            };
            (closed, mark)
        };
        take_up(&mark, &target);
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

    async fn start(&self, text: String, scope: Option<String>) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        let working_dir = {
            let state = self.state.lock().expect("background poisoned");
            state.front.control.clone()
        }
        .working_dir_now()
        .await;
        self.start_locked(working_dir, text, scope, None).await
    }

    /// [`Self::start`],替 `front_end` 那个 runtime —— 只在它**此刻**就是前台那个时。
    ///
    /// 模型在前台那段对话里调 `code_review` 时走这里(`ReviewHome`)。`Ok(None)` 是
    /// 「不在这里」:它已经被挪到后台了(那就不是替谁干活,就地审),或者别的操作正
    /// 拿着 `op`。后一种**不等**:这个调用发生在前台回合的一次工具调用里,而拿着 `op`
    /// 的可能正等这个回合停下(退出时的 `shutdown_all`)—— 在这里等就是互相等死。
    /// 就地审是能被取消的那条路。
    ///
    /// 目录由调用方给:它就是那次工具调用的目录。再去问前台 runtime 要,问的正是
    /// 那个正在跑这次调用的 runtime。
    async fn start_for(
        &self,
        front_end: &Arc<FrontEnd>,
        working_dir: PathBuf,
        text: String,
        scope: Option<String>,
        name: Option<String>,
    ) -> Result<Option<HostReply>, HostError> {
        let Ok(_op) = self.op.try_lock() else {
            return Ok(None);
        };
        let in_front = {
            let state = self.state.lock().expect("background poisoned");
            Arc::ptr_eq(state.front.control.front_end(), front_end)
        };
        if !in_front {
            return Ok(None);
        }
        // Only the input of the turn that delegated this job is its request.
        // An automatic turn must not borrow an unrelated earlier user message.
        let request = {
            let state = self.state.lock().expect("background poisoned");
            let log = log_of(&state.front);
            current_request(&log)
        };
        let text = match request {
            Some(request) => format!(
                "{request}\n\n{}",
                text.split_once('\n')
                    .map_or(text.as_str(), |(_, instructions)| instructions)
            ),
            None => text,
        };
        self.start_locked(working_dir, text, scope, name)
            .await
            .map(Some)
    }

    /// 起一个后台会话去做 `text`,替前台那个。调用方拿着 `op`。
    async fn start_locked(
        &self,
        working_dir: PathBuf,
        text: String,
        scope: Option<String>,
        name: Option<String>,
    ) -> Result<HostReply, HostError> {
        let control = {
            let state = self.state.lock().expect("background poisoned");
            if state.slots.len() >= MOST {
                return Err(Self::full());
            }
            state.front.control.clone()
        };
        // 说得出范围的(审查就是),顺手把"这次有几个文件在变"算出来 —— 那行要说它。
        let files = scope
            .as_deref()
            .and_then(|scope| changed_files(&working_dir, scope));
        let mut live = self.spawn_at(working_dir.clone(), false).await?;
        // 它是替**前台那个**干的:干完把结果投回去,而不是留在这里等人来读。
        live.origin = control.session_id();
        // 结果落进那段对话之后,盘上才标它不是人的一段对话(见 `Mark`)。
        live.mark = Arc::new(Mutex::new(Mark::Pending(working_dir)));
        live.name = name;
        live.task = one_line(&text);
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
        Ok(HostReply::Backgrounded {
            session,
            slot,
            files,
        })
    }

    async fn tell(&self, target: String, text: String) -> Result<HostReply, HostError> {
        let (handle, track, mark) = {
            let state = self.state.lock().expect("background poisoned");
            let slot = state
                .slots
                .iter()
                .find(|slot| slot.control.session_id() == target)
                .ok_or(HostError::NotFound)?;
            (
                slot.control.runtime().clone(),
                slot.track.clone(),
                slot.mark.clone(),
            )
        };
        take_up(&mark, &target);
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

    /// 那个会话此刻挂着的问询。没在等人就没有可说的。
    ///
    /// 不猜、也不编:列表那一行已经说了它在干什么,这里再说一遍是噪音;而屏幕
    /// 只在它自己认为那个会话在等人时才会来要,所以这两条 `NotFound` 都是竞态的护栏。
    fn question(&self, target: String) -> Result<HostReply, HostError> {
        let state = self.state.lock().expect("background poisoned");
        let live = state
            .slots
            .iter()
            .find(|slot| slot.control.session_id() == target)
            .ok_or(HostError::NotFound)?;
        let (id, kind, payload) = {
            let track = live.track.lock().expect("track poisoned");
            match track.pending.clone() {
                Some(AgentEvent::Request { id, kind, payload }) => (id, kind, payload),
                _ => return Err(HostError::NotFound),
            }
        };
        Ok(HostReply::BackgroundQuestion {
            session: live.control.session_id(),
            id,
            kind,
            payload,
            facts: tail_of(&log_of(live)),
        })
    }

    /// 把那个后台会话挂着的那个请求答了。答不进去就明说。
    ///
    /// 答案走的是**那个 runtime 自己的命令通道**(`Live.commands`),与屏幕答自己那个
    /// 会话时走的 `AgentCommand::Respond` 是同一条路 —— 变的只是它去的 runtime。
    fn answer(
        &self,
        target: String,
        id: atomcode_kernel::event::RequestId,
        value: serde_json::Value,
    ) -> Result<HostReply, HostError> {
        use atomcode_i18n::screen::{t as tr, Msg as SMsg};
        let sent = {
            let state = self.state.lock().expect("background poisoned");
            let live = state
                .slots
                .iter()
                .find(|slot| slot.control.session_id() == target)
                .ok_or(HostError::NotFound)?;
            {
                let mut track = live.track.lock().expect("track poisoned");
                match track.pending.as_ref() {
                    Some(AgentEvent::Request { id: waiting, .. }) if *waiting == id => {
                        // 送不送得进去都清:送不进去是那个 runtime 已经没了,留着它只会让
                        // 列表继续说「在等」一个再也答不了的问题。
                        track.pending = None;
                    }
                    // 挂着的已经不是它了(作废、被别处答过、回合结束):拒绝,而不是把
                    // 人的答案安在别的问题上。
                    _ => return Err(Self::busy(tr(SMsg::BgAnswerStale).into_owned())),
                }
            }
            live.commands
                .send(AgentCommand::Respond { id, value })
                .is_ok()
        };
        // 它不再等了:列表要立刻跟着改,否则屏幕还当它在等 —— 提示行指着它,下一次
        // 提问询又挑中它、拿回 `NotFound`,排在后面的那个在等的会话就一直出不来。
        self.announce_list();
        if !sent {
            return Err(HostError::Failed {
                message: tr(SMsg::BgAnswerUndelivered).into_owned(),
            });
        }
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
            HostCommand::StartBackground { text, scope } => self.start(text, scope).await,
            HostCommand::TellBackground { target, text } => self.tell(target, text).await,
            HostCommand::DropBackground { target } => self.drop_one(target).await,
            HostCommand::BackgroundQuestion { target } => self.question(target),
            HostCommand::AnswerBackground { target, id, value } => self.answer(target, id, value),
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

    /// 范围里有几个文件在变,是真的去问 git。
    ///
    /// 三种范围各过一遍:这个数是给人读的("这次有 2 个文件在变"),编不出来。
    #[test]
    fn a_scope_counts_the_files_it_touches() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // 这台机器没有 git:没有可量的东西。
        }
        let dir = std::env::temp_dir().join(format!("atomcode-bg-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "one").expect("write");
        std::fs::write(dir.join("b.txt"), "two").expect("write");
        assert_eq!(
            changed_files(&dir, "working_tree"),
            Some(2),
            "两个还没提交的文件,包括 git 还没看见的那个"
        );
        git(&["add", "a.txt"]);
        assert_eq!(changed_files(&dir, "staged"), Some(1));
        git(&["add", "b.txt"]);
        git(&["commit", "-qm", "one"]);
        std::fs::write(dir.join("b.txt"), "two again").expect("write");
        git(&["add", "b.txt"]);
        git(&["commit", "-qm", "two"]);
        assert_eq!(
            changed_files(&dir, "HEAD~1"),
            Some(1),
            "已提交的那一段:HEAD~1..HEAD 只动了 b.txt"
        );
        // 一个选项不是 ref:交给 git 就成了 `--output=…`,它会去写那个文件。
        assert_eq!(changed_files(&dir, "--output=written"), None);
        assert!(!dir.join("written..HEAD").exists(), "git 没把它当选项");
        // 问不出来的地方说"问不出来",而不是一个谁都没量过的零。
        assert_eq!(
            changed_files(std::path::Path::new("/"), "working_tree"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // 被熔断、轮数用尽的不是一次结论 —— 不算干完,也就不会被投回去。
        for reason in [
            StopReason::RepeatLoop,
            StopReason::ToolLoopDetected,
            StopReason::MaxRounds,
            StopReason::RunawayFuse,
            StopReason::PolicyDenied,
        ] {
            track.saw(&ended(reason.clone()));
            assert_eq!(track.ended, Some(BackgroundState::Failed), "{reason:?}");
        }
    }

    /// 后台会话的花费按每次请求累加(`sent` / `sent_cached`),`prompt` / `cached`
    /// 仍是最后一次请求 —— 一个说花了多少,一个说上下文现在多大。
    #[test]
    fn background_cost_is_summed_over_requests_and_context_is_the_last() {
        let usage = |prompt, cached| LoggedEvent {
            seq: 0,
            at: 0,
            event: SessionEvent::Usage {
                turn: 1,
                round: 1,
                usage: atomcode_kernel::stream::TokenUsage {
                    prompt,
                    completion: 10,
                    cached,
                },
            },
        };
        let stats = stats_of(&[usage(1000, 0), usage(1200, 1100)], None).expect("ran");
        assert_eq!((stats.sent, stats.sent_cached), (2200, 1100));
        // A long-lived session's sums run past `u32::MAX` and keep counting.
        let long = stats_of(
            &[
                usage(3_000_000_000, 2_000_000_000),
                usage(3_000_000_000, 2_000_000_000),
            ],
            None,
        )
        .expect("ran");
        assert_eq!(
            (long.sent, long.sent_cached),
            (6_000_000_000, 4_000_000_000)
        );
        assert_eq!((stats.prompt, stats.cached), (1200, 1100));
        assert_eq!(stats.completion, 20);
    }

    /// 还在跑的,用时量到此刻 —— 模型安静地想一分钟,面板上的用时不该停在它最后
    /// 一条事实那里;跑完的,量到最后一条事实。
    #[test]
    fn a_running_background_session_s_time_runs_to_now() {
        let at = |at, event| LoggedEvent { seq: 0, at, event };
        let log = [
            at(1_000, SessionEvent::TurnStart { turn: 1 }),
            at(
                2_000,
                SessionEvent::Usage {
                    turn: 1,
                    round: 1,
                    usage: atomcode_kernel::stream::TokenUsage {
                        prompt: 100,
                        completion: 1,
                        cached: 0,
                    },
                },
            ),
        ];
        assert_eq!(stats_of(&log, None).unwrap().elapsed_ms, 1_000);
        assert_eq!(stats_of(&log, Some(61_000)).unwrap().elapsed_ms, 60_000);
    }

    /// 投回去的是最后一个回合的结论;最后一个回合没出字,就没有结论可投。
    #[test]
    fn the_answer_is_the_last_turns_own() {
        let logged = |event| LoggedEvent {
            seq: 0,
            at: 0,
            event,
        };
        let said = |turn: u64, text: &str| {
            logged(SessionEvent::AssistantMessage {
                turn,
                round: 0,
                text: text.into(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
                reasoning_blocks: Vec::new(),
                meta: None,
            })
        };
        let first = vec![
            logged(SessionEvent::TurnStart { turn: 1 }),
            said(1, "结论:两处要改"),
            said(1, "  "),
        ];
        assert_eq!(last_turn_answer(&first).as_deref(), Some("结论:两处要改"));

        // 第二个回合什么都没说:不能把第一回合的结论再投一次。
        let mut second = first.clone();
        second.push(logged(SessionEvent::TurnStart { turn: 2 }));
        second.push(said(2, ""));
        assert_eq!(last_turn_answer(&second), None);

        second.push(said(2, "补充:第三处也要改"));
        assert_eq!(
            last_turn_answer(&second).as_deref(),
            Some("补充:第三处也要改")
        );
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
    fn a_delegated_review_uses_only_its_current_turn_request() {
        let logged = |event| LoggedEvent {
            seq: 0,
            at: 0,
            event,
        };
        let mut log = vec![
            logged(SessionEvent::TurnStart { turn: 1 }),
            logged(SessionEvent::UserMessage {
                turn: 1,
                text: "审查 v5.2.2".into(),
                images: vec![],
            }),
        ];
        assert_eq!(current_request(&log).as_deref(), Some("审查 v5.2.2"));
        log.push(logged(SessionEvent::TurnStart { turn: 2 }));
        assert_eq!(
            current_request(&log),
            None,
            "an automatic turn has no user input to borrow"
        );
        log.push(logged(SessionEvent::UserMessage {
            turn: 2,
            text: "只看鉴权".into(),
            images: vec![],
        }));
        assert_eq!(current_request(&log).as_deref(), Some("只看鉴权"));
    }

    #[test]
    fn one_line_takes_the_first_line_with_words() {
        assert_eq!(one_line("\n  \n  改完了 \n第二行"), Some("改完了".into()));
        assert_eq!(one_line("   "), None);
    }
}
