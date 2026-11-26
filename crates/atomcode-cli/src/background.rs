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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use atomcode_capabilities::session::manager::SessionOrigin;
use atomcode_coding::front_end::FrontEnd;
use atomcode_coding::runtime::{RuntimePhase, UserInput};
use atomcode_coding::{CodingAgentConfig, CodingRuntime};
use atomcode_daemon::native_live::EmbeddedSessions as _;
use atomcode_harness::feed::Feed;
use atomcode_harness::session::SessionLog;
use atomcode_host_api::{
    BackgroundSession, BackgroundState, BackgroundStats, HostCommand, HostConnection, HostControl,
    HostError, HostEvent, HostReply, LoggedFact, ProviderUnavailableReason,
};
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::session::{LoggedEvent, SessionEvent};
use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::host::{find_switchable_entry, HostConfig, RuntimeControl};

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
    fn report_only(&self) -> bool {
        let Some(background) = self.home.0.lock().expect("review home poisoned").upgrade() else {
            return false;
        };
        let Some(front_end) = self.front_end.upgrade() else {
            return false;
        };
        let control = {
            let state = background.state.lock().expect("background poisoned");
            let Some(live) = std::iter::once(&state.front)
                .chain(&state.slots)
                .find(|live| {
                    Arc::ptr_eq(live.control.front_end(), &front_end)
                        && live.review_assignment.as_deref()
                            == Some(live.control.session_id().as_str())
                })
            else {
                return false;
            };
            if !matches!(*live.mark.lock().expect("mark poisoned"), Mark::TakenUp) {
                return true;
            }
            live.control.clone()
        };
        // Taking over the screen does not turn the in-flight assignment into
        // an ordinary chat turn. Only subsequent turns belong to the person.
        !read_log(&control, |events| {
            events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
        })
    }

    async fn delegate(
        &self,
        working_dir: &std::path::Path,
        task: String,
        scope: Option<String>,
        files: usize,
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
                files,
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
    review_assignment: Option<String>,
    control: Arc<RuntimeControl>,
    commands: mpsc::UnboundedSender<AgentCommand>,
    /// 这个会话的日志投影:面板的聚合、回合状态与缓存游标都在这里。泵每收到一个
    /// 事件就喂它(事实只从日志折入,见 [`LogProjection::absorb`]),`describe` 只读
    /// 字段。`Arc` 是因为泵与 [`Live`] 都要用。
    projection: Arc<Mutex<LogProjection>>,
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
    /// 是谁发起的:人(`/bg`、`/review`),还是这场对话里的一个工具(模型的
    /// `code_review`)。隐藏只认这面旗,不再认 `name` 里的字符串哨兵 -- 一个概念,
    /// 一处编码(issue #1821 #3)。
    initiated_by: InitiatedBy,
}

/// 谁起了一个后台会话。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum InitiatedBy {
    #[default]
    Person,
    /// 这场对话里的一个工具:它的结果回投给这场对话,未打开时不出现在 `/resume`。
    Tool,
}

/// The last fact read from one live log. A replacement App can rebuild the same
/// session id with an equal or longer history, so the sequence number alone is
/// insufficient. A sequence number, not a position: a settled round's chunks
/// leave the log, which moves every later fact's index.
#[derive(Default)]
struct LogCursor {
    log: Weak<SessionLog>,
    seen: u64,
}

impl LogCursor {
    fn read_new<R>(
        &mut self,
        log: &Arc<SessionLog>,
        read: impl FnOnce(bool, &[LoggedEvent]) -> R,
    ) -> R {
        log.read_events(|events| {
            let last = events.last().map_or(0, |logged| logged.seq);
            let reset = self.log.as_ptr() != Arc::as_ptr(log) || self.seen > last;
            let start = if reset {
                self.log = Arc::downgrade(log);
                0
            } else {
                events.partition_point(|logged| logged.seq <= self.seen)
            };
            let result = read(reset, &events[start..]);
            self.seen = last;
            result
        })
    }
}

#[derive(Default)]
struct LogProjection {
    cursor: LogCursor,
    summary: LogSummary,
    /// 正在整份重建(`seed`)。只有这时日志侧 `TurnEnd` 的 `error` 才写成面板的错。
    rebuilding: bool,
}

/// 一次要喂给投影的事实:日志里的一条,或运行时说出的一条。
///
/// 两条来源汇进同一个 [`LogProjection::absorb`],面板聚合与回合状态就不会有两套
/// 推导 -- 这也让 `attach` 的全量 seed 与之后的增量无论先后都得出同一份结果
/// (issue #1821 #12)。事实只认 `Logged` 这一种形状:运行时事件里也有 `Fact`,说的
/// 是同一件事,它不参与计数(理由见 [`LogProjection::absorb`])。
enum Observed<'a> {
    Logged(&'a LoggedEvent),
    Live(&'a AgentEvent),
}

/// 面板要说的一切,全部由 [`LogProjection::absorb`] 增量维护。
///
/// 一半来自日志(标题、第一句、花费、`TurnEnd` 的结局),一半来自运行时事件
/// (挂着的请求、错误);两边都只写这里,描述只读这里。
#[derive(Default)]
struct LogSummary {
    /// 折进来的日志事实条数。只有日志那一路(seed 与 `update`)推进它 —— 通道上
    /// 的 `Fact` 不算(见 [`LogProjection::absorb`])。它等于日志长度,就是「同一条
    /// 事实只折一次」的判据(`a_fact_on_the_channel_is_folded_once_with_its_log_record`)。
    len: u64,
    title: Option<String>,
    first_user: Option<String>,
    asked: Option<String>,
    last_said: Option<String>,
    has_user_message: bool,
    first_at: Option<u64>,
    last_at: u64,
    steps: u32,
    tools: u32,
    completion: u32,
    sent: u64,
    sent_cached: u64,
    usage: Option<atomcode_kernel::stream::TokenUsage>,
    /// 在等人回答的那个请求。不是日志里的事实:重订阅不会再发一次,所以由运行时
    /// 事件记在这里,回到前台时再投一次。
    pending: Option<AgentEvent>,
    /// 上一个回合怎么结束的。日志的 `TurnEnd` 与运行时的 `TurnComplete` 说的是
    /// 同一件事,两边都写同一个字段。
    ended: Option<BackgroundState>,
    /// 最近一次出的错,给面板那一行。运行时的新回合(`TurnStarted`)清掉 ——
    /// 日志里的 `TurnStart` 说的是过去,不清现在这一行。
    error: Option<String>,
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

/// 这个范围里有多少个文件在变。
///
/// `None` = 这里问不出来(不在 git 仓库里、没有那个 base、或 git 不在):那就不说数,
/// 而不是说一个谁都没量过的 0。`scope` 就是 `/review` 自己的词汇 —— 一行说"范围是
/// 未提交的改动",另一行说"X 个文件",两处不能各算一半。
async fn changed_files(dir: &std::path::Path, scope: &str) -> Option<usize> {
    let mut git = tokio::process::Command::new("git");
    git.kill_on_drop(true);
    git.current_dir(dir);
    match scope {
        "staged" => {
            git.args(["diff", "--cached", "--name-only"]);
        }
        "working_tree" => {
            // `status`,不是 `diff`:这次还没提交的包括还没被 git 看见的那些新文件。
            git.args(["status", "--porcelain", "-uall"]);
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
    let out = tokio::time::timeout(std::time::Duration::from_secs(3), git.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().filter(|line| !line.trim().is_empty()).count())
}

/// 一个回合结束后,这个会话该被算成什么状态。
///
/// 一处判断两处用:面板那一行(`LogSummary` 的 `ended`)与"要不要把结果投回去"看的是同一件事
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

/// A terminal outcome must reach its parent even when the model produced no
/// closing text. Never pass an earlier turn's answer off as this turn's result.
fn completion_report(log: &[LoggedEvent], reason: &StopReason) -> String {
    use atomcode_i18n::screen::{t, Msg};
    if *reason == StopReason::Stopped {
        return last_turn_answer(log)
            .unwrap_or_else(|| t(Msg::BackgroundNoConclusion).into_owned());
    }
    let start = log
        .iter()
        .rposition(|e| matches!(e.event, SessionEvent::TurnStart { .. }))
        .unwrap_or(0);
    let error = log[start..].iter().rev().find_map(|e| match &e.event {
        SessionEvent::TurnEnd {
            error: Some(error), ..
        } => Some(error.as_str()),
        _ => None,
    });
    let reason = error
        .map(|e| format!("{reason:?}: {e}"))
        .unwrap_or_else(|| format!("{reason:?}"));
    let partial = last_turn_answer(log).unwrap_or_default();
    t(Msg::BackgroundIncomplete {
        reason: &reason,
        partial: &partial,
    })
    .into_owned()
}

struct State {
    front: Arc<Live>,
    slots: Vec<Arc<Live>>,
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
    /// 已经排上队、等发起它的那段对话收下的结果,键是 sender 的会话 id。
    ///
    /// 事件到达时与 10 秒一次的兜底扫描([`Self::settle_waiting`])都只看它:命中
    /// 一条就标 [`Mark::Home`] 并摘除,
    /// 不再有每条投递一个 10 分钟的全量轮询(issue #1821 #14 / #9)。
    settling: Mutex<HashMap<String, SettleEntry>>,
    /// The sessions stopped because the screen went away, in slot order — what
    /// the launcher prints a `resume` line for after the terminal is back.
    left: Mutex<Vec<String>>,
    /// The session in front when the screen went away, if it had anything said
    /// in it — `None` inside until the exit is recorded, `Some(None)` for an
    /// empty one, which gets no `resume` line.
    exit_front: Mutex<Option<Option<String>>>,
}

/// 一次「结果已排队、等 home 收下」的投递。
struct SettleEntry {
    mark: Arc<Mutex<Mark>>,
    /// 发起它的那段对话的 runtime,用来读它的日志。
    home: Arc<RuntimeControl>,
    /// 投递前 home 日志的末 seq。只有**之后**的 `Injected{Peer{from}}` 才算这次
    /// 收下了 —— 同一段对话第二次委派时,第一次的旧注入事实不能把它提前标成
    /// [`Mark::Home`](issue #1821 #1)。
    baseline_seq: u64,
    /// 上一次检查时 home 日志的末 seq。事件到达时先比这一条:日志没有新事实,就
    /// 不必从 `baseline_seq` 反向扫一遍(issue #1821 #13)。
    seen_seq: u64,
    /// 到这个时刻(unix 毫秒)还没收下就放弃:结果没投进,会话仍该可恢复。
    deadline: u64,
}

/// 被丢进 home 的注可能只落日志、不产生事件,所以除了事件到达时的检查,还有一条
/// 低频兜底:每 10 秒借日志的 `seq` 看一次,不做全量克隆。
const SETTLE_SWEEP: std::time::Duration = std::time::Duration::from_secs(10);
/// 一次投递最多等这么久。过了就不再等 —— 没投进去的结果仍在 `/bg` 里。
const SETTLE_BUDGET_MS: u64 = 10 * 60 * 1000;

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
    )?;
    let background = Arc::new_cyclic(|me: &Weak<Background>| Background {
        state: Mutex::new(State {
            front: Arc::new(front),
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
        settling: Mutex::new(HashMap::new()),
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
    // 注可能只落 home 的日志、不产生事件。低频借一次 seq 兜底;平时
    // `settle_waiting` 在事件到达时就关掉了。
    {
        let weak = Arc::downgrade(&background);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SETTLE_SWEEP);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(background) = weak.upgrade() else {
                    break;
                };
                background.settle_waiting();
            }
        });
    }
    crate::tui_share::remember(background.front_control());
    let owner: Arc<dyn atomcode_daemon::native_live::EmbeddedSessions> = background.clone();
    atomcode_daemon::native_live::register_embedded_sessions(Arc::downgrade(&owner));

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
    projection: Arc<Mutex<LogProjection>>,
}

impl Pumps {
    /// 事件与宿主事件各一条泵,都交给 `background` 决定转不转。
    fn pump(self, background: Weak<Background>) {
        let Pumps {
            id,
            mut events,
            mut said,
            projection,
        } = self;
        {
            let background = background.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    let changed = projection
                        .lock()
                        .expect("background projection poisoned")
                        .absorb(Observed::Live(&event));
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
) -> Result<(Live, Pumps), String> {
    let (connection, control) = crate::host::attach(
        spawned.runtime,
        spawned.front_end,
        spawned.config,
        host_config,
        // Only the session the screen opens on is asked `Readiness` at start.
        id == 0,
    )?;
    let HostConnection {
        commands, events, ..
    } = connection;
    let projection = Arc::new(Mutex::new(LogProjection::default()));
    let said = control.subscribe();
    let own = control.session_id();
    let live = Live {
        id,
        review_assignment: None,
        control,
        commands,
        projection: projection.clone(),
        since: now_ms(),
        origin: own,
        mark: Arc::new(Mutex::new(Mark::Own)),
        name: None,
        task: None,
        initiated_by: InitiatedBy::Person,
    };
    // seed 与增量走同一个 `absorb`:这里把已有的日志折一遍,之后 `describe` /
    // `list` 只补差异。新起的会话这里是空的,恢复的会话在这里或第一次更新时补上。
    if let Some(log) = session_log(&live.control) {
        live.projection
            .lock()
            .expect("background projection poisoned")
            .seed(&log);
    }
    Ok((
        live,
        Pumps {
            id,
            events,
            said,
            projection,
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
fn state_of(live: &Live, summary: &LogSummary) -> BackgroundState {
    state_for(
        live.control.runtime().is_stopped(),
        live.control.runtime().status().phase,
        summary.ended,
        summary.pending.is_some(),
    )
}

/// 阶段与回合结局怎么合成面板状态。拆出来是为了能按矩阵把每一条都钉住
/// (issue #1821 #23),而不是只靠相邻的注释。
fn state_for(
    stopped: bool,
    phase: RuntimePhase,
    ended: Option<BackgroundState>,
    pending: bool,
) -> BackgroundState {
    if stopped {
        return BackgroundState::Failed;
    }
    match phase {
        // 阶段先翻、pump 记下 `pending` 后到:这一小段里还不能说「在等」——不然报出去
        // 的 `Waiting` 没有请求可答,`BackgroundQuestion` 只会落回 `NotFound`。等
        // `pending` 落地那一刻,泵自己会报 changed=true 让 `announce_list` 立刻把它
        // 翻过来,不会漏报,只是晚一拍。
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

fn session_log(control: &RuntimeControl) -> Option<Arc<SessionLog>> {
    let session = control.session_id();
    let app = control.front_end().app()?;
    Feed::find(&app, &session).map(|agent| agent.session())
}

fn read_log<R>(control: &RuntimeControl, read: impl FnOnce(&[LoggedEvent]) -> R) -> R {
    match session_log(control) {
        Some(log) => log.read_events(read),
        None => read(&[]),
    }
}

fn review_task(request: Option<&str>, task: &str) -> String {
    let context = request
        .map(|request| format!("=== ORIGINATING REQUEST (CONTEXT ONLY) === {request}\n\n"))
        .unwrap_or_default();
    format!(
        "{context}=== ASSIGNED READ-ONLY REVIEW ===\n{task}\n\n\
         Review and report findings only. Do not edit files, commit, push, or create pull requests. \
         The originating conversation owns all follow-up work, including work requested above."
    )
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

fn review_title(scope: Option<&str>) -> String {
    use atomcode_i18n::screen::{t, Msg};
    let scope = match scope {
        None => return "code-review".into(),
        Some("working_tree") => t(Msg::ReviewWhatUncommitted),
        Some("staged") => t(Msg::ReviewWhatStaged),
        Some(base) => t(Msg::ReviewWhatRange { base }),
    };
    format!("code-review · {scope}")
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
impl LogSummary {
    /// 复位日志派生的一半,只把只来自运行时事件的字段搬回来。
    ///
    /// `pending`/`error` 不是日志里的事实:提问(与它的回答)只在提问那一刻发一次,
    /// 重订阅不会再补发,日志里也没有对应记录。清掉就再也补不回来 —— 提问侧还挂在
    /// `rx` 上,面板却从 `Waiting` 掉回 `Running`,`question()`/`answer()` 再也取不回
    /// 那个请求。日志那一半由随后的 seed/增量重建(issue #1821 #12)。
    fn reset_log_derived(&mut self) {
        let pending = self.pending.take();
        let error = self.error.take();
        *self = Self::default();
        self.pending = pending;
        self.error = error;
    }

    /// 折进日志里的一条事实:计一条,再按它改字段。seed 与增量都走这里,所以两条
    /// 路径不可能各说一套(issue #1821 #12)。
    ///
    /// `rebuild`:这一批是不是 `seed` 的整份重建。日志侧的 `error` 只在这时写 ——
    /// 增量折进来的一条 `TurnEnd` 可能早于运行时刚报的 `Error`(面板没开时日志只积
    /// 不折),它说的不是「现在」。
    fn absorb_logged(&mut self, logged: &LoggedEvent, rebuild: bool) -> bool {
        self.len += 1;
        self.absorb_event(logged.at, &logged.event, rebuild)
    }

    /// 一条已提交的事实对面板那一行的影响,返回它会不会因此变。
    ///
    /// 事实的计数在 [`Self::absorb_logged`]:这里只管它改哪个字段。
    fn absorb_event(&mut self, at: u64, event: &SessionEvent, rebuild: bool) -> bool {
        self.first_at.get_or_insert(at);
        self.last_at = at;
        match event {
            SessionEvent::TurnStart { .. } => {
                // 新回合开始:上一个回合的结局不再代表现在。`error` 不在这里清:
                // 这条事实可能早于它被折进来的那一秒 —— 面板关着时日志只积不折,
                // 一条还没折的旧 `TurnStart` 会清掉运行时刚刚报上来的 `Error`,而
                // 那条 `Error` 恰恰是「现在这一回合失败了」。清 `error` 只在运行
                // 时那一侧(`saw(TurnStarted)`)做,那里说的才是现在。
                self.ended = None;
                true
            }
            SessionEvent::TurnEnd { stop, error, .. } => {
                // `pending` 不在这里清:它不是日志里的事实,日志这一侧从不产生它。
                // 而这条 `TurnEnd` 可能已经旧了 —— 面板没开时日志只积不折,新的
                // `AgentEvent::Request` 先到(`pending` 已置位),第一次 `describe`
                // 折上一回合的 `TurnEnd` 就会把刚到的请求清掉:面板从 `Waiting`
                // 掉回 `Running`,`question()` 永久 `NotFound`,`answer()` 回
                // `Stale`。回合结束该由运行时的 `saw(TurnComplete)` 说 —— 与
                // `ended` 同一条路径(issue #1821 #24)。
                self.ended = Some(ended_state(stop));
                if error.is_some() && rebuild {
                    // 日志侧的 `error` 只在整份重建时写。增量折进来的这条 `TurnEnd`
                    // 可能已经旧了 —— 面板关着时日志只积不折,运行时刚报的 `Error`
                    // 先到,一次增量读上一回合的 `TurnEnd` 就把新错盖掉,那一行要等
                    // 下一条无关 announce 才播,而新 `Error` 才是「现在这一回合失败了」。
                    self.error = error.clone();
                }
                true
            }
            SessionEvent::StepEnd { tool_calls, .. } => {
                self.steps += 1;
                self.tools = self.tools.saturating_add(*tool_calls);
                true
            }
            SessionEvent::Usage { usage, .. } => {
                self.completion = self.completion.saturating_add(usage.completion);
                self.sent += u64::from(usage.prompt);
                self.sent_cached += u64::from(usage.cached);
                self.usage = Some(*usage);
                true
            }
            SessionEvent::Titled { title, .. } if !title.trim().is_empty() => {
                self.title = Some(title.clone());
                true
            }
            SessionEvent::UserMessage { text, .. } => {
                let first = !self.has_user_message;
                self.has_user_message = true;
                if first {
                    self.first_user = one_line(text);
                }
                first
            }
            SessionEvent::Asked { question, .. } => {
                if let Some(words) = one_line(&question.prompt) {
                    self.asked = Some(words);
                    return true;
                }
                false
            }
            SessionEvent::AssistantMessage { text, .. }
            | SessionEvent::PartialReply { text, .. } => {
                if let Some(words) = one_line(text) {
                    self.last_said = Some(words);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// 折进一条运行时事件。`pending` 只在这里记(它不是日志里的事实);
    /// `ended` / `error` 与日志的 `TurnEnd` 写同一个字段 —— 但**清** error 只在这里
    /// 的 `TurnStarted`,日志那条路说的是过去。
    fn saw(&mut self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::TurnStarted { .. } => {
                self.error = None;
                self.ended = None;
                true
            }
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
                // 这一行确实会变:phase 已经落 `Failed`,没有 `TurnComplete` 的收尾
                // 路径(重配回滚失败、恢复持久化失败、fail-close)不会再有第二条事件
                // 来说它 —— 所以这里要一次重画,否则面板要等下一条无关 announce 才播。
                self.error = Some(message.clone());
                true
            }
            _ => false,
        }
    }

    fn stats(&self, running_at: Option<u64>) -> Option<BackgroundStats> {
        let elapsed_ms = self.first_at.map_or(0, |first| {
            running_at.unwrap_or(self.last_at).saturating_sub(first)
        });
        let usage = match (self.usage, running_at) {
            (Some(usage), _) => usage,
            (None, Some(_)) if elapsed_ms > 0 => atomcode_kernel::stream::TokenUsage::default(),
            (None, _) => return None,
        };
        Some(BackgroundStats {
            steps: self.steps,
            tools: self.tools,
            prompt: usage.prompt,
            cached: usage.cached,
            completion: self.completion,
            elapsed_ms,
            sent: self.sent,
            sent_cached: self.sent_cached,
        })
    }
}

impl LogProjection {
    /// 折进一条观察。返回面板要不要重画。
    ///
    /// `Logged` 是日志事实的唯一来源(seed、增量都算 `len`);其余运行时事件只更新
    /// 回合状态。两条来源共用 [`LogSummary`],没有第二套推导。
    ///
    /// 通道上的 `AgentEvent::Fact` **不**在这里折:它说的是日志刚多了哪一条,而
    /// pump 与 `describe` 面对的是同一个前端、同一份日志。屏幕一订阅
    /// (`Subscribe { from: 0 }`),feed 就把整段历史与新提交的事实逐条推过来,
    /// `update` 又按 cursor 从日志折同一批 —— 两边都折,首屏重放与回到后台之后
    /// 面板的步数/工具/token 就会翻倍。事实引起的面板变化由下一次 `update` 带出来
    /// (面板每秒都在读),所以这里也不必替它要一次重画。
    fn absorb(&mut self, observed: Observed<'_>) -> bool {
        match observed {
            Observed::Logged(logged) => self.summary.absorb_logged(logged, self.rebuilding),
            Observed::Live(AgentEvent::Fact(_)) => false,
            Observed::Live(event) => self.summary.saw(event),
        }
    }

    /// 从一份已有的日志建投影:与之后的增量走同一个 [`Self::absorb`]。
    fn seed(&mut self, log: &Arc<SessionLog>) {
        // 日志这一半整份重建,只在运行时事件里的字段不跟着走。
        self.summary.reset_log_derived();
        // 整份重建:日志侧 `error` 在这里说这份日志最后的失败。
        self.rebuilding = true;
        let last = log.read_events(|events| {
            for logged in events {
                self.absorb(Observed::Logged(logged));
            }
            events.last().map_or(0, |logged| logged.seq)
        });
        self.rebuilding = false;
        self.cursor = LogCursor {
            log: Arc::downgrade(log),
            seen: last,
        };
    }

    /// 只折日志里新增的事实。`reset`(换了日志、或 seq 倒退)时从零重建 —— 同样是
    /// 一个 `absorb`,所以 seed 与增量等值。
    fn update(&mut self, log: Option<Arc<SessionLog>>) {
        let Some(log) = log else {
            // 日志暂时读不到(App 正在重建、`Feed` 还没找到这个会话):不复位,也不折
            // 事实。`pending`/`error` 不在日志里,清掉就再也补不回来(提问侧还挂在
            // `rx` 上,面板却从 `Waiting` 掉回 `Running`);`cursor` 留着,日志回到同一份
            // `Arc` 时按 `seen` 续读,重建期间新增的事实一条不落,已折过的字段也不会
            // 因为「只折新增」而永久空着(issue #1821 #12)。
            return;
        };
        // 借出两个字段:`read_new` 握着 `cursor`,折事实的那半边只能碰 `summary`。
        let Self {
            cursor, summary, ..
        } = self;
        cursor.read_new(&log, |reset, events| {
            if reset {
                // 换了日志(或 seq 倒退):日志那一半从头重建,只把不在日志里的字段搬回来。
                summary.reset_log_derived();
            }
            for logged in events {
                // `update` 折的是「日志里已有/新增的事实」,即便 `reset` 是整份重建,
                // 这批事实也可能早于运行时刚报的 `Error`。日志侧的 `error` 只在
                // `seed`(attach 时)写,这里一律不写。
                summary.absorb_logged(logged, false);
            }
        });
    }
}

fn describe(live: &Live) -> BackgroundSession {
    let mut projection = live
        .projection
        .lock()
        .expect("background projection poisoned");
    projection.update(session_log(&live.control));
    let summary = &projection.summary;
    let state = state_of(live, summary);
    // 在等的那个请求本身说了什么:不是每种问法都先在日志里记一条 `Asked`。
    let waiting_on = || {
        let Some(AgentEvent::Request { payload, .. }) = summary.pending.as_ref() else {
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
        BackgroundState::Waiting => summary
            .asked
            .clone()
            .or_else(waiting_on)
            .or_else(|| summary.last_said.clone()),
        BackgroundState::Failed => summary
            .error
            .clone()
            .and_then(|error| one_line(&error))
            .or_else(|| summary.last_said.clone()),
        _ => summary.last_said.clone(),
    };
    BackgroundSession {
        session: live.control.session_id(),
        // 发起方起的名字优先;再是它自己起的;还没起名的,用它的第一句话 —— 日志里
        // 还没有时,用交给它的那件事的第一行。面板那一列总得有点什么可认,而一串
        // id 前缀认不出是哪件活。
        title: live
            .name
            .clone()
            .or_else(|| summary.title.clone())
            .or_else(|| summary.first_user.clone())
            .or_else(|| live.task.clone()),
        state,
        created_at: live.since,
        last,
        stats: summary.stats(
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

/// The result of `session` is in `home`'s log: `home` took the note in, *after*
/// this delivery began. Facts at or before `baseline_seq` belong to an earlier
/// delegation of the same session and must not count (issue #1821 #1).
fn holds_note_from(events: &[LoggedEvent], session: &str, baseline_seq: u64) -> bool {
    use atomcode_kernel::session::InjectionOrigin;
    events
        .iter()
        .rev()
        .take_while(|logged| logged.seq > baseline_seq)
        .any(|logged| {
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

/// Whether a parent session is hidden from `/resume`/`--continue` right now.
///
/// Done with a tool's review stays out; one the person scoped with `/review`
/// stays visible until its result lands. A settled delivery is out either way.
fn hides(mark: &Mark, initiated_by: InitiatedBy) -> bool {
    match mark {
        Mark::Home(_) => true,
        Mark::Pending(_) => initiated_by == InitiatedBy::Tool,
        Mark::Own | Mark::TakenUp => false,
    }
}

/// 一个会话里有没有人说过话。没有的话它不值得占一个槽位。
///
/// 读的是已经折好的投影:先补上日志里新增的事实,再问一个标志 —— 不再为一个是非题
/// 扫一遍整份日志(issue #1821 #11)。
fn has_conversation(live: &Live) -> bool {
    let mut projection = live
        .projection
        .lock()
        .expect("background projection poisoned");
    projection.update(session_log(&live.control));
    projection.summary.has_user_message
}

async fn stop(live: &Live) {
    let handle = live.control.runtime().clone();
    if in_turn(live) {
        // 取消失败不挡停:停本身也会把回合带走,取消只是让它以「被打断」落盘。
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle.cancel()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "background cancel failed; proceeding to runtime shutdown")
            }
            Err(_) => tracing::warn!(
                "background cancel exceeded five seconds; proceeding to runtime shutdown"
            ),
        }
    }
    // Keep awaiting the owner's terminal: it is published only after kernel,
    // transport and session-lease cleanup. Dropping this wait on a timer would
    // allow an editor to outlive its slot and a replacement to race its lease.
    if let Err(error) = handle.shutdown().await {
        tracing::warn!(%error, "background runtime shutdown unavailable");
    }
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
        // 结果被 home 收下这件事,通常伴随 home 的一条事件(回合开始、被接受、
        // 上下文加入、回合结束)。借这一刻把注册表里等待的那几条关掉;真正只落日志
        // 的那种由 10 秒兜底扫描接住。
        if matches!(
            &event,
            AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnComplete { .. }
                | AgentEvent::Accepted { .. }
                | AgentEvent::ContextAdded { .. }
                | AgentEvent::Fact(_)
        ) {
            self.settle_waiting();
        }
        // 替别的会话干活的那个,干完了把结果投回去 —— 内容落在发起它的那段对话里。
        // 这是"结果回来了"在那段对话里的形状。**只投内容**:要不要接着逐条核实,是那段
        // 对话看了上下文自己决定的事,不由这里替它下令。
        if let AgentEvent::TurnComplete { reason, .. } = &event {
            self.deliver_home(id, reason);
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
    fn deliver_home(&self, id: u64, reason: &StopReason) {
        use atomcode_i18n::screen::{t as tr, Msg as SMsg};
        let (origin, sender, log, mark) = {
            let state = self.state.lock().expect("background poisoned");
            let Some(live) = state.slots.iter().find(|slot| slot.id == id) else {
                return;
            };
            if live.origin.is_empty() || live.origin == live.control.session_id() {
                return;
            }
            (
                live.origin.clone(),
                live.control.session_id(),
                session_log(&live.control),
                live.mark.clone(),
            )
        };
        let summarize =
            |events: &[LoggedEvent]| (name_of(events), completion_report(events, reason));
        let (title, answer) = match log {
            Some(log) => log.read_events(summarize),
            None => summarize(&[]),
        };
        let frame = tr(SMsg::BackgroundResult {
            title: &title,
            answer: &answer,
        })
        .into_owned();
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
        // 投递前 home 日志的末 seq:只有它之后的事实才算这一次收下了。同一段对话
        // 第二次委派时,第一次的旧注入事实不能把它提前标成 `Mark::Home`(#1)。
        //
        // 日志此刻读不到(App 正在重建、`Feed` 还没找到这个会话)就**不登记**:下界
        // 退化成 0 会把整份 home 日志放进匹配范围,上一次委派留下的旧注入事实会被
        // 算作这次「收下了」,会话随即被隐藏。宁可在退出/恢复里多留一条,也不把没
        // 确认收下的结果标成 `Mark::Home`。
        match session_log(&home).map(|log| log.last_seq()) {
            Some(baseline_seq) => {
                if matches!(*mark.lock().expect("mark poisoned"), Mark::Pending(_)) {
                    let mut settling = self.settling.lock().expect("settling poisoned");
                    settling.insert(
                        sender.clone(),
                        SettleEntry {
                            mark: mark.clone(),
                            home: home.clone(),
                            baseline_seq,
                            seen_seq: baseline_seq,
                            deadline: now_ms().saturating_add(SETTLE_BUDGET_MS),
                        },
                    );
                }
            }
            None => tracing::debug!(
                origin = %origin,
                from = %sender,
                "home log unreadable at delivery: not watching for a settle"
            ),
        }
        let handle = home.runtime().clone();
        let me = self.me.clone();
        tokio::spawn(async move {
            // 一条注,不是一次用户提交:日志里说话的是那个后台会话,不是人。屏上的内容
            // 一字不少 —— 变的只是**谁说的**。
            //
            // `Busy` 是那段对话正在收尾一次取消或一次策略介入,过一会儿就收得下:等一等
            // 再投,而不是把结果扔了。别的拒绝(runtime 没了、换了代)不重试 —— 那时
            // 这个 handle 背后未必还是发起它的那段对话;结果仍在 /bg 面板里。
            // 选中接收方以后仍可能切会话;由 runtime owner 在投递时再核对原会话。
            const TRIES: u32 = 10;
            let mut delivered = false;
            for attempt in 1..=TRIES {
                match handle
                    .note_to_session(origin.clone(), sender.clone(), frame.clone())
                    .await
                {
                    Ok(_) => {
                        delivered = true;
                        break;
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
                        break;
                    }
                }
            }
            let Some(background) = me.upgrade() else {
                return;
            };
            if !delivered {
                // 重试耗尽:什么都没进 home,不再等。
                background.remove_settle(&sender);
                return;
            }
            // 排上队不等于收下了:home 正忙时这是一次 steer。事件到达与兜底扫描会
            // 在它真进了日志之后再标;这里先看一眼,免得事件在登记前就到了。
            background.settle_sender(&sender);
        });
    }

    /// Settle, before the exit reads them, the background sessions whose result
    /// was taken in by the conversation they worked for but not yet marked.
    ///
    /// 用的是每次投递自己的 baseline:没有登记的(结果根本没排上队)不在这里结算。
    fn settle_before_exit(&self) {
        let senders: Vec<String> = {
            let settling = self.settling.lock().expect("settling poisoned");
            settling.keys().cloned().collect()
        };
        for sender in senders {
            self.settle_sender(&sender);
        }
    }

    /// 把注册表里等待的那几条检查一遍。
    ///
    /// 两个触发点共用这一个函数体:home 的一条事件到了([`Self::arrived`]),或 10 秒
    /// 一次的兜底扫描。检查本身只看 `seen_seq` 之后的新事实,所以多问几次不贵 ——
    /// 而少问一次就可能漏掉只落日志、不产生事件的注(issue #1821 #14)。
    fn settle_waiting(&self) {
        let senders: Vec<String> = {
            let settling = self.settling.lock().expect("settling poisoned");
            if settling.is_empty() {
                return;
            }
            settling.keys().cloned().collect()
        };
        for sender in senders {
            self.settle_sender(&sender);
        }
    }

    /// 一条投递现在收下了没有。收下 → 标 [`Mark::Home`] 并摘除;home 没了或预算
    /// 耗尽 → 摘除,结果仍可恢复。
    fn settle_sender(&self, sender: &str) {
        let (mark, home, baseline_seq, seen_seq) = {
            let mut settling = self.settling.lock().expect("settling poisoned");
            let Some(entry) = settling.get(sender) else {
                return;
            };
            if now_ms() >= entry.deadline {
                settling.remove(sender);
                return;
            }
            (
                entry.mark.clone(),
                entry.home.clone(),
                entry.baseline_seq,
                entry.seen_seq,
            )
        };
        let Some(log) = session_log(&home) else {
            // A retired home cannot add another fact: stop watching, leave the result
            // recoverable rather than marking an undelivered one as received.
            if home.runtime().is_stopped() || home.front_end().app().is_none() {
                self.remove_settle(sender);
            }
            return;
        };
        let last = log.last_seq();
        // 日志没有新事实就不必反向扫:事件到达时每条登记项只是比一次 seq,与等待的
        // 条数和日志长度都无关(issue #1821 #13)。
        if last > seen_seq
            && log.read_events(|events| holds_note_from(events, sender, baseline_seq))
        {
            came_home_now(&mark, sender);
            self.remove_settle(sender);
            return;
        }
        if let Some(entry) = self
            .settling
            .lock()
            .expect("settling poisoned")
            .get_mut(sender)
        {
            entry.seen_seq = entry.seen_seq.max(last);
        }
        // 退役的 home 不会再添事实:停止观察,结果仍可恢复。
        if home.runtime().is_stopped() {
            self.remove_settle(sender);
        }
    }

    /// Drop a watch. Every give-up path goes through here: stopped home, exhausted
    /// budget, exhausted delivery retries, delivery error, take-up, drop.
    fn remove_settle(&self, sender: &str) {
        self.settling
            .lock()
            .expect("settling poisoned")
            .remove(sender);
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
        // 锁内只取槽位快照;读日志、算聚合与 `describe` 都在锁外,不再让每秒钟的
        // 面板刷新把 `state` 扣在一份大日志上(issue #1821 #10a)。
        let slots: Vec<Arc<Live>> = {
            let state = self.state.lock().expect("background poisoned");
            state.slots.iter().cloned().collect()
        };
        slots.iter().map(|live| describe(live)).collect()
    }

    /// 有人在看,而且后台有活在跑(在跑一个回合,或在等人回答)。
    fn someone_running(&self) -> bool {
        if self.watchers.lock().expect("watchers poisoned").is_empty() {
            return false;
        }
        let state = self.state.lock().expect("background poisoned");
        state.slots.iter().any(|slot| in_turn(slot))
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
                AgentCommand::Subscribe { session, from } => Kind::Subscribe {
                    session: session.clone(),
                    from: *from,
                },
                // The foreground gate must open for receipted subscriptions too.
                // Their runtime answers only after it enqueues the history.
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
                    // Request publication can precede the phase watch update.
                    // The projection is the authority for an unanswered live request.
                    let pending = lead
                        .then(|| {
                            state
                                .front
                                .projection
                                .lock()
                                .expect("background projection poisoned")
                                .summary
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
                        if let AgentCommand::Tagged { id, .. } = &command {
                            let _ = front_end.events().send(AgentEvent::Accepted {
                                command: id.clone(),
                                turn: None,
                                steered: false,
                            });
                        }
                    }
                    _ => {
                        let _ = commands.send(command);
                    }
                }
            }
            Kind::Respond { id } => {
                let state = self.state.lock().expect("background poisoned");
                {
                    let mut projection = state
                        .front
                        .projection
                        .lock()
                        .expect("background projection poisoned");
                    if matches!(&projection.summary.pending, Some(AgentEvent::Request { id: waiting, .. }) if *waiting == id)
                    {
                        projection.summary.pending = None;
                    }
                }
                let _ = state.front.commands.send(command);
            }
            Kind::Send => {
                let state = self.state.lock().expect("background poisoned");
                state
                    .front
                    .projection
                    .lock()
                    .expect("background projection poisoned")
                    .summary
                    .ended = None;
                let _ = state.front.commands.send(command);
            }
            // 退出:前台照旧,后台的一个个停下来(`tui_front::run` 等它们停完)。
            Kind::Shutdown => {
                // Which session is in front, read **before** the stop goes out:
                // after it the App is gone and its log with it.
                self.record_exit_front();
                self.settle_before_exit();
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
                .filter(|slot| has_conversation(slot.as_ref()) && !came_home(slot.as_ref()))
                .map(|slot| slot.control.session_id()),
        );
        futures::future::join_all(
            slots
                .into_iter()
                .map(|slot| async move { stop(&slot).await }),
        )
        .await;
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
        // Request publication can precede the status watch's WaitingApproval
        // update. A question already shown on screen must not be left behind.
        // A WebUI response bypasses the TUI's cached request. Read current
        // runtime control state so a resolved question cannot strand navigation.
        let waiting = front
            .control
            .runtime()
            .has_pending_blocking_request()
            .unwrap_or(true);
        if waiting {
            return Err(Self::busy(tr(SMsg::BgRefusedWhileAsking).into_owned()));
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

    async fn spawn_at(&self, working_dir: PathBuf) -> Result<Live, HostError> {
        let (live, pumps) = self.prepare_at(working_dir).await?;
        pumps.pump(self.me.clone());
        Ok(live)
    }

    /// Keep a candidate's events private until its session is ready to display.
    async fn prepare_at(&self, working_dir: PathBuf) -> Result<(Live, Pumps), HostError> {
        let spawned = (self.spawn)(working_dir)
            .await
            .map_err(|message| HostError::Failed { message })?;
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (live, pumps) = attach(id, spawned, self.host_config.clone())
            .map_err(|message| HostError::Failed { message })?;
        if live.control.session_id().is_empty() {
            stop(&live).await;
            return Err(HostError::Failed {
                message: "the new runtime has no session".into(),
            });
        }
        Ok((live, pumps))
    }

    fn full() -> HostError {
        use atomcode_i18n::screen::{t as tr, Msg as SMsg};
        Self::busy(tr(SMsg::BgSlotsFull { most: MOST }).into_owned())
    }

    /// An idle parent still owns the destination for an unfinished delegation.
    /// Keep it alive until the result is committed, rather than retiring it as
    /// an ordinary idle history tab during a shared navigation.
    fn retain_front(state: &State) -> bool {
        let session = state.front.control.session_id();
        in_turn(&state.front)
            || state.slots.iter().any(|slot| {
                slot.origin == session
                    && matches!(*slot.mark.lock().expect("mark poisoned"), Mark::Pending(_))
            })
    }

    /// 把 `incoming` 放到前台。返回被换下来的那个。调用方持着 `op`。
    fn put_in_front(&self, state: &mut State, incoming: Arc<Live>) -> Arc<Live> {
        let outgoing = std::mem::replace(&mut state.front, incoming);
        if in_turn(&outgoing) {
            // 正在跑的那个回合还没有结局,上一个回合的结局不算数了。
            outgoing
                .projection
                .lock()
                .expect("background projection poisoned")
                .summary
                .ended = None;
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
        let fresh = self.spawn_at(working_dir).await?;
        let fresh_session = fresh.control.session_id();
        let mut fresh = Some(fresh);
        let result = {
            let exiting = self.exit_front.lock().expect("exit poisoned");
            let mut state = self.state.lock().expect("background poisoned");
            Self::may_move(&state.front).and_then(|_| {
                if exiting.is_some()
                    || state.front.control.session_id() != session
                    || state.front.control.runtime().is_stopped()
                {
                    return Err(HostError::NotFound);
                }
                crate::tui_share::follow_with(
                    fresh.as_ref().expect("candidate").control.clone(),
                    || {
                        let outgoing = self
                            .put_in_front(&mut state, Arc::new(fresh.take().expect("candidate")));
                        state.slots.push(outgoing);
                        state.slots.len() as u32
                    },
                )
                .map_err(|message| HostError::Failed { message })
            })
        };
        let slot = match result {
            Ok(slot) => slot,
            Err(error) => {
                stop(fresh.as_ref().expect("uncommitted candidate")).await;
                return Err(error);
            }
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
        self.foreground_locked(session, target).await
    }

    /// Caller holds the slot-operation lock, including when entered via /resume.
    async fn foreground_locked(
        &self,
        session: String,
        target: String,
    ) -> Result<HostReply, HostError> {
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
            crate::tui_share::follow_with(state.slots[at].control.clone(), || {
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
            })
            .map_err(|message| HostError::Failed { message })?
        };
        take_up(&mark, &target);
        // 人接手了:这条会话不再是无人认领的结果,不必再等它「回家」。
        self.remove_settle(&target);
        self.announce(HostEvent::SessionChanged {
            session: target.clone(),
            previous: Some(session),
        });
        self.announce_list();
        if let Some(empty) = closed {
            stop(&empty).await;
        }
        Ok(HostReply::SessionChanged { session: target })
    }

    /// A running foreground keeps its runtime and lease while another session
    /// is prepared privately. Failed preparation never changes the slot table.
    async fn resume(
        &self,
        session: String,
        target: String,
        across_projects: bool,
    ) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        self.resume_locked(session, target, across_projects).await
    }

    async fn resume_locked(
        &self,
        session: String,
        target: String,
        project_navigation: bool,
    ) -> Result<HostReply, HostError> {
        let front = {
            let state = self.state.lock().expect("background poisoned");
            if state.front.control.session_id() != session {
                return Err(HostError::NotFound);
            }
            Self::may_move(&state.front)?;
            state.front.control.clone()
        };
        if self.in_background(&target) {
            if project_navigation {
                let control = self
                    .state
                    .lock()
                    .expect("background poisoned")
                    .slots
                    .iter()
                    .find(|slot| slot.control.session_id() == target)
                    .map(|slot| slot.control.clone())
                    .ok_or(HostError::NotFound)?;
                use atomcode_capabilities::session::SessionManager;
                let dirs = atomcode_coding::config::product_dirs_from_env();
                let scan = crate::host::scan_session_catalog().await?;
                let entry = crate::host::unique_stored_entry(&scan.entries, &target)?;
                let cwd = control.working_dir_now().await;
                if !cwd.is_dir()
                    || SessionManager::project_hash(&cwd, &dirs) != entry.project_bucket
                {
                    return Err(HostError::Failed {
                        message: "The session's project directory is unavailable or belongs to another storage bucket.".into(),
                    });
                }
            }
            return self.foreground_locked(session, target).await;
        }
        if target == session
            || (!project_navigation
                && !crate::tui_share::sharing()
                && !in_turn(&self.state.lock().expect("background poisoned").front))
        {
            return front.call(HostCommand::Resume { session, target }).await;
        }
        {
            let state = self.state.lock().expect("background poisoned");
            if state.slots.len() >= MOST && Self::retain_front(&state) {
                return Err(Self::full());
            }
        }
        let mut working_dir = front.working_dir_now().await;
        let entry = if project_navigation {
            let scan = crate::host::scan_session_catalog().await?;
            let entry = find_switchable_entry(&scan.entries, &target)?;
            if entry.needs_newer_version {
                return Err(HostError::Failed {
                    message: "Update AtomCode before opening this session.".into(),
                });
            }
            working_dir = crate::host::resume_working_dir(entry, working_dir, true)?;
            Some(entry.clone())
        } else {
            None
        };
        let (mut incoming, mut pumps) = self.prepare_at(working_dir.clone()).await?;
        let candidate = incoming.control.session_id();
        let reply = match entry {
            Some(entry) => {
                incoming
                    .control
                    .resume_catalog_entry(&candidate, &entry, working_dir)
                    .await
            }
            None => {
                incoming
                    .control
                    .call(HostCommand::Resume {
                        session: candidate,
                        target,
                    })
                    .await
            }
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                stop(&incoming).await;
                return Err(error);
            }
        };
        let resumed = incoming.control.session_id();
        incoming.origin = resumed.clone();
        // The hidden control already announced its fresh -> resumed transition.
        // Publish exactly one foreground transition, naming the actual old view.
        while pumps.said.try_recv().is_ok() {}
        // The seed is the log as it stands after preparation, drained facts included
        // once: a first cached `len` that disagrees with the session's log would keep
        // the projection offset for the rest of its life (issue #1821 #12).
        if let Some(log) = session_log(&incoming.control) {
            pumps
                .projection
                .lock()
                .expect("background projection poisoned")
                .seed(&log);
        }
        while let Ok(event) = pumps.events.try_recv() {
            // Preserve a request that appeared during preparation: subscribing
            // to the committed foreground must still bring that question back.
            // Facts that arrived meanwhile change nothing: the seed above already
            // folded them, and the projection takes facts only from the log.
            pumps
                .projection
                .lock()
                .expect("background projection poisoned")
                .absorb(Observed::Live(&event));
        }
        let mut incoming = Some(incoming);
        let mut retired = None;
        let switched = {
            // Match record_exit_front's lock order: an exit already recorded
            // during preparation must not acquire a replacement foreground.
            let exiting = self.exit_front.lock().expect("exit poisoned");
            let mut state = self.state.lock().expect("background poisoned");
            if exiting.is_some()
                || state.front.control.runtime().is_stopped()
                || state.front.control.session_id() != session
            {
                Err(HostError::NotFound)
            } else if let Err(error) = Self::may_move(&state.front) {
                Err(error)
            } else {
                crate::tui_share::follow_with(
                    incoming.as_ref().expect("candidate").control.clone(),
                    || {
                        let retain = Self::retain_front(&state);
                        let outgoing = self.put_in_front(
                            &mut state,
                            Arc::new(incoming.take().expect("candidate")),
                        );
                        if retain {
                            state.slots.push(outgoing);
                        } else {
                            retired = Some(outgoing);
                        }
                    },
                )
                .map_err(|message| HostError::Failed { message })
            }
        };
        if let Err(error) = switched {
            stop(incoming.as_ref().expect("uncommitted candidate")).await;
            return Err(error);
        }
        pumps.pump(self.me.clone());
        self.announce(HostEvent::SessionChanged {
            session: resumed,
            previous: Some(session),
        });
        self.announce_list();
        if let Some(retired) = retired {
            stop(&retired).await;
        }
        Ok(reply)
    }

    async fn start(&self, text: String, scope: Option<String>) -> Result<HostReply, HostError> {
        let _op = self.op.lock().await;
        let working_dir = {
            let state = self.state.lock().expect("background poisoned");
            state.front.control.clone()
        }
        .working_dir_now()
        .await;
        self.start_locked(working_dir, text, scope, None, None)
            .await
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
        files: usize,
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
        let request = read_log(&self.front_control(), current_request);
        // The parent's request is context, not another assignment. In
        // particular, its authorized edits/commit/PR remain the parent's job.
        let text = review_task(request.as_deref(), &text);
        self.start_locked(working_dir, text, scope, name, Some(files))
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
        confirmed_files: Option<usize>,
    ) -> Result<HostReply, HostError> {
        let control = {
            let state = self.state.lock().expect("background poisoned");
            if state.slots.len() >= MOST {
                return Err(Self::full());
            }
            state.front.control.clone()
        };
        // Read the live owner's mode, not the launch-time configuration. Take
        // one snapshot for this new task; later changes do not alter it.
        let review_only = scope.is_some() || name.as_deref() == Some("code-review");
        // ReviewHandoffRow confines an unopened review to its read-only reviewer;
        // its approval mode still follows the conversation that assigned it.
        let mode = control
            .runtime()
            .mode()
            .await
            .map_err(crate::host::refused)?;
        // 说得出范围的(审查就是),顺手把"这次有几个文件在变"算出来 —— 那行要说它。
        let files = match (confirmed_files, scope.as_deref()) {
            (Some(files), _) => Some(files),
            (None, Some(scope)) => changed_files(&working_dir, scope).await,
            (None, None) => None,
        };
        let mut live = self.spawn_at(working_dir.clone()).await?;
        // Configure before publishing the slot or submitting any work. Failure
        // retires the candidate and never runs it with a fallback permission.
        if let Err(error) = live.control.runtime().set_mode(mode).await {
            stop(&live).await;
            return Err(crate::host::refused(error));
        }
        if review_only {
            // Name the assignment before its first prompt can launch the model
            // titler. The same logged fact serves the panel and resumed sessions.
            let session = live.control.session_id();
            let agent = live
                .control
                .front_end()
                .app()
                .and_then(|app| Feed::find(&app, &session));
            let Some(agent) = agent else {
                stop(&live).await;
                return Err(HostError::Unavailable);
            };
            let log = agent.session();
            atomcode_harness::session::commit(
                agent.ctx(),
                &log,
                SessionEvent::Titled {
                    turn: log.current_turn(),
                    title: review_title(scope.as_deref()),
                    user_set: false,
                },
            );
        }
        // 它是替**前台那个**干的:干完把结果投回去,而不是留在这里等人来读。
        live.origin = control.session_id();
        // 结果落进那段对话之后,盘上才标它不是人的一段对话(见 `Mark`)。
        live.mark = Arc::new(Mutex::new(Mark::Pending(working_dir)));
        // 隐藏只由发起方决定:模型的 `code_review` 是一个工具发起的,`/review` /
        // `/bg` 是人发起的。`review_only` 仍回答权限问题,不再兼做隐藏判断(#3）。
        live.initiated_by = if name.as_deref() == Some("code-review") {
            InitiatedBy::Tool
        } else {
            InitiatedBy::Person
        };
        live.name = name;
        live.task = one_line(&text);
        let session = live.control.session_id();
        live.review_assignment = review_only.then(|| session.clone());
        let id = live.id;
        let handle = live.control.runtime().clone();
        // Register visibility before submit can commit the first user message.
        // The operation lock prevents navigation/drop during this reservation.
        let slot = {
            let mut state = self.state.lock().expect("background poisoned");
            state.slots.push(Arc::new(live));
            state.slots.len() as u32
        };
        if let Err(error) = handle.submit(UserInput::from(text)).await {
            let failed = {
                let mut state = self.state.lock().expect("background poisoned");
                let at = state
                    .slots
                    .iter()
                    .position(|live| live.id == id)
                    .expect("reserved slot");
                state.slots.remove(at)
            };
            stop(&failed).await;
            self.announce_list();
            return Err(crate::host::refused(error));
        }
        self.announce_list();
        Ok(HostReply::Backgrounded {
            session,
            slot,
            files,
        })
    }

    async fn tell(&self, target: String, text: String) -> Result<HostReply, HostError> {
        let (handle, projection, mark) = {
            let state = self.state.lock().expect("background poisoned");
            let slot = state
                .slots
                .iter()
                .find(|slot| slot.control.session_id() == target)
                .ok_or(HostError::NotFound)?;
            (
                slot.control.runtime().clone(),
                slot.projection.clone(),
                slot.mark.clone(),
            )
        };
        take_up(&mark, &target);
        self.remove_settle(&target);
        handle
            .submit(UserInput::from(text))
            .await
            .map_err(crate::host::refused)?;
        projection
            .lock()
            .expect("background projection poisoned")
            .summary
            .ended = None;
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
        self.remove_settle(&target);
        stop(&live).await;
        self.announce_list();
        Ok(HostReply::Done)
    }

    /// 那个会话此刻挂着的问询。没在等人就没有可说的。
    ///
    /// 不猜、也不编:列表那一行已经说了它在干什么,这里再说一遍是噪音;而屏幕
    /// 只在它自己认为那个会话在等人时才会来要,所以这两条 `NotFound` 都是竞态的护栏。
    fn question(&self, target: String) -> Result<HostReply, HostError> {
        // 只借一个槽位快照:读日志尾巴不扣着 `state`。
        let live = {
            let state = self.state.lock().expect("background poisoned");
            state
                .slots
                .iter()
                .find(|slot| slot.control.session_id() == target)
                .cloned()
                .ok_or(HostError::NotFound)?
        };
        let (id, kind, payload) = {
            let projection = live
                .projection
                .lock()
                .expect("background projection poisoned");
            match projection.summary.pending.clone() {
                Some(AgentEvent::Request { id, kind, payload }) => (id, kind, payload),
                _ => return Err(HostError::NotFound),
            }
        };
        Ok(HostReply::BackgroundQuestion {
            session: live.control.session_id(),
            id,
            kind,
            payload,
            facts: read_log(&live.control, tail_of),
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
                let mut projection = live
                    .projection
                    .lock()
                    .expect("background projection poisoned");
                match projection.summary.pending.as_ref() {
                    Some(AgentEvent::Request { id: waiting, .. }) if *waiting == id => {
                        // 送不送得进去都清:送不进去是那个 runtime 已经没了,留着它只会让
                        // 列表继续说「在等」一个再也答不了的问题。
                        projection.summary.pending = None;
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
            HostCommand::ListSessions { working_dir } => {
                let mut reply = self
                    .front_control()
                    .call(HostCommand::ListSessions { working_dir })
                    .await?;
                if let HostReply::Sessions { sessions, .. } = &mut reply {
                    sessions.retain(|session| !self.hidden(&session.id));
                }
                Ok(reply)
            }
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
            HostCommand::Resume { session, target } => self.resume(session, target, false).await,
            HostCommand::ResumeAcrossProjects { session, target } => {
                self.resume(session, target, true).await
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

// Keep actionable refusals typed across the embedded-driver boundary.
fn embedded_navigation_error(error: HostError) -> atomcode_daemon::live_hub::HubError {
    use atomcode_daemon::live_hub::HubError;
    match error {
        HostError::Busy { .. } => HubError::ActiveTurn,
        // The host already wrote a person-readable reason: surface it,
        // instead of re-wrapping it in the variant's Debug shape.
        HostError::Failed { message } | HostError::InvalidWorkingDirectory { message } => {
            HubError::RuntimeRejected(message)
        }
        // A bare variant name ("NotFound", ...) told a remote client nothing
        // it could act on. Name the state instead, without asserting which
        // session is gone — the variant reaches here from the target lookup
        // AND from the source/front checks, where the session exists but the
        // front moved on (issue #1740).
        HostError::NotFound => HubError::RuntimeRejected(
            "Session not found: one of the sessions involved in this switch is no longer available. Refresh the session list and try again."
                .into(),
        ),
        HostError::SessionInUse { id } => HubError::RuntimeRejected(format!(
            "Session {id} is already open in another AtomCode window; close it there before switching."
        )),
        // Typed, so /cd renders the shared RuntimeUnavailable wording (with
        // the restart action) instead of a shorter second copy living here.
        HostError::Unavailable => HubError::RuntimeUnavailable,
        HostError::Cancelled => HubError::RuntimeRejected(
            "The session switch was cancelled before it finished.".into(),
        ),
        HostError::ProviderUnavailable { reason } => {
            let message = match reason {
                ProviderUnavailableReason::NotConfigured => {
                    "No model provider is configured. Configure one, then try the switch again."
                }
                ProviderUnavailableReason::AuthenticationRequired => {
                    "The model provider needs authentication. Sign in or set its credentials, then try again."
                }
                ProviderUnavailableReason::UnsupportedBuild => {
                    "The configured model provider is not supported on this build."
                }
                _ => "The model provider is unavailable. Check the provider setup and try again.",
            };
            HubError::RuntimeRejected(message.into())
        }
        HostError::UndoOutOfRange { requested, available } => {
            let reason = format!(
                "Undo history does not extend that far: {requested} requested, {available} available. Try again with a smaller step."
            );
            HubError::RuntimeRejected(reason)
        }
        // The session moved on since the switch was started. Typed, so /cd
        // renders the shared StaleBinding wording instead of a second copy of
        // the same "refresh and retry" sentence.
        HostError::Stale { .. } => HubError::StaleBinding,
        // A variant this mapper does not name: keep its shape out of the
        // client message — that is the symptom this function exists to
        // remove (issue #1740), and it must stay removed when
        // #[non_exhaustive] grows the enum — and its diagnostic in the log.
        error => {
            // warn, not debug: the CLI file-log filter defaults to info, and
            // this is the only place the unmapped variant and its payload are
            // kept at all.
            tracing::warn!(?error, "embedded navigation: unmapped host error");
            HubError::RuntimeRejected(
                "The switch was rejected for an unexpected reason. Try again; if it persists, open a new session."
                    .into(),
            )
        }
    }
}

#[async_trait]
impl atomcode_daemon::native_live::EmbeddedSessions for Background {
    fn owns(&self, session: &str) -> bool {
        let state = self.state.lock().expect("background poisoned");
        state.front.control.session_id() == session
            || state
                .slots
                .iter()
                .any(|slot| slot.control.session_id() == session)
    }

    fn hidden(&self, session: &str) -> bool {
        let state = self.state.lock().expect("background poisoned");
        state.slots.iter().any(|slot| {
            slot.control.session_id() == session
                && hides(&slot.mark.lock().expect("mark poisoned"), slot.initiated_by)
        })
    }

    async fn switch(
        &self,
        expected: String,
        target: String,
    ) -> Result<atomcode_daemon::native_live::Resumed, atomcode_daemon::live_hub::HubError> {
        let _op = self.op.lock().await;
        let reply = self
            .resume_locked(expected, target.clone(), true)
            .await
            .map_err(embedded_navigation_error)?;
        // The operation already committed. Do not ask for another full replay:
        // it can exceed its bound while the incoming turn is still running,
        // which must not turn a successful switch into a reported failure.
        let front = self.front_control();
        let changed = match atomcode_daemon::native_live::embedded_binding()
            .filter(|binding| binding.session_id == front.session_id())
        {
            Some(binding) => atomcode_coding::SessionChanged {
                generation: atomcode_coding::RuntimeGeneration(binding.generation),
                session_id: Some(binding.session_id),
                working_dir: binding.working_dir,
            },
            // Sharing may have been explicitly detached during preparation.
            None => atomcode_coding::SessionChanged {
                generation: atomcode_coding::RuntimeGeneration(front.runtime().status().generation),
                session_id: Some(front.session_id()),
                working_dir: front.working_dir_now().await,
            },
        };
        Ok(atomcode_daemon::native_live::Resumed {
            forked_from: matches!(reply, HostReply::DoneWithNote { .. }).then_some(target),
            changed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份把时间钉住的日志:`append` 与 `record` 盖出的 `at` 都不变,断言 `stats` 的
    /// `elapsed_ms` 才不是看运气(两条日志各记一遍同一批事实时,真实时钟会差几毫秒)。
    fn stamped_log(id: &str) -> Arc<SessionLog> {
        Arc::new(SessionLog::with_clock(
            atomcode_kernel::session::SessionHeader::new(id),
            Arc::new(atomcode_kernel::clock::FixedWallClock(1_700_000_000_000)),
        ))
    }

    /// Chunks leaving the log between two reads move later facts to lower
    /// positions; the cursor still hands over each new fact exactly once.
    #[test]
    fn the_log_cursor_misses_nothing_when_settled_chunks_leave() {
        let log = Arc::new(SessionLog::new("bg-cursor"));
        log.append(SessionEvent::TurnStart { turn: 1 });
        for _ in 0..5 {
            log.append(SessionEvent::AssistantChunk {
                turn: 1,
                round: 1,
                delta: "字".into(),
                reasoning: false,
            });
        }
        let mut cursor = LogCursor::default();
        let first = cursor.read_new(&log, |reset, events| (reset, events.len()));
        assert_eq!(first, (true, 6));

        log.append(SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: "答".into(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_blocks: vec![],
            meta: None,
        });
        log.append(SessionEvent::UserMessage {
            turn: 1,
            text: "再看看".into(),
            images: vec![],
        });
        let next = cursor.read_new(&log, |reset, events| {
            (
                reset,
                events.iter().map(|e| e.event.clone()).collect::<Vec<_>>(),
            )
        });
        assert!(!next.0, "the same log is not a reset");
        assert_eq!(next.1.len(), 2, "{:?}", next.1);
        assert!(matches!(next.1[0], SessionEvent::AssistantMessage { .. }));
        assert!(matches!(next.1[1], SessionEvent::UserMessage { .. }));
        assert!(cursor.read_new(&log, |_, events| events.is_empty()));
    }

    #[test]
    fn delegated_review_keeps_context_without_assigning_follow_up_work() {
        let text = review_task(Some("审查后修复、提交并创建 PR"), "Review staged changes");
        assert!(text.contains("ORIGINATING REQUEST (CONTEXT ONLY)"));
        assert!(text.contains("审查后修复、提交并创建 PR"));
        assert!(text.contains("ASSIGNED READ-ONLY REVIEW ===\nReview staged changes"));
        assert!(text.contains("Do not edit files, commit, push, or create pull requests"));
        assert!(review_task(None, "review task").contains("review task"));
    }

    #[test]
    fn embedded_navigation_preserves_busy_refusals() {
        use atomcode_daemon::live_hub::HubError;
        assert_eq!(
            embedded_navigation_error(HostError::Busy {
                reason: "finishing".into()
            }),
            HubError::ActiveTurn
        );
        assert!(matches!(
            embedded_navigation_error(HostError::NotFound),
            HubError::RuntimeRejected(_)
        ));
    }

    #[test]
    fn embedded_navigation_names_the_refusal_instead_of_the_variant() {
        use atomcode_daemon::live_hub::HubError;
        let reason = |error: HostError| match embedded_navigation_error(error) {
            HubError::RuntimeRejected(reason) => reason,
            other => panic!("expected RuntimeRejected, got {other:?}"),
        };
        let not_found = reason(HostError::NotFound);
        assert_ne!(not_found, "NotFound");
        assert!(not_found.contains("not found"), "{not_found}");
        assert_eq!(
            reason(HostError::Failed {
                message: "hold on".into()
            }),
            "hold on"
        );
        let in_use = reason(HostError::SessionInUse { id: "abc".into() });
        assert!(in_use.contains("abc"), "{in_use}");
        assert_eq!(
            embedded_navigation_error(HostError::Busy { reason: "x".into() }),
            HubError::ActiveTurn
        );
    }

    #[test]
    fn embedded_navigation_never_leaks_the_variant_shape() {
        use atomcode_daemon::live_hub::HubError;
        let reason = |error: HostError| match embedded_navigation_error(error) {
            HubError::RuntimeRejected(reason) => reason,
            other => panic!("expected RuntimeRejected, got {other:?}"),
        };
        let provider = reason(HostError::ProviderUnavailable {
            reason: ProviderUnavailableReason::NotConfigured,
        });
        assert!(provider.contains("No model"), "{provider}");
        assert!(!provider.contains("ProviderUnavailable"), "{provider}");
        let undo = reason(HostError::UndoOutOfRange {
            requested: 5,
            available: 2,
        });
        assert!(undo.contains("5"), "{undo}");
        assert!(!undo.contains("UndoOutOfRange"), "{undo}");
        // Unavailable/Stale stay typed: /cd renders their shared wording.
        assert_eq!(
            embedded_navigation_error(HostError::Unavailable),
            HubError::RuntimeUnavailable
        );
        assert_eq!(
            embedded_navigation_error(HostError::Stale { current: 7 }),
            HubError::StaleBinding
        );
        // The fallback names nothing it does not know, and keeps even the
        // payload out of the client message.
        let fallback = reason(HostError::CodeRewindUnavailable {
            message: "internal detail".into(),
        });
        assert!(!fallback.contains("CodeRewindUnavailable"), "{fallback}");
        assert!(!fallback.contains("internal detail"), "{fallback}");
    }

    /// 范围里有几个文件在变,是真的去问 git。
    ///
    /// 三种范围各过一遍:这个数是给人读的("这次有 2 个文件在变"),编不出来。
    #[tokio::test]
    async fn a_scope_counts_the_files_it_touches() {
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
            changed_files(&dir, "working_tree").await,
            Some(2),
            "两个还没提交的文件,包括 git 还没看见的那个"
        );
        std::fs::create_dir(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/c.txt"), "three").unwrap();
        std::fs::write(dir.join("nested/d.txt"), "four").unwrap();
        assert_eq!(changed_files(&dir, "working_tree").await, Some(4));
        git(&["add", "a.txt"]);
        assert_eq!(changed_files(&dir, "staged").await, Some(1));
        git(&["add", "b.txt"]);
        git(&["commit", "-qm", "one"]);
        std::fs::write(dir.join("b.txt"), "two again").expect("write");
        git(&["add", "b.txt"]);
        git(&["commit", "-qm", "two"]);
        assert_eq!(
            changed_files(&dir, "HEAD~1").await,
            Some(1),
            "已提交的那一段:HEAD~1..HEAD 只动了 b.txt"
        );
        // 一个选项不是 ref:交给 git 就成了 `--output=…`,它会去写那个文件。
        assert_eq!(changed_files(&dir, "--output=written").await, None);
        assert!(!dir.join("written..HEAD").exists(), "git 没把它当选项");
        // 问不出来的地方说"问不出来",而不是一个谁都没量过的零。
        assert_eq!(
            changed_files(std::path::Path::new("/"), "working_tree").await,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回合怎么结束的,面板就怎么分组——被取消的不算失败,出错的要人看。
    #[test]
    fn how_a_turn_ended_is_what_the_panel_says() {
        let mut summary = LogSummary::default();
        let ended = |reason| AgentEvent::TurnComplete {
            turn: None,
            reason,
            provider_failure: None,
        };
        summary.saw(&ended(StopReason::Cancelled));
        assert_eq!(summary.ended, Some(BackgroundState::Cancelled));
        summary.saw(&ended(StopReason::ProviderError));
        assert_eq!(summary.ended, Some(BackgroundState::Failed));
        summary.saw(&ended(StopReason::Stopped));
        assert_eq!(summary.ended, Some(BackgroundState::Done));
        // 被熔断、轮数用尽的按失败回传，不冒充成功结论。
        for reason in [
            StopReason::RepeatLoop,
            StopReason::ToolLoopDetected,
            StopReason::MaxRounds,
            StopReason::RunawayFuse,
            StopReason::PolicyDenied,
        ] {
            summary.saw(&ended(reason.clone()));
            assert_eq!(summary.ended, Some(BackgroundState::Failed), "{reason:?}");
        }
        // 日志侧的 `TurnEnd` 写的是同一个字段 —— seed 出来的旧回合也能分组。
        let mut seeded = LogSummary::default();
        seeded.absorb_event(
            0,
            &SessionEvent::TurnEnd {
                turn: 1,
                stop: StopReason::Stopped,
                error: None,
            },
            true,
        );
        assert_eq!(seeded.ended, Some(BackgroundState::Done));
    }

    fn stats_of(log: &[LoggedEvent], running_at: Option<u64>) -> Option<BackgroundStats> {
        let mut summary = LogSummary::default();
        for logged in log {
            summary.absorb_event(logged.at, &logged.event, false);
        }
        summary.stats(running_at)
    }

    #[test]
    fn background_projection_reads_new_facts_once_and_resets_on_log_replacement() {
        let usage = |prompt, cached| SessionEvent::Usage {
            turn: 1,
            round: 1,
            usage: atomcode_kernel::stream::TokenUsage {
                prompt,
                completion: 10,
                cached,
            },
        };
        let log = Arc::new(SessionLog::new("same-session"));
        log.append(SessionEvent::Titled {
            turn: 1,
            title: "Previous title".into(),
            user_set: false,
        });
        log.append(usage(100, 0));
        let mut projection = LogProjection::default();
        projection.update(Some(log.clone()));
        assert_eq!(projection.summary.stats(None).unwrap().sent, 100);

        log.append(SessionEvent::PartialReply {
            turn: 1,
            round: 1,
            text: "\n  Last reply  \nMore details".into(),
            reasoning: String::new(),
        });
        log.append(usage(250, 100));
        projection.update(Some(log.clone()));
        projection.update(Some(log.clone()));

        let stats = projection.summary.stats(None).unwrap();
        assert_eq!(
            (stats.sent, stats.sent_cached, stats.completion),
            (350, 100, 20)
        );
        assert_eq!(Some(stats), stats_of(&log.events(), None));
        assert_eq!(projection.summary.last_said.as_deref(), Some("Last reply"));
        assert_eq!(projection.summary.title.as_deref(), Some("Previous title"));

        let replacement = Arc::new(SessionLog::new("same-session"));
        for _ in 0..log.len() {
            replacement.append(usage(10, 0));
        }
        projection.update(Some(replacement.clone()));
        assert_eq!(
            projection.summary.stats(None),
            stats_of(&replacement.events(), None)
        );
        assert!(projection.summary.last_said.is_none());
        assert!(projection.summary.title.is_none());
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
        let failed = completion_report(&second, &StopReason::ProviderError);
        assert!(failed.contains("ProviderError"));
        assert!(!failed.contains("结论:两处要改"));
        let cancelled = completion_report(&second, &StopReason::Cancelled);
        assert!(cancelled.contains("Cancelled"));

        second.push(said(2, "补充:第三处也要改"));
        assert_eq!(
            last_turn_answer(&second).as_deref(),
            Some("补充:第三处也要改")
        );
        second.push(logged(SessionEvent::TurnEnd {
            turn: 2,
            stop: StopReason::PolicyDenied,
            error: Some("review budget exhausted; partial findings retained".into()),
        }));
        let failed = completion_report(&second, &StopReason::PolicyDenied);
        assert!(failed.contains("review budget exhausted"));
        assert!(failed.contains("补充:第三处也要改"));
        assert!(failed.contains("不是成功结论") || failed.contains("not a successful conclusion"));
    }

    #[test]
    fn a_new_turn_forgets_the_previous_error() {
        let mut summary = LogSummary {
            error: Some("previous failure".into()),
            ended: Some(BackgroundState::Failed),
            ..LogSummary::default()
        };
        summary.saw(&AgentEvent::TurnStarted { turn: Some(2) });
        summary.saw(&AgentEvent::TurnComplete {
            turn: Some(2),
            reason: StopReason::MaxRounds,
            provider_failure: None,
        });
        assert!(summary.error.is_none());
        assert_eq!(summary.ended, Some(BackgroundState::Failed));
    }

    /// 同一类的另一条:日志里一条还没折进来的 `TurnStart` 不能清掉运行时刚报上来的
    /// 错 —— 面板关着时它比那条 `Error` 旧,清掉之后那一行要等下一条无关 announce
    /// 才播,而这条 `Error` 正是「现在这一回合失败了」(issue #1821 #24)。
    #[test]
    fn a_late_turn_start_does_not_drop_the_error_the_runtime_just_reported() {
        let log = stamped_log("late-turn-start");
        log.append(SessionEvent::TurnStart { turn: 1 });
        log.append(SessionEvent::TurnEnd {
            turn: 1,
            stop: StopReason::Stopped,
            error: None,
        });
        log.append(SessionEvent::TurnStart { turn: 2 });

        let mut projection = LogProjection::default();
        // 还没折进来的那条 `TurnStart` 说的是过去;运行时先说了现在。
        projection.absorb(Observed::Live(&AgentEvent::Error {
            message: "provider died".into(),
            http_status: None,
            code: None,
            retryable: None,
            retry_after_secs: None,
            ends_turn: false,
        }));
        projection.update(Some(log.clone()));

        assert_eq!(
            projection.summary.error.as_deref(),
            Some("provider died"),
            "a turn start folded late describes the past, not the runtime's now"
        );
        // 日志那一半照旧折进来。
        assert_eq!(projection.summary.len, 3);
        assert_eq!(projection.summary.ended, None);
    }

    /// 同一类的另一条:日志里一条还没折进来的 `TurnEnd { error: Some }` 也不能盖掉
    /// 运行时刚报上来的错 —— 面板关着时日志只积不折,这条 `TurnEnd` 说的是上一回合
    /// 的旧错,运行时那条 `Error` 才是「现在这一回合失败了」(issue #1821 #24)。
    /// 日志侧的 `error` 只该在整份重建(seed)时写。
    #[test]
    fn a_late_turn_end_does_not_drop_the_error_the_runtime_just_reported() {
        let log = stamped_log("late-turn-end-error");
        log.append(SessionEvent::TurnStart { turn: 1 });
        log.append(SessionEvent::TurnEnd {
            turn: 1,
            stop: StopReason::ProviderError,
            error: Some("old failure from the previous turn".into()),
        });

        let mut projection = LogProjection::default();
        // 这条 `TurnEnd` 还没折进来,说的是过去;运行时先说了现在。
        projection.absorb(Observed::Live(&AgentEvent::Error {
            message: "provider died".into(),
            http_status: None,
            code: None,
            retryable: None,
            retry_after_secs: None,
            ends_turn: false,
        }));
        projection.update(Some(log.clone()));

        assert_eq!(
            projection.summary.error.as_deref(),
            Some("provider died"),
            "a turn end folded late carries the previous turn's error, not the runtime's now"
        );
        // 日志那一半照旧折进来,旧回合的结局仍然分组。
        assert_eq!(projection.summary.len, 2);
        assert_eq!(projection.summary.ended, Some(BackgroundState::Failed));

        // 整份重建时,日志侧 `error` 才代表这份日志最后的失败。
        let mut seeded = LogProjection::default();
        seeded.seed(&log);
        assert_eq!(
            seeded.summary.error.as_deref(),
            Some("old failure from the previous turn")
        );
    }

    /// 回合结束,挂着的问题就不再挂着——回到前台时不该再问一次已经作废的问题。
    #[test]
    fn a_question_is_forgotten_when_its_turn_ends() {
        let mut summary = LogSummary::default();
        summary.saw(&AgentEvent::Request {
            id: 7,
            kind: "approval".into(),
            payload: serde_json::Value::Null,
        });
        assert!(summary.pending.is_some());
        summary.saw(&AgentEvent::TurnComplete {
            provider_failure: None,
            turn: None,
            reason: StopReason::Stopped,
        });
        assert!(summary.pending.is_none());
    }

    /// 审查 Required:面板没开的时候日志只积不折,上一回合的 `TurnEnd` 要等到第一次
    /// `describe` 才折进来。它只能改日志那一半 —— `pending` 只来自运行时事件,在这里
    /// 清掉就等于把刚到的请求吃了:面板从 `Waiting` 掉回 `Running`,`question()`
    /// 永久 `NotFound`、`answer()` 回 `Stale`,提问那一侧还挂在 `rx` 上(issue #1821 #24)。
    #[test]
    fn a_late_turn_end_does_not_drop_the_request_that_arrived_first() {
        let log = stamped_log("late-turn-end");
        log.append(SessionEvent::TurnStart { turn: 1 });
        log.append(SessionEvent::UserMessage {
            turn: 1,
            text: "上一回合说过话".into(),
            images: vec![],
        });
        log.append(SessionEvent::TurnEnd {
            turn: 1,
            stop: StopReason::Stopped,
            error: None,
        });

        let mut projection = LogProjection::default();
        // 投影一次都没读过这份日志:上一回合的 `TurnEnd` 还积着。新的回合已经问上
        // 了,请求先到。
        projection.absorb(Observed::Live(&AgentEvent::Request {
            id: 7,
            kind: "approval".into(),
            payload: serde_json::Value::Null,
        }));
        // 第一次 describe:把积着的那条 `TurnEnd` 折进来。
        projection.update(Some(log.clone()));

        assert!(
            matches!(
                &projection.summary.pending,
                Some(AgentEvent::Request { id: 7, .. })
            ),
            "a turn end folded late cannot drop the request the runtime just took"
        );
        // 日志那一半照旧折对了。
        assert_eq!(projection.summary.len, 3);
        assert_eq!(projection.summary.ended, Some(BackgroundState::Done));
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

    /// #1: 同一段对话第二次委派时,第一次留下的注入事实不能算作第二次「收下了」。
    #[test]
    fn a_home_note_counts_only_after_this_deliverys_baseline() {
        use atomcode_kernel::session::InjectionOrigin;
        let note = |seq, from: &str| LoggedEvent {
            seq,
            at: 0,
            event: SessionEvent::Injected {
                turn: 1,
                text: "done".into(),
                origin: InjectionOrigin::Peer {
                    from: from.into(),
                    source_turn: None,
                    outside: true,
                },
            },
        };
        // 第一次投递留下的事实不能算作第二次「收下了」:baseline 就是它。
        let first = vec![note(3, "review-a")];
        assert!(!holds_note_from(&first, "review-a", 3));
        // 第二次投递之后新增的那条才成立;再往后看又不再算这一条。
        let both = vec![note(3, "review-a"), note(9, "review-a"), note(10, "other")];
        assert!(holds_note_from(&both, "review-a", 3));
        assert!(!holds_note_from(&both, "review-a", 9));
        assert!(!holds_note_from(&both, "review-b", 0));
    }

    /// #12: 全量 seed 与逐条增量必须得出同一份投影。
    #[test]
    fn seed_and_incremental_absorption_agree() {
        let usage = |prompt, cached| SessionEvent::Usage {
            turn: 1,
            round: 1,
            usage: atomcode_kernel::stream::TokenUsage {
                prompt,
                completion: 7,
                cached,
            },
        };
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::UserMessage {
                turn: 1,
                text: "  first words  \nsecond".into(),
                images: vec![],
            },
            SessionEvent::StepEnd {
                turn: 1,
                step: 1,
                tool_calls: 3,
            },
            SessionEvent::Usage {
                turn: 1,
                round: 1,
                usage: atomcode_kernel::stream::TokenUsage {
                    prompt: 100,
                    completion: 7,
                    cached: 20,
                },
            },
            SessionEvent::Titled {
                turn: 1,
                title: "a title".into(),
                user_set: false,
            },
            SessionEvent::PartialReply {
                turn: 1,
                round: 1,
                text: "\n  the last word  ".into(),
                reasoning: String::new(),
            },
            SessionEvent::Asked {
                turn: 1,
                question: atomcode_kernel::session::Question {
                    prompt: "Which one?".into(),
                    ..Default::default()
                },
            },
            SessionEvent::TurnEnd {
                turn: 1,
                stop: StopReason::Stopped,
                error: None,
            },
            SessionEvent::TurnStart { turn: 2 },
            SessionEvent::StepEnd {
                turn: 2,
                step: 1,
                tool_calls: 1,
            },
            usage(50, 0),
        ];

        let full = stamped_log("seed-equal");
        for event in &events {
            full.append(event.clone());
        }
        let mut seeded = LogProjection::default();
        seeded.seed(&full);

        let incremental_log = stamped_log("seed-equal");
        let mut incremental = LogProjection::default();
        for event in &events {
            incremental_log.append(event.clone());
            incremental.update(Some(incremental_log.clone()));
        }

        let shape = |s: &LogSummary| {
            (
                format!(
                    "{} {:?} {:?} {:?} {:?} {} {:?} {}",
                    s.len,
                    s.title,
                    s.first_user,
                    s.last_said,
                    s.asked,
                    s.has_user_message,
                    s.first_at,
                    s.last_at
                ),
                s.last_at,
                (s.steps, s.tools, s.completion, s.sent, s.sent_cached),
                format!("{:?}", s.ended),
            )
        };
        assert_eq!(shape(&seeded.summary), shape(&incremental.summary));
        assert_eq!(seeded.summary.len, events.len() as u64);
        assert_eq!(seeded.summary.stats(None), incremental.summary.stats(None));
        // 第二个回合开始后,第一个回合的结局不再代表现在。
        assert_eq!(seeded.summary.ended, None);
    }

    /// #12: 同一条事实在通道上(`Fact`)与日志里各来一次,只能折一次。
    ///
    /// pump 与 `describe` 面对的是同一个前端、同一份日志:屏幕一订阅
    /// (`Subscribe { from: 0 }`),feed 就把整段历史逐条推成 `Fact`,新提交的事实
    /// 也从这里过来;回到后台后 `update` 又按 cursor 从日志折一遍。两边都折,面板的
    /// 步数/工具/token 就会把每条事实记两遍。渠道喂养与全量 seed 必须等值。
    #[test]
    fn a_fact_on_the_channel_is_folded_once_with_its_log_record() {
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::UserMessage {
                turn: 1,
                text: "  first words  \nsecond".into(),
                images: vec![],
            },
            SessionEvent::StepEnd {
                turn: 1,
                step: 1,
                tool_calls: 3,
            },
            SessionEvent::Usage {
                turn: 1,
                round: 1,
                usage: atomcode_kernel::stream::TokenUsage {
                    prompt: 100,
                    completion: 7,
                    cached: 20,
                },
            },
            SessionEvent::PartialReply {
                turn: 1,
                round: 1,
                text: "\n  the last word  ".into(),
                reasoning: String::new(),
            },
            SessionEvent::TurnEnd {
                turn: 1,
                stop: StopReason::Stopped,
                error: None,
            },
        ];

        let full = stamped_log("fact-once");
        for event in &events {
            full.append(event.clone());
        }
        let mut seeded = LogProjection::default();
        seeded.seed(&full);

        // 混合喂养:每条事实先在通道上到一次(订阅重放或新提交),再由日志增量折
        // 一次 —— 这正是会话在前台被订阅期间发生的事。
        let mixed_log = stamped_log("fact-once");
        let mut mixed = LogProjection::default();
        for event in &events {
            let (seq, at) = mixed_log.record(event.clone());
            let fact = AgentEvent::Fact(Box::new(atomcode_kernel::session::Committed {
                session: "fact-once".into(),
                seq,
                at,
                event: event.clone(),
            }));
            assert!(
                !mixed.absorb(Observed::Live(&fact)),
                "a fact on the channel says nothing the log does not: {seq}"
            );
            mixed.update(Some(mixed_log.clone()));
        }

        assert_eq!(seeded.summary.len, events.len() as u64);
        assert_eq!(
            mixed.summary.len, seeded.summary.len,
            "a fact is folded once, from the log"
        );
        assert_eq!(mixed.summary.stats(None), seeded.summary.stats(None));
        assert_eq!(mixed.summary.first_user, seeded.summary.first_user);
        assert_eq!(mixed.summary.last_said, seeded.summary.last_said);
        assert_eq!(
            format!("{:?}", mixed.summary.ended),
            format!("{:?}", seeded.summary.ended)
        );
    }

    /// 日志暂时读不到(App 正在重建、`Feed` 还没找到这个会话)时,`update(None)` 不能把
    /// 只来自运行时事件的字段一起清掉:提问不在日志里,重订阅也不会再补发。日志回来
    /// 时按 `seen` 续读 —— 重建期间新增的事实一条不落,已折过的字段也不会空着。
    #[test]
    fn an_unreadable_log_keeps_the_request_only_the_runtime_knows() {
        let log = stamped_log("keeps-pending");
        log.append(SessionEvent::TurnStart { turn: 1 });
        log.append(SessionEvent::UserMessage {
            turn: 1,
            text: "  first words  ".into(),
            images: vec![],
        });
        log.append(SessionEvent::StepEnd {
            turn: 1,
            step: 1,
            tool_calls: 3,
        });
        let mut projection = LogProjection::default();
        projection.update(Some(log.clone()));
        projection.absorb(Observed::Live(&AgentEvent::Request {
            id: 7,
            kind: "approval".into(),
            payload: serde_json::Value::Null,
        }));

        // App 正在重建:日志读不到。
        projection.update(None);
        assert!(
            matches!(
                &projection.summary.pending,
                Some(AgentEvent::Request { id: 7, .. })
            ),
            "a request that never reaches the log must survive an unreadable log"
        );
        assert_eq!(
            projection.summary.first_user.as_deref(),
            Some("first words"),
            "the log half is still the last thing the log said"
        );

        // 重建期间日志又长了一条:回来时把它折进来,已折过的不重复。
        log.append(SessionEvent::StepEnd {
            turn: 1,
            step: 2,
            tool_calls: 2,
        });
        projection.update(Some(log.clone()));
        let mut seeded = LogProjection::default();
        seeded.seed(&log);
        assert_eq!(projection.summary.len, seeded.summary.len);
        assert_eq!(
            projection.summary.stats(None),
            seeded.summary.stats(None),
            "the facts that arrived while the log was unreadable are folded once, not lost"
        );
        assert!(projection.summary.has_user_message);
        assert!(projection.summary.pending.is_some());
    }

    /// 换了日志(重建后的 App 拿来另一份,`read_new` 判 reset)时,日志那一半从头重建,
    /// 只来自运行时事件的 `pending`/`error` 仍要在 —— 它们同样补不回来。
    #[test]
    fn a_reset_log_rebuilds_the_log_half_and_keeps_the_runtime_half() {
        let old = stamped_log("reset");
        old.append(SessionEvent::TurnStart { turn: 1 });
        old.append(SessionEvent::UserMessage {
            turn: 1,
            text: "old words".into(),
            images: vec![],
        });
        old.append(SessionEvent::StepEnd {
            turn: 1,
            step: 1,
            tool_calls: 3,
        });
        let mut projection = LogProjection::default();
        projection.update(Some(old.clone()));
        projection.absorb(Observed::Live(&AgentEvent::Request {
            id: 9,
            kind: "question".into(),
            payload: serde_json::Value::Null,
        }));
        projection.absorb(Observed::Live(&AgentEvent::Error {
            message: "provider died".into(),
            http_status: None,
            code: None,
            retryable: None,
            retry_after_secs: None,
            ends_turn: false,
        }));
        assert_eq!(projection.summary.steps, 1);

        let rebuilt = stamped_log("reset");
        rebuilt.append(SessionEvent::UserMessage {
            turn: 2,
            text: "rebuilt words".into(),
            images: vec![],
        });
        rebuilt.append(SessionEvent::StepEnd {
            turn: 2,
            step: 1,
            tool_calls: 1,
        });
        projection.update(Some(rebuilt.clone()));

        assert!(
            matches!(
                &projection.summary.pending,
                Some(AgentEvent::Request { id: 9, .. })
            ),
            "a reset rebuilds the log half, not the request"
        );
        assert_eq!(projection.summary.error.as_deref(), Some("provider died"));
        assert_eq!(
            projection.summary.first_user.as_deref(),
            Some("rebuilt words"),
            "the log half comes from the log that is there now"
        );
        assert_eq!(projection.summary.len, 2);
        assert_eq!((projection.summary.steps, projection.summary.tools), (1, 1));
        let mut seeded = LogProjection::default();
        seeded.seed(&rebuilt);
        assert_eq!(projection.summary.stats(None), seeded.summary.stats(None));
        assert!(projection.summary.has_user_message);
    }

    /// 审查 Low:`saw(AgentEvent::Error)` 改写了面板那一行,就要说「会变」——
    /// 没有 `TurnComplete` 的收尾路径不会再有第二条事件来播它。
    #[test]
    fn an_error_asks_for_the_panel_line_to_be_repainted() {
        let mut summary = LogSummary::default();
        assert!(summary.saw(&AgentEvent::Error {
            message: "provider died".into(),
            http_status: Some(500),
            code: None,
            retryable: None,
            retry_after_secs: None,
            ends_turn: true,
        }));
        assert_eq!(summary.error.as_deref(), Some("provider died"));
    }

    /// #23: `(stopped, phase, ended, pending)` 的每种组合都有明确答案。
    #[test]
    fn the_panel_state_matrix_is_explicit() {
        use RuntimePhase::*;
        let done = Some(BackgroundState::Done);
        assert_eq!(
            state_for(true, InTurn, done, true),
            BackgroundState::Failed,
            "a retired runtime is failed whatever the phase says"
        );
        assert_eq!(
            state_for(false, WaitingApproval, None, true),
            BackgroundState::Waiting
        );
        assert_eq!(
            state_for(false, WaitingApproval, done, false),
            BackgroundState::Done
        );
        assert_eq!(
            state_for(false, WaitingApproval, None, false),
            BackgroundState::Running
        );
        assert_eq!(
            state_for(false, InTurn, None, false),
            BackgroundState::Running
        );
        assert_eq!(
            state_for(false, InTurn, Some(BackgroundState::Cancelled), false),
            BackgroundState::Cancelled
        );
        for phase in [Failed, Stopped, ShuttingDown] {
            assert_eq!(
                state_for(false, phase, done, false),
                BackgroundState::Failed,
                "{phase:?}"
            );
        }
        for phase in [Ready, Reconfiguring, AwaitingProvider] {
            assert_eq!(
                state_for(false, phase, None, false),
                BackgroundState::Idle,
                "{phase:?}"
            );
            assert_eq!(
                state_for(false, phase, done, false),
                BackgroundState::Done,
                "{phase:?}"
            );
        }
    }

    /// #3: 只有「工具发起且结果还没落地」的审查才隐藏;人发起的 scoped 任务
    /// 在 Pending 期间照常出现在 `/resume`。
    #[test]
    fn a_session_is_hidden_only_while_a_tool_review_is_unopened() {
        let dir = PathBuf::from("/tmp/project");
        assert!(hides(&Mark::Home(dir.clone()), InitiatedBy::Person));
        assert!(hides(&Mark::Home(dir.clone()), InitiatedBy::Tool));
        assert!(!hides(&Mark::Pending(dir.clone()), InitiatedBy::Person));
        assert!(hides(&Mark::Pending(dir.clone()), InitiatedBy::Tool));
        assert!(!hides(&Mark::Own, InitiatedBy::Tool));
        assert!(!hides(&Mark::TakenUp, InitiatedBy::Tool));
    }
}
