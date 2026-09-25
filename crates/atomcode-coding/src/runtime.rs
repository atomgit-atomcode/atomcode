//! Stable driver control plane and kernel-agent owner for a coding runtime.
//!
//! The runtime owns the replaceable kernel [`AgentHandle`] so native controls and
//! events never need to traverse a legacy driver adapter.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use atomcode_capabilities::session::snapshot::SnapshotPersistenceStatus;
use atomcode_capabilities::session::{
    DisplayAnchor, PresentationEntry, RewindPoint, RewindTransactionReceipt, SessionLease,
    SessionStoreError, TurnStat,
};
#[cfg(test)]
use atomcode_capabilities::session::{PresentationFile, SessionMeta, StorageOwner};
use atomcode_capabilities::tools::request_user_input::{
    UserInputResponse, REQUEST_USER_INPUT_KIND,
};
use atomcode_capabilities::tools::{ApprovalResponse, APPROVAL_KIND};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::checkpoint::CompactionCheckpointError;
use atomcode_kernel::event::{
    AgentCommand, AgentEvent, PolicyIntervention, PolicyRecoveryAction, RequestId, StopReason,
};
pub use atomcode_kernel::message::CompactTrigger;
use atomcode_kernel::message::{
    CompactionStrategy, CompactionView, Conversation, ImageContent, Message, MessageMeta,
    SessionSnapshot,
};
use atomcode_kernel::provider::LlmProvider;
use tokio::sync::{mpsc, oneshot, watch};

use crate::controllers::{
    evaluate_goal, goal_cap_stop_note, goal_continuation_message, summarize_for_goal, EvalOutcome,
    GoalPhase, GoalProgress, GoalResult, GoalState, GoalTerminal, LoopProgress, LoopState,
    ScheduleWakeupTool, WakeupRequest, MAX_UNPRODUCTIVE,
};
use crate::parts::prepare_with_plugin_hook_source_reusing_lease;
#[cfg(test)]
use crate::prepare_with_plugin_hook_source;
use crate::{CodingAgentConfig, CodingProviderFactory, PluginHookSource, PrepareOptions};

/// Runtime facts emitted by the coding engine without depending on the legacy
/// `atomcode-core` driver protocol.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum CodingRuntimeEvent {
    /// A kernel observation that is not owned as a runtime terminal/request.
    Agent(AgentEvent),
    /// Structured Team Agent lifecycle projection. Drivers consume this typed
    /// event instead of parsing tool output or progress strings.
    Team {
        generation: RuntimeGeneration,
        event: atomcode_capabilities::team::TeamEvent,
    },
    /// Vision (VL) preprocessing recognised the turn's image(s) — the driver
    /// renders a "✓ VL recognised image, returned N chars" status line.
    VisionPreprocessSuccess {
        vl_model: String,
        char_count: usize,
    },
    /// Vision (VL) preprocessing failed — the driver surfaces a warning and
    /// re-attaches the images it remembers from submit so the user can retry
    /// without re-pasting from the clipboard.
    VisionPreprocessFailed {
        reason: String,
    },
    /// Driver-correlation acknowledgement for inputs accepted as in-turn
    /// steers. Unlike the kernel `AgentEvent::Steered`, these are the original
    /// inputs submitted at the runtime boundary, before local-context or vision
    /// preprocessing rewrites them.
    SteerAcknowledged {
        inputs: Vec<UserInput>,
    },
    /// A potentially slow compaction strategy has started.
    CompactionStarted {
        trigger: CompactTrigger,
    },
    /// A compaction attempt reached exactly one terminal state.
    CompactionFinished {
        completion: CompactionCompletion,
    },
    /// Exactly-once runtime terminal on the native event stream.
    RuntimeStopped(RuntimeExit),
    /// Driver-correlated approval or other middleware request.
    Request(RuntimeRequest),
    /// Exactly one terminal for an accepted foreground turn.
    TurnFinished(TurnCompletion),
    /// A driver acknowledged the runtime's pending security intervention.
    /// This is control-plane state only and is never appended to conversation input.
    PolicyInterventionResolved {
        intervention_id: u64,
        action: PolicyRecoveryAction,
    },
    /// The runtime invalidated a pending intervention because its owning
    /// conversation was replaced. Drivers must remove only the matching UI.
    PolicyInterventionCleared {
        intervention_id: u64,
    },
    ModeChanged {
        mode: RuntimeMode,
    },
    Reconfiguring {
        operation: ReconfigureKind,
    },
    Reconfigured {
        operation: ReconfigureKind,
    },
    ProviderChanged {
        provider: String,
        model: String,
    },
    /// The active runtime adopted a new reasoning-effort setting. This is
    /// separate from `ProviderChanged`: effort can change while provider/model
    /// identity stays the same.
    ReasoningEffortChanged {
        provider: String,
        effort: Option<atomcode_kernel::provider::ReasoningEffort>,
        applicable: bool,
    },
    ProviderUnavailable {
        reason: ProviderUnavailableReason,
        forced: bool,
    },
    SessionNameSuggested {
        name: String,
    },
    /// Ephemeral composer suggestion sampled after a naturally completed turn.
    /// It is not part of the conversation or session persistence. Drivers must
    /// discard it when the correlated generation/session/turn is no longer current.
    NextPromptSuggested {
        generation: RuntimeGeneration,
        session_id: Option<String>,
        turn_id: u64,
        text: String,
    },
    SessionChanged(SessionChanged),
    WorkingDirectoryChanged(std::path::PathBuf),
    GoalChanged(GoalProgress),
    LoopChanged(LoopProgress),
    UndoFinished(Result<UndoResult, RuntimeError>),
    RewindCatalogRefreshed(Result<RewindCatalog, RuntimeError>),
    RewindFinished(Result<RewindResult, RuntimeError>),
    ContextStatsRefreshed(Result<RuntimeContextStats, RuntimeError>),
    SnapshotRestoreFinished {
        correlation_id: u64,
        result: Result<Arc<SessionSnapshot>, RuntimeError>,
    },
    SessionResumeFinished(Result<SessionChanged, RuntimeError>),
    ProviderReloadFinished(Result<RuntimeGeneration, RuntimeError>),
    ProviderDeactivationFinished(Result<RuntimeGeneration, RuntimeError>),
    ControllerWarning(String),
    /// Non-fatal failure of an auxiliary persistence surface such as the raw
    /// per-turn transcript. Drivers must present this outside the conversation;
    /// it is not model output and does not change the turn terminal.
    PersistenceWarning(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconfigureKind {
    Provider,
    Reprepare,
    FreshSession,
    ResumeSession,
    ChangeDirectory,
    RestoreSession,
    Undo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeGeneration(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub struct McpStatusSnapshot {
    pub generation: RuntimeGeneration,
    pub servers: Vec<(String, atomcode_capabilities::mcp::ServerStatus)>,
}

/// An MCP tool made "always allowed" by [`CodingRuntimeHandle::approve_mcp_tool`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpToolApproval {
    pub server: String,
    /// The tool's own name on its server — what `autoApprove` lists.
    pub tool: String,
    /// Why the project file was not written, when it was not. The session
    /// grant holds either way; only the next session would ask again.
    pub persist_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpToolsSnapshot {
    pub generation: RuntimeGeneration,
    pub server: String,
    pub status: Option<atomcode_capabilities::mcp::ServerStatus>,
    pub tools: Vec<String>,
    /// Every configured server key (sorted), so a caller can tell an UNKNOWN key
    /// (`status == None`) apart from a configured-but-empty one and suggest the
    /// real names — shell-style "not found → here's what exists".
    pub available: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpRowsSnapshot {
    pub generation: RuntimeGeneration,
    pub rows: Vec<crate::parts::McpRowFacts>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpDetailSnapshot {
    pub generation: RuntimeGeneration,
    /// `None` when no configured server has that key. Not an error: a screen
    /// says "no such server" and lists what there is.
    pub detail: Option<crate::parts::McpRowFacts>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionChanged {
    pub generation: RuntimeGeneration,
    pub session_id: Option<String>,
    pub working_dir: std::path::PathBuf,
}

#[derive(Clone)]
pub struct ReprepareInput {
    pub config: CodingAgentConfig,
    pub prepare: PrepareOptions,
    pub operation: ReconfigureKind,
}

#[derive(Clone, Debug)]
pub struct UndoResult {
    pub generation: RuntimeGeneration,
    pub snapshot: Arc<SessionSnapshot>,
    pub restored_prompt: String,
    pub target_n: usize,
    pub prompts_before: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewindScope {
    Conversation,
    Code,
    ConversationAndCode,
}

impl RewindScope {
    fn restores_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::ConversationAndCode)
    }

    fn restores_code(self) -> bool {
        matches!(self, Self::Code | Self::ConversationAndCode)
    }
}

/// Which two things `/diff` compares.
///
/// The runtime's own word for it; [`atomcode_host_api::ChangeScope`] is the
/// wire's, and the mapping between them is the host adapter's — a runtime that
/// imported the contract would be a runtime that could only serve one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceScope {
    /// Before this session's first prompt, against now.
    #[default]
    Session,
    /// The checkout, against `HEAD`.
    Git,
}

/// Whether this session is driving itself, and how far it has got.
///
/// Read rather than pushed: the runtime already publishes `GoalChanged` /
/// `LoopChanged` every round, but that stream is the runtime's own and the
/// screen is not on it.
///
/// Answered through host control, the way `McpStatus` is. `docs/adr/0021` §3
/// puts goal and loop with the capability rows and that is where their
/// *commands* are — `GoalCommand` is a shim over `RuntimeCommands`. The state
/// is not theirs: these controllers are runtime-owned (`controllers.rs`: "
/// Runtime-owned autonomous controllers"), they live as locals of the driver
/// loop, and reporting what the runtime owns is the host's job. MCP is the same
/// shape: rows mount the servers, host control reports their state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Autonomy {
    pub goal: Option<GoalProgress>,
    pub looping: Option<LoopProgress>,
}

/// What a session has done to the workspace, for a front end to show.
///
/// One shape for both levels of the answer: the list of files, or one file's
/// diff. Two messages that differed only in which field was filled would be two
/// round trips to keep in step.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceChanges {
    /// Every file this session changed, with how much.
    pub files: Vec<atomcode_capabilities::session::FileChangeSummary>,
    /// What each of them is, when the scope knows — the `git` scope reads an
    /// index and so can say `staged`/`modified`; the session's own diff is two
    /// trees compared and has no index to ask.
    ///
    /// Parallel to `files` rather than folded into `FileChangeSummary` because
    /// that type is the checkpoint store's, and the checkpoint store has no
    /// opinion about anybody's index.
    pub states: Vec<Option<(atomcode_capabilities::worktree_status::Status, bool)>>,
    /// The unified diff of the one file that was asked for.
    pub diff: Option<String>,
    /// Why there is no answer, when there is none. Not an error: a session with
    /// no workspace checkpointing is an ordinary session, and the screen has to
    /// say which of "nothing changed" and "cannot tell" it is.
    pub unavailable: Option<String>,
}

/// The checkout's own changes, as [`WorkspaceScope::Git`] asks for them.
///
/// Every failure is an answer rather than an error: `/diff git` outside a
/// repository has to **say** so, because an empty list reads as "nothing
/// changed" and that is a different thing to be told.
fn git_workspace_changes(at: &std::path::Path, file: Option<&str>) -> WorkspaceChanges {
    use atomcode_capabilities::worktree_status as git;
    if let Some(path) = file {
        return match git::file_diff(at, path) {
            Ok(diff) => WorkspaceChanges {
                diff: Some(diff),
                ..Default::default()
            },
            Err(why) => WorkspaceChanges {
                unavailable: Some(why),
                ..Default::default()
            },
        };
    }
    match git::read(at) {
        Ok(found) => {
            let mut files = Vec::with_capacity(found.len());
            let mut states = Vec::with_capacity(found.len());
            for (file, added, removed, binary) in found {
                states.push(
                    file.unstaged
                        .or(file.staged)
                        .map(|status| (status, file.is_staged())),
                );
                files.push(atomcode_capabilities::session::FileChangeSummary {
                    path: file.path,
                    additions: added,
                    deletions: removed,
                    binary,
                });
            }
            WorkspaceChanges {
                files,
                states,
                ..Default::default()
            }
        }
        Err(why) => WorkspaceChanges {
            unavailable: Some(why),
            ..Default::default()
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindCatalog {
    pub generation: RuntimeGeneration,
    pub revision: u64,
    pub points: Vec<RewindPoint>,
    pub code_unavailable: Option<CodeUnavailable>,
}

/// Why the workspace half of a rewind is not on offer.
///
/// **A kind, not a sentence** — the words belong to whoever is talking to the
/// person, and a front end that was handed a sentence could only pass it
/// through in whatever language this crate happened to write it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeUnavailable {
    /// Off by default, to protect disk space. The person can opt in.
    NotEnabled,
    /// This session is not written down, so there is nothing to checkpoint
    /// against.
    NoSession,
    /// Opted in, but the checkpoint could not be set up — with the cause.
    SetupFailed(String),
}

impl CodeUnavailable {
    /// A reason string, for a caller that has nowhere to put the kind.
    pub fn say(&self) -> String {
        match self {
            Self::NotEnabled => {
                atomcode_capabilities::session::CodeRewindUnavailable::NotEnabled.to_string()
            }
            Self::NoSession => "rewind requires a persistent session".to_string(),
            Self::SetupFailed(why) => {
                atomcode_capabilities::session::CodeRewindUnavailable::SetupFailed(why.clone())
                    .to_string()
            }
        }
    }
}

impl From<atomcode_capabilities::session::CodeRewindUnavailable> for CodeUnavailable {
    fn from(why: atomcode_capabilities::session::CodeRewindUnavailable) -> Self {
        use atomcode_capabilities::session::CodeRewindUnavailable as Why;
        match why {
            Why::NotEnabled => Self::NotEnabled,
            Why::SetupFailed(message) => Self::SetupFailed(message),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RewindResult {
    pub generation: RuntimeGeneration,
    pub scope: RewindScope,
    pub point: RewindPoint,
    pub snapshot: Arc<SessionSnapshot>,
    pub restored_prompt: Option<String>,
    pub restored_files: Vec<String>,
}

/// Internal ownership token carried by [`CodingRuntimeControl::BeginRewind`].
///
/// Public only because the driver control protocol is public; callers should use
/// [`CodingRuntimeHandle::rewind_from_catalog`] rather than construct or inspect it.
#[doc(hidden)]
pub struct RewindTransactionGuard {
    tx: mpsc::UnboundedSender<CodingRuntimeControl>,
    generation: u64,
    receipt: Option<RewindTransactionReceipt>,
}

impl RewindTransactionGuard {
    fn new(
        tx: mpsc::UnboundedSender<CodingRuntimeControl>,
        generation: u64,
        receipt: RewindTransactionReceipt,
    ) -> Self {
        Self {
            tx,
            generation,
            receipt: Some(receipt),
        }
    }

    fn receipt(&self) -> &RewindTransactionReceipt {
        self.receipt
            .as_ref()
            .expect("active rewind transaction has a receipt")
    }

    fn commit(mut self) -> RewindTransactionReceipt {
        self.receipt
            .take()
            .expect("active rewind transaction has a receipt")
    }

    fn take_for_compensation(&mut self) -> RewindTransactionReceipt {
        self.receipt
            .take()
            .expect("active rewind transaction has a receipt")
    }
}

impl Drop for RewindTransactionGuard {
    fn drop(&mut self) {
        let Some(receipt) = self.receipt.take() else {
            return;
        };
        let (done, _result) = oneshot::channel();
        let _ = self.tx.send(CodingRuntimeControl::FinishRewind {
            generation: self.generation,
            receipt,
            outcome: RewindFinalization::Recover,
            done,
        });
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotUndoResult {
    pub snapshot: SessionSnapshot,
    pub restored_prompt: String,
    pub target_n: usize,
    pub prompts_before: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    #[default]
    Build,
    #[serde(rename = "accept_edits")]
    AcceptEdits,
    #[serde(rename = "bypass")]
    Auto,
    Plan,
}

impl RuntimeMode {
    pub fn next(self) -> Self {
        match self {
            Self::Build => Self::AcceptEdits,
            Self::AcceptEdits => Self::Auto,
            Self::Auto => Self::Plan,
            Self::Plan => Self::Build,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Build => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto => Self::AcceptEdits,
            Self::AcceptEdits => Self::Build,
        }
    }

    pub fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    pub fn is_accept_edits(self) -> bool {
        matches!(self, Self::AcceptEdits)
    }

    pub fn to_flags(self) -> (bool, bool, bool) {
        match self {
            Self::Build => (false, false, false),
            Self::AcceptEdits => (false, false, true),
            Self::Auto => (false, true, false),
            Self::Plan => (true, false, false),
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::AcceptEdits => "accept_edits",
            Self::Auto => "bypass",
            Self::Plan => "plan",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::AcceptEdits => "AcceptEdits",
            Self::Auto => "Auto",
            Self::Plan => "Plan",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeContextStats {
    pub context_window: u32,
    pub used_tokens: u32,
    pub utilization: f32,
    pub model: String,
    pub working_dir: std::path::PathBuf,
    /// 这个会话跑在哪份系统提示词上 —— 只有问了才带。
    ///
    /// **只有问了才带**,因为它长:装了技能与项目说明的会话上是几千字,而
    /// 每个别的调用方要的都是一个数。人想看它的那一刻很具体:agent 表现得
    /// 像是被告知了一件谁也不记得告诉过它的事。
    pub system_prompt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalContextInput {
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeRequest {
    pub id: RequestId,
    pub kind: String,
    pub payload: serde_json::Value,
    pub snapshot: Option<Arc<SessionSnapshot>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuntimeTurnStats {
    pub last_usage: Option<MessageMeta>,
    /// Sum of provider usage across every model round in this user turn.
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cached_tokens: usize,
    pub duration: std::time::Duration,
    pub turn_count: usize,
    pub tool_call_count: usize,
}

impl RuntimeTurnStats {
    fn record_usage(&mut self, meta: &MessageMeta) {
        self.turn_count = self.turn_count.saturating_add(1);
        self.prompt_tokens = self
            .prompt_tokens
            .saturating_add(meta.tokens.prompt as usize);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(meta.tokens.completion as usize);
        self.cached_tokens = self
            .cached_tokens
            .saturating_add(meta.tokens.cached as usize);
        self.last_usage = Some(meta.clone());
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnCompletion {
    Completed {
        turn_id: u64,
        reason: StopReason,
        snapshot: Arc<SessionSnapshot>,
        stats: RuntimeTurnStats,
    },
    SnapshotUnavailable {
        turn_id: u64,
        reason: StopReason,
        error: RuntimeSnapshotError,
        stats: RuntimeTurnStats,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSnapshotError {
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<ImageContent>,
}

/// One accepted in-turn submit, before and after runtime-owned preprocessing.
///
/// Kernel `Steered` events necessarily describe the input that reached the
/// kernel. Drivers, however, correlate those acknowledgements with the input
/// they submitted. Vision preprocessing and pending local context can rewrite
/// that payload, so the runtime owner keeps the two projections together until
/// the kernel confirms the fold.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSteerAcknowledgement {
    generation: u64,
    original: UserInput,
    forwarded: UserInput,
}

fn acknowledge_steered_inputs(
    pending: &mut VecDeque<PendingSteerAcknowledgement>,
    generation: u64,
    inputs: &[atomcode_kernel::event::SteeredInput],
) -> Vec<UserInput> {
    while pending
        .front()
        .is_some_and(|entry| entry.generation != generation)
    {
        pending.pop_front();
    }

    inputs
        .iter()
        .filter_map(|input| {
            let matches_front = pending.front().is_some_and(|entry| {
                entry.forwarded.text == input.text && entry.forwarded.images == input.images
            });
            if !matches_front {
                return None;
            }

            Some(
                pending
                    .pop_front()
                    .expect("front was checked above")
                    .original,
            )
        })
        .collect()
}

fn forwarded_steer_for_acknowledgement(
    original: Option<&UserInput>,
    command: &AgentCommand,
) -> Option<UserInput> {
    match (original, command) {
        (Some(_), AgentCommand::SendMessage { text, images }) => Some(UserInput {
            text: text.clone(),
            images: images.clone(),
        }),
        _ => None,
    }
}

/// 账户还剩多少 —— 以及「问不到」这件事本身。
///
/// **`windows` 空有两个意思,而它们必须分得开。** 一个是「这个宿主根本不计
/// 额度」(没有账户服务),另一个是「问了,没答上来」(超时、网络断、服务在
/// 抽风)。把后者画成前者,屏幕上写的就是事实的反面——一个正被额度挡住的人
/// 会读到「不计额度」,然后去别处找原因。所以问不到的时候 `unavailable`
/// 说为什么,而那时 `windows` 的空不代表任何事。
#[derive(Clone, Debug, Default)]
pub struct Allowance {
    /// 一个窗口一行。空且 `unavailable` 也空 = 真的不计额度。
    pub windows: Vec<crate::rate_limit::RateLimitWindow>,
    /// 窗口背后的套餐,服务说得出的话。
    pub plan: Option<crate::rate_limit::Entitlement>,
    /// 已经花掉的,服务记得的话。
    pub spent: Option<crate::rate_limit::AccountUsage>,
    /// 问不到的时候,为什么。
    pub unavailable: Option<String>,
}

/// 问到了就是那些窗口;没问到是空的,而空由 [`why_not`] 解释。
fn windows_or_nothing(
    asked: &Result<
        Result<Vec<crate::rate_limit::RateLimitWindow>, String>,
        tokio::time::error::Elapsed,
    >,
) -> Vec<crate::rate_limit::RateLimitWindow> {
    match asked {
        Ok(Ok(windows)) => windows.clone(),
        _ => Vec::new(),
    }
}

/// 为什么没有答案。`None` = 有答案(哪怕答案是「一个窗口都没有」)。
fn why_not(
    asked: &Result<
        Result<Vec<crate::rate_limit::RateLimitWindow>, String>,
        tokio::time::error::Elapsed,
    >,
) -> Option<String> {
    match asked {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(error.clone()),
        Err(elapsed) => Some(elapsed.to_string()),
    }
}

/// Ordered, fire-and-forget driver requests. This is the native replacement for
/// the core AgentCommand channel during asynchronous runtime startup.
#[derive(Clone, Debug)]
pub enum DriverCommand {
    Submit(UserInput),
    Respond {
        id: RequestId,
        value: serde_json::Value,
    },
    ResolvePolicyIntervention {
        intervention_id: u64,
        action: PolicyRecoveryAction,
    },
    Cancel,
    PauseGoal,
    Compact(Option<String>),
    SetMode(RuntimeMode),
    QueueLocalContext(LocalContextInput),
    ReloadProvider(CodingAgentConfig),
    ReprepareConfig(CodingAgentConfig),
    DeactivateProvider(ProviderUnavailableReason),
    UndoToPrompt(Option<usize>),
    Rewind {
        turn_id: u64,
        scope: RewindScope,
    },
    RefreshContextStats,
    RestoreSnapshot(SessionSnapshot),
    RestoreSnapshotCorrelated {
        snapshot: SessionSnapshot,
        correlation_id: u64,
    },
    /// Work towards this on its own until it holds.
    ///
    /// The text is the condition **and** the first round's prompt — the
    /// runtime opens that round itself. Images ride along because a goal is
    /// often given as "make it look like this".
    StartGoal(UserInput),
    StopGoal,
    StartLoop(String),
    StopLoop,
    Shutdown,
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

/// What a capability row asks the runtime to do on a person's word
/// (`docs/adr/0021` §3, §10).
///
/// Narrow on purpose: the row is mounted in the agent's tree and must not grow
/// a dependency on the whole driver protocol, which is an implementation of the
/// transition and not a contract (§5). Everything here is something a person
/// runs from a front end — never a tool the model can reach.
#[async_trait::async_trait]
pub trait RuntimeCommands: Send + Sync {
    /// Work towards `condition` on its own until it holds.
    async fn start_goal(&self, condition: String) -> Result<(), String>;
    /// Stop the goal that is running.
    async fn stop_goal(&self) -> Result<(), String>;
    /// Leave the goal where it is; it can be taken up again.
    async fn pause_goal(&self) -> Result<(), String>;
    /// Run `prompt` again and again until it is stopped.
    ///
    /// `every` is the person's own cadence, in seconds. `None` leaves the
    /// pacing to the model — it asks for the next round with `schedule_wakeup`,
    /// and a round it does not ask after is the end of the loop. With a cadence
    /// the rounds keep coming whether the model asks or not, which is what
    /// "every five minutes" means and the reason the two are one command rather
    /// than two.
    async fn start_loop(&self, prompt: String, every: Option<u32>) -> Result<(), String>;
    async fn stop_loop(&self) -> Result<(), String>;
    /// Put `text` in front of the next turn, as the person's own context.
    async fn queue_local_context(&self, text: String) -> Result<(), String>;
    /// The policy intervention waiting for a person to say how to go on, if one
    /// is. The row asks before it resolves: whether there is one, and whether
    /// what a person typed is among its choices, are the row's two judgements
    /// to make (`docs/adr/0021` §8) — the host contract has no say in them.
    async fn pending_policy(&self) -> Option<PolicyIntervention>;
    /// Go on from the intervention `id` the way `action` says.
    async fn resolve_policy(&self, id: u64, action: PolicyRecoveryAction) -> Result<(), String>;
    /// Point the runtime at `directory` — a new session in the same place, with
    /// everything that belonged to where it ran rebuilt for there.
    ///
    /// The runtime's own transition (`docs/adr/0001`), awaited rather than
    /// raced: a row that had to *make* the directory first (`/worktree`) cannot
    /// answer a person honestly without knowing whether they got there, and a
    /// driver-side optimistic `cd` is the thing that ADR refuses.
    async fn change_directory(&self, directory: std::path::PathBuf) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitReceipt {
    Started { generation: u64, turn_id: u64 },
    Steered { generation: u64, turn_id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    Busy,
    Cancelled,
    SessionInUse { id: String },
    StaleRequest { id: RequestId },
    NoPendingPolicyIntervention,
    InvalidPolicyRecoveryAction,
    DeliveryFailed,
    Unavailable,
    ProviderUnavailable(ProviderUnavailableReason),
    SnapshotUnavailable(String),
    ReconfigureFailed(String),
    InvalidWorkingDirectory(String),
    UndoOutOfRange { requested: usize, available: usize },
    RewindPointUnavailable { turn_id: u64 },
    CodeRewindUnavailable(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("coding runtime is busy"),
            Self::Cancelled => f.write_str("coding runtime operation was cancelled"),
            Self::SessionInUse { id } => {
                write!(f, "session {id:?} is already in use by another runtime")
            }
            Self::StaleRequest { id } => write!(f, "runtime request {id} is stale"),
            Self::NoPendingPolicyIntervention => {
                f.write_str("no security policy intervention is pending")
            }
            Self::InvalidPolicyRecoveryAction => {
                f.write_str("security policy recovery action is not available")
            }
            Self::DeliveryFailed => f.write_str("kernel command delivery failed"),
            Self::Unavailable => f.write_str("coding runtime is unavailable"),
            Self::ProviderUnavailable(reason) => write!(f, "provider unavailable: {reason}"),
            Self::SnapshotUnavailable(message) => write!(f, "snapshot unavailable: {message}"),
            Self::ReconfigureFailed(message) => {
                write!(f, "runtime reconfiguration failed: {message}")
            }
            Self::InvalidWorkingDirectory(message) => f.write_str(message),
            Self::UndoOutOfRange {
                requested,
                available,
            } => write!(
                f,
                "cannot undo prompt {requested}; only {available} user prompts are available"
            ),
            Self::RewindPointUnavailable { turn_id } => {
                write!(f, "rewind point for turn {turn_id} is unavailable")
            }
            Self::CodeRewindUnavailable(reason) => {
                write!(f, "code rewind is unavailable: {reason}")
            }
        }
    }
}

impl Error for RuntimeError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderUnavailableReason {
    NotConfigured,
    AuthenticationRequired,
    UnsupportedBuild,
}

impl fmt::Display for ProviderUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("no provider configured — run /login or /provider"),
            Self::AuthenticationRequired => {
                f.write_str("provider authentication required — run /login")
            }
            Self::UnsupportedBuild => f.write_str(
                "this build cannot access the AtomGit gateway — use an official build or switch provider",
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderBootstrap {
    Required,
    RecoverAuthentication,
    Unavailable(ProviderUnavailableReason),
}

/// One totally ordered event emitted by a runtime instance.
#[derive(Clone, Debug)]
pub struct SequencedRuntimeEvent {
    pub generation: u64,
    pub sequence: u64,
    pub event: CodingRuntimeEvent,
}

pub type CodingRuntimeEvents = mpsc::UnboundedReceiver<SequencedRuntimeEvent>;

struct GenerationTaggedRuntimeEvent {
    generation: u64,
    event: CodingRuntimeEvent,
}

struct RuntimeEventEmitter {
    raw: mpsc::UnboundedSender<CodingRuntimeEvent>,
    tagged: Option<mpsc::UnboundedSender<GenerationTaggedRuntimeEvent>>,
    generation: Arc<AtomicU64>,
}

fn project_team_event(
    current_generation: u64,
    sequences: &mut BTreeMap<(u64, String), u64>,
    tagged: crate::team::GenerationTeamEvent,
) -> Option<atomcode_capabilities::team::TeamEvent> {
    if tagged.generation != current_generation {
        return None;
    }
    let key = (tagged.generation, tagged.event.run_id.to_string());
    let previous = sequences.entry(key).or_insert(0);
    if tagged.event.seq <= *previous {
        return None;
    }
    *previous = tagged.event.seq;
    Some(tagged.event)
}

impl RuntimeEventEmitter {
    fn send(&self, event: CodingRuntimeEvent) -> Result<(), ()> {
        let raw_sent = self.raw.send(event.clone()).is_ok();
        let tagged_sent = self
            .tagged
            .as_ref()
            .map(|sender| {
                sender
                    .send(GenerationTaggedRuntimeEvent {
                        generation: self.generation.load(Ordering::Acquire),
                        event,
                    })
                    .is_ok()
            })
            .unwrap_or(false);
        if raw_sent || tagged_sent {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// Inputs needed to build the first runtime generation without a bridge dependency.
/// Injected hook that rewrites a user turn's `(text, images)` before it is
/// sent to the model — the seam for vision (VL) preprocessing.
///
/// When the active model can't accept images, the implementation replaces
/// them with a VL-generated text description and returns empty images; a
/// vision-capable model passes through unchanged. Lives here (rather than the
/// runtime calling `atomcode_core::vision_preprocessor` directly) so
/// `atomcode-coding` keeps its no-`core` dependency: the driver (CLI/daemon),
/// which has `core`, injects the concrete implementation via
/// [`CodingRuntimeStart::image_preprocessor`], mirroring `provider_factory`.
///
/// Runs on the async owner task with the turn already marked in-progress, so
/// it never blocks the (fire-and-forget) submit call or the UI spinner. It
/// DOES hold the runtime's owner loop for its duration — controls (cancel,
/// compact) queue until it returns — matching the retired bridge's behavior.
#[async_trait::async_trait]
pub trait ImagePreprocessor: Send + Sync {
    /// `supports_vision` is the runtime's resolved main-turn capability (honours
    /// a `--provider` / `/model` selection and explicit profile override).
    /// `session_id` is the
    /// active conversation's id, forwarded onto any auxiliary (VL) call so a
    /// gateway pins it to the same upstream account.
    ///
    /// Returns the rewritten input plus an optional [`VisionNotice`] the
    /// runtime turns into a user-visible status line (the "✓ VL recognised
    /// image …" toast / a failure warning). `None` = nothing to surface
    /// (vision-capable model or no images).
    async fn preprocess(
        &self,
        text: String,
        images: Vec<ImageContent>,
        supports_vision: bool,
        session_id: Option<String>,
    ) -> (UserInput, Option<VisionNotice>);
}

/// Result of a vision preprocessing pass, surfaced to the user as a status
/// line by the runtime (the driver deliberately can't render directly — it
/// runs on the owner task, not the UI loop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisionNotice {
    /// VL converted the image(s) to text — show the "recognised" toast.
    Recognised { vl_model: String, char_count: usize },
    /// VL failed (images were cleared from the model request + a failure
    /// marker folded into the text) — the driver surfaces a warning and
    /// re-attaches the images it remembers from submit (the runtime doesn't
    /// carry them, so image↔marker pairing stays authoritative on the driver).
    Failed { reason: String },
}

pub struct CodingRuntimeStart {
    pub agent: CodingAgentConfig,
    pub prepare: PrepareOptions,
    pub provider_factory: Arc<dyn CodingProviderFactory>,
    pub plugin_hooks: Arc<dyn PluginHookSource>,
    /// Optional VL preprocessing hook (see [`ImagePreprocessor`]). `None` on
    /// paths that either can't send images to a non-vision model or already
    /// preprocess upstream (the daemon today).
    pub image_preprocessor: Option<Arc<dyn ImagePreprocessor>>,
}

struct RuntimeResources {
    config: CodingAgentConfig,
    prepare: PrepareOptions,
    provider_factory: Arc<dyn CodingProviderFactory>,
    plugin_hooks: Arc<dyn PluginHookSource>,
    parts: crate::CodingParts,
    /// The mounted tree, on the harness engine. `None` on the chain.
    ///
    /// Held, never read: unloading it would tear down every row under a live
    /// handle. When the harness carries the reassembly paths too, this is what
    /// `ControlSvc::patch` will be reached through.
    ///
    /// Never read ON PURPOSE — it is held for its `Drop`, not its value.
    harness_app: Option<atomcode_plexus::App>,
    /// The provider table the `llm` row reads by id. Holding it is what makes a
    /// `/model` switch a patch rather than a rebuild — see
    /// `on_harness::ProviderSlots`.
    harness_providers: Option<Arc<crate::on_harness::ProviderSlots>>,
    wakeup_tx: mpsc::UnboundedSender<WakeupRequest>,
    loop_active: Arc<std::sync::atomic::AtomicBool>,
    image_preprocessor: Option<Arc<dyn ImagePreprocessor>>,
}

/// The `/mcp` panel's rows for the tree mounted right now: every configured server
/// (disabled ones included) joined with the live statuses and this session's tool
/// counts.
///
/// One helper for the list and the detail arm, so the two cannot disagree about
/// the same server. `None` registry — no tree, MCP off — is no rows, not an error;
/// a config file that does not parse is one, carried verbatim.
async fn mcp_rows_of(runtime: &RuntimeResources) -> Result<Vec<crate::parts::McpRowFacts>, String> {
    let counts: Vec<(String, usize)> = runtime
        .parts
        .mcp_statuses()
        .await
        .into_iter()
        .map(|(name, _)| {
            let n = runtime.parts.mcp_tools_for_server(&name).len();
            (name, n)
        })
        .collect();
    match &runtime.parts.mcp_registry {
        Some(registry) => {
            crate::parts::mcp_row_facts(&runtime.config.working_dir, registry, &counts).await
        }
        None => Ok(Vec::new()),
    }
}

/// Run one panel action against the live tree: the six things a person can do to
/// a configured MCP server (`docs/mcp-panel-design.md` §5.2), bar signing in.
///
/// Signing in is not here: it opens a browser and writes a token, and touches
/// nothing the runtime owns, so it is the front end's to run beside the screen
/// (`cli/tui_mcp.rs`) — the way `/openrouter` authorises — followed by the same
/// `/mcp reload` any config change takes.
///
/// The failure is a `String` because it is carried to the front end verbatim: a
/// refused config write must arrive with the guard's own words rather than a
/// generic failure (§6). The caller wraps it in `RuntimeError::ReconfigureFailed`.
///
/// This writes the state; it does not reconnect. Every action but `Disable` is
/// followed by a capability reload from [`CodingRuntimeHandle::mcp_act`], which is
/// what makes a trust or an enabled entry reach the session.
///
/// Order is the point where trust or auth change: the tools come off the session
/// BEFORE the state they were authorised under is changed — the fail-closed order
/// `CodingParts::withdraw_mcp_tools` documents for `/mcp reload`, `/mcp untrust`
/// and `/mcp logout`.
///
/// `holds` is what each `Disable` in this session held back, by server, so the
/// matching `Enable` gives back exactly that (`parts::hold_for_disable`).
async fn apply_mcp_action(
    runtime: &mut RuntimeResources,
    server: String,
    action: crate::parts::McpAction,
    holds: &mut BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    use crate::parts::McpAction;

    match action {
        McpAction::Trust => {
            atomcode_capabilities::mcp::trust::trust_project(&runtime.config.working_dir)
                .map_err(|e| format!("{e:#}"))
        }
        McpAction::Untrust => {
            // Fail-closed, in this order: the tools come off
            // BEFORE the trust that lets them connect is
            // withdrawn (`parts.rs:1312-1321`).
            runtime.parts.withdraw_mcp_tools().await;
            atomcode_capabilities::mcp::trust::untrust_project(&runtime.config.working_dir)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        }
        McpAction::Logout => {
            runtime.parts.withdraw_mcp_tools().await;
            atomcode_capabilities::mcp::McpTokenStore::default()
                .delete_token(&server)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        }
        McpAction::Disable => {
            crate::parts::mcp_set_enabled(&runtime.config.working_dir, &server, false).await?;
            // Take THIS server's tools off the session — by their published
            // names, not by a glob (sanitised names can carry a hash suffix) —
            // and remember which ones, so enabling it again can give them back.
            if let Some(catalog) = runtime.parts.tool_catalog() {
                let names = runtime.parts.mcp_tools_for_server(&server);
                let held = crate::parts::hold_for_disable(&catalog, &names);
                holds.entry(server).or_default().extend(held);
            }
            Ok(())
        }
        McpAction::Enable => {
            crate::parts::mcp_set_enabled(&runtime.config.working_dir, &server, true).await?;
            if let Some(held) = holds.remove(&server) {
                match runtime.parts.tool_catalog() {
                    Some(catalog) => crate::parts::release_after_enable(&catalog, &held),
                    // No mounted catalog to go through: drop the switches
                    // directly, so the tools are not born hidden when the
                    // rebuild brings them back.
                    None => {
                        let switches = runtime.parts.tool_switches();
                        for name in &held {
                            switches.turn_on(name, &[]);
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

struct NextPromptSuggestionOutcome {
    generation: u64,
    revision: u64,
    session_id: Option<String>,
    turn_id: u64,
    text: String,
}

/// A native coding runtime. Dropping `events` causes a fail-closed shutdown.
pub struct CodingRuntime {
    pub handle: CodingRuntimeHandle,
    pub events: CodingRuntimeEvents,
    pub task: tokio::task::JoinHandle<RuntimeExit>,
    pub session: Option<RuntimeSessionInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSessionInfo {
    pub id: String,
    pub resumed: bool,
}

#[derive(Debug)]
pub enum RuntimeStartError {
    Prepare(std::io::Error),
    SessionInUse { id: String },
    Provider(crate::ProviderBuildError),
    Assemble(std::io::Error),
}

impl fmt::Display for RuntimeStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(error) => write!(f, "coding runtime prepare failed: {error}"),
            Self::SessionInUse { id } => {
                write!(f, "session {id:?} is already in use by another runtime")
            }
            Self::Provider(error) => write!(f, "coding runtime provider failed: {error}"),
            Self::Assemble(error) => write!(f, "coding runtime assemble failed: {error}"),
        }
    }
}

impl Error for RuntimeStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Prepare(error) | Self::Assemble(error) => Some(error),
            Self::Provider(error) => Some(error),
            Self::SessionInUse { .. } => None,
        }
    }
}

/// Terminal state of a compaction accepted by the coding runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactionCompletion {
    /// The kernel returned a normal compaction result.
    Completed(CompactionOutcome),
    /// The prepared result could not be durably checkpointed, so it was not committed.
    Failed {
        trigger: CompactTrigger,
        error: CompactionCheckpointError,
    },
    /// The owning runtime was replaced or stopped before the kernel returned a result.
    Interrupted {
        trigger: CompactTrigger,
        reason: CompactionInterruption,
    },
}

impl CompactionCompletion {
    /// Trigger that initiated this compaction attempt.
    pub fn trigger(&self) -> &CompactTrigger {
        match self {
            Self::Completed(outcome) => &outcome.trigger,
            Self::Failed { trigger, .. } => trigger,
            Self::Interrupted { trigger, .. } => trigger,
        }
    }

    /// Whether this terminal belongs to a user-requested `/compact`.
    pub fn is_manual(&self) -> bool {
        matches!(self.trigger(), CompactTrigger::Manual { .. })
    }
}

/// Why a compaction could not reach a kernel result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionInterruption {
    /// The agent/session/provider owning the request was replaced.
    RuntimeReconfigured,
    /// The coding runtime was shut down.
    RuntimeShutdown,
    /// The current agent was already unavailable when delivery was attempted.
    RuntimeUnavailable,
}

/// The driver-neutral result of a compaction attempt.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionOutcome {
    pub trigger: CompactTrigger,
    pub epoch: u64,
    pub removed_messages: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub committed: bool,
    pub estimated_tokens_before: usize,
    pub estimated_tokens_after: usize,
    /// Exact candidate used for a committed manual compaction. For a session-bound
    /// runtime its durable checkpoint has already succeeded; ephemeral runtimes may
    /// also carry it so driver projections can converge on the live state.
    pub committed_snapshot: Option<Arc<SessionSnapshot>>,
}

/// Result of compacting an already-persisted conversation without starting an
/// agent loop. Used by stateless daemon slash-command execution.
pub struct SnapshotCompaction {
    pub messages: Vec<Message>,
    pub outcome: CompactionOutcome,
    pub mutation: SnapshotCompactionMutation,
}

/// Shape of the committed snapshot mutation. Stateless drivers use this to
/// preserve legacy-only fields on messages that survived compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotCompactionMutation {
    /// The kernel refused the plan or the policy proposed no changes.
    Noop,
    /// Existing message positions were retained; only message bodies changed
    /// in place (and a synthetic note may have been appended).
    RewriteOnly,
    /// One contiguous old span was replaced by a new span.
    Replace {
        old_start: usize,
        old_end: usize,
        new_end: usize,
    },
}

/// Apply the same v2 manual-compaction policy and kernel invariants to a
/// persisted message list.
pub async fn compact_snapshot(
    messages: Vec<Message>,
    provider: Arc<dyn LlmProvider>,
    focus: Option<String>,
) -> SnapshotCompaction {
    let mut conversation = Conversation {
        messages,
        cache_epoch: 0,
    };
    let floor = conversation.sacred_floor();
    let (recorded_window, used_tokens, _) = conversation.last_pressure();
    let live_window = provider.context_window();
    let ctx_window = if live_window > 0 {
        live_window
    } else {
        recorded_window
    };
    let utilization = if ctx_window > 0 {
        used_tokens as f32 / ctx_window as f32
    } else {
        0.0
    };
    let trigger = CompactTrigger::Manual { focus };
    let strategy = atomcode_capabilities::compaction::OverflowCompaction::new(
        atomcode_capabilities::compaction::StubCompaction::default(),
        Some(provider),
    );
    let plan = strategy
        .plan(&CompactionView {
            messages: &conversation.messages,
            trigger: trigger.clone(),
            ctx_window,
            used_tokens,
            utilization,
            sacred_floor: floor,
        })
        .await;
    let mutation = snapshot_mutation(&conversation, &plan);
    let report = conversation.apply_plan(plan, floor);
    let outcome = CompactionOutcome::from_kernel(
        trigger,
        report.epoch_after,
        report.removed,
        report.bytes_before,
        report.bytes_after,
        report.committed,
        Some(used_tokens as usize).filter(|tokens| *tokens > 0),
    );
    let mutation = if report.committed {
        mutation
    } else {
        SnapshotCompactionMutation::Noop
    };
    SnapshotCompaction {
        messages: conversation.messages,
        outcome,
        mutation,
    }
}

fn snapshot_mutation(
    conversation: &Conversation,
    plan: &atomcode_kernel::message::CompactionPlan,
) -> SnapshotCompactionMutation {
    let len = conversation.messages.len();
    let floor = conversation.sacred_floor().min(len);
    let old_start = plan.drain_from.max(floor).min(len);
    let old_end = plan.drain_to.min(len);
    if old_start < old_end || plan.summary.is_some() {
        let new_end = old_start + usize::from(plan.summary.is_some());
        SnapshotCompactionMutation::Replace {
            old_start,
            old_end,
            new_end,
        }
    } else {
        SnapshotCompactionMutation::RewriteOnly
    }
}

impl CompactionOutcome {
    /// Build an outcome from the kernel audit fields and the most recent real
    /// provider usage observed by the runtime owner.
    #[doc(hidden)]
    pub fn from_kernel(
        trigger: CompactTrigger,
        epoch: u64,
        removed_messages: usize,
        bytes_before: usize,
        bytes_after: usize,
        committed: bool,
        observed_tokens_before: Option<usize>,
    ) -> Self {
        let estimated_tokens_before = observed_tokens_before
            .filter(|tokens| *tokens > 0)
            .unwrap_or(bytes_before / 4);
        let estimated_tokens_after =
            estimate_after_tokens(estimated_tokens_before, bytes_before, bytes_after);

        Self {
            trigger,
            epoch,
            removed_messages,
            bytes_before,
            bytes_after,
            committed,
            estimated_tokens_before,
            estimated_tokens_after,
            committed_snapshot: None,
        }
    }

    /// Whether the attempt was explicitly requested by a user.
    pub fn is_manual(&self) -> bool {
        matches!(self.trigger, CompactTrigger::Manual { .. })
    }

    /// Whether this is a silent, cache-friendly in-place tool-output fold: an
    /// AUTOMATIC compaction that only stubbed stale tool results with no turns drained
    /// (`removed_messages == 0`). This is invisible transcript maintenance — the
    /// "Tool output folded · saved ~N tok" mark is noise that also misreads as "wasting
    /// tokens" when it is in fact SAVING context. The daemon/webui projectors call this to
    /// suppress the mark, keeping it only for real drains (`removed_messages > 0`) and
    /// manual/overflow compactions (other triggers). An auto drain/summarize normally
    /// removes ≥1 turn, so `Auto && removed_messages == 0` is the pure-stub case.
    ///
    /// CAVEAT: an auto re-summarize can net zero removals yet is NOT a pure stub fold; the
    /// TUI distinguishes it via the announce (`CompactionStarted`) signal, which this outcome
    /// does not carry. Fully unifying both drivers onto one discriminator would need a kernel
    /// `stub_only` flag threaded onto the compaction event.
    pub fn is_silent_auto_tool_fold(&self) -> bool {
        matches!(self.trigger, CompactTrigger::Auto { .. }) && self.removed_messages == 0
    }

    /// Whether the proposed compacted conversation was larger than the input.
    pub fn summary_would_grow(&self) -> bool {
        self.bytes_after > self.bytes_before
    }
}

fn estimate_after_tokens(tokens_before: usize, bytes_before: usize, bytes_after: usize) -> usize {
    if bytes_before == 0 {
        return tokens_before;
    }

    ((tokens_before as u128 * bytes_after as u128) / bytes_before as u128) as usize
}

/// Cloneable, stable control handle held by a driver.
#[derive(Clone, Debug)]
pub struct CodingRuntimeHandle {
    tx: mpsc::UnboundedSender<CodingRuntimeControl>,
    state: Arc<AtomicU64>,
    provider_unavailable_reason: Arc<AtomicU8>,
    terminal: watch::Receiver<Option<RuntimeExit>>,
    /// Fired by [`cancel`](Self::cancel) *before* the command goes on the
    /// channel, so work the owner is awaiting inside its own loop can see the
    /// stop it cannot yet read.
    ///
    /// The owner reads commands one at a time; anything it awaits in a command's
    /// arm blocks every command behind it, this one included. Image recognition
    /// is such an await — a model call with no overall cap — and `esc` under it
    /// used to sit in the channel for the whole call. The owner puts a fresh
    /// token here when it handles the cancel, so a stop is only ever held
    /// against the work it was meant for.
    stop: Arc<Mutex<CancellationToken>>,
}

#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct RuntimeSnapshotReceipt {
    snapshot: Arc<SessionSnapshot>,
    undo_snapshot: Arc<SessionSnapshot>,
    revision: u64,
}

type RuntimeSnapshotWaiter = oneshot::Sender<Result<RuntimeSnapshotReceipt, RuntimeError>>;

/// Readiness of a runtime whose asynchronous preparation is owned by a driver adapter.
/// Once ready, consumers read the authoritative phase from the stable runtime handle
/// instead of maintaining a second lifecycle state mirror.
#[derive(Clone, Debug)]
pub enum DeferredRuntimeState {
    Starting,
    Ready(CodingRuntimeHandle),
    Failed(String),
}

impl CodingRuntimeHandle {
    pub fn is_stopped(&self) -> bool {
        self.terminal.borrow().is_some() || self.tx.is_closed()
    }

    /// Current actor-owned lifecycle state projected for fast driver checks.
    pub fn status(&self) -> RuntimeStatus {
        runtime_status(self.state.load(Ordering::Acquire))
    }

    /// Current reason an `AwaitingProvider` runtime cannot accept turns.
    /// The runtime owner is the sole writer; drivers use this projection to
    /// distinguish recoverable authentication from configuration/build gaps.
    pub fn provider_unavailable_reason(&self) -> Option<ProviderUnavailableReason> {
        decode_provider_unavailable_reason(self.provider_unavailable_reason.load(Ordering::Acquire))
    }

    /// Whether a fire-and-forget driver command can be accepted in the current
    /// authoritative runtime phase. Drivers use this before entering UI states
    /// that assume the command reached the runtime owner.
    pub fn accepts(&self, command: &DriverCommand) -> bool {
        !self.is_stopped() && runtime_phase_accepts_command(self.status().phase, command)
    }

    /// Request manual conversation compaction from the current kernel agent.
    pub fn compact(&self, focus: Option<String>) -> Result<(), RuntimeUnavailable> {
        let state = self.state.load(Ordering::Acquire);
        if !runtime_state_available(state) {
            return Err(RuntimeUnavailable);
        }
        self.tx
            .send(CodingRuntimeControl::Compact {
                generation: runtime_state_generation(state),
                focus,
            })
            .map_err(|_| RuntimeUnavailable)
    }

    pub fn dispatch(&self, command: DriverCommand) -> Result<(), RuntimeUnavailable> {
        let state = self.state.load(Ordering::Acquire);
        if self.is_stopped()
            || !runtime_phase_accepts_command(runtime_status(state).phase, &command)
        {
            return Err(RuntimeUnavailable);
        }
        let generation = runtime_state_generation(state);
        let command = match command {
            DriverCommand::UndoToPrompt(nth) => {
                let handle = self.clone();
                tokio::spawn(async move {
                    let _ = handle.undo_to_prompt(nth).await;
                });
                return Ok(());
            }
            DriverCommand::Rewind { turn_id, scope } => {
                let handle = self.clone();
                tokio::spawn(async move {
                    let _ = handle.rewind(turn_id, scope).await;
                });
                return Ok(());
            }
            DriverCommand::RefreshContextStats => {
                let handle = self.clone();
                tokio::spawn(async move {
                    let _ = handle.context_stats().await;
                });
                return Ok(());
            }
            command => command,
        };
        let control = match command {
            DriverCommand::Submit(input) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Submit {
                    generation,
                    input,
                    done,
                }
            }
            DriverCommand::Respond { id, value } => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Respond {
                    generation,
                    id,
                    value,
                    done,
                }
            }
            DriverCommand::ResolvePolicyIntervention {
                intervention_id,
                action,
            } => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::ResolvePolicyIntervention {
                    generation,
                    intervention_id,
                    action,
                    done,
                }
            }
            DriverCommand::Cancel => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Cancel { generation, done }
            }
            DriverCommand::PauseGoal => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::PauseGoal { generation, done }
            }
            DriverCommand::Compact(focus) => CodingRuntimeControl::Compact { generation, focus },
            DriverCommand::SetMode(mode) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::SetMode {
                    generation,
                    mode,
                    done,
                }
            }
            DriverCommand::QueueLocalContext(input) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::QueueLocalContext {
                    generation,
                    input,
                    done,
                }
            }
            DriverCommand::ReloadProvider(next) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::ReassembleProvider {
                    generation,
                    next,
                    done,
                }
            }
            DriverCommand::ReprepareConfig(next) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Reprepare {
                    generation,
                    target: ReprepareTarget::ReloadConfig(next),
                    done,
                }
            }
            DriverCommand::DeactivateProvider(reason) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::DeactivateProvider {
                    generation,
                    reason,
                    done,
                }
            }
            DriverCommand::UndoToPrompt(_)
            | DriverCommand::Rewind { .. }
            | DriverCommand::RefreshContextStats => {
                unreachable!("handled before control conversion")
            }
            DriverCommand::RestoreSnapshot(snapshot) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::RestoreSnapshot {
                    generation,
                    snapshot,
                    done,
                }
            }
            DriverCommand::RestoreSnapshotCorrelated { snapshot, .. } => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::RestoreSnapshot {
                    generation,
                    snapshot,
                    done,
                }
            }
            DriverCommand::StartGoal(input) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::StartGoal {
                    generation,
                    condition: input.text,
                    images: input.images,
                    done,
                    recovery_tx: self.tx.clone(),
                }
            }
            DriverCommand::StopGoal => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::StopGoal { generation, done }
            }
            DriverCommand::StartLoop(prompt) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::StartLoop {
                    generation,
                    prompt,
                    // The driver protocol carries no cadence, so this is the
                    // model-paced form. A driver that wants the other one asks
                    // through `RuntimeCommands`, where the cadence lives.
                    every: None,
                    done,
                    recovery_tx: self.tx.clone(),
                }
            }
            DriverCommand::StopLoop => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::StopLoop { generation, done }
            }
            DriverCommand::Shutdown => CodingRuntimeControl::Shutdown { generation },
        };
        self.tx.send(control).map_err(|_| RuntimeUnavailable)
    }

    pub async fn submit(&self, input: UserInput) -> Result<SubmitReceipt, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Submit {
                generation: runtime_state_generation(state),
                input,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn respond(
        &self,
        id: RequestId,
        value: serde_json::Value,
    ) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Respond {
                generation: runtime_state_generation(state),
                id,
                value,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn resolve_policy_intervention(
        &self,
        intervention_id: u64,
        action: PolicyRecoveryAction,
    ) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ResolvePolicyIntervention {
                generation: runtime_state_generation(state),
                intervention_id,
                action,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// The policy intervention waiting for a person, if one is.
    pub async fn pending_policy_intervention(&self) -> Option<PolicyIntervention> {
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::PendingPolicyIntervention { done })
            .ok()?;
        result.await.ok().flatten()
    }

    pub async fn snapshot(&self) -> Result<Arc<SessionSnapshot>, RuntimeError> {
        Ok(self.snapshot_with_revision().await?.snapshot)
    }

    async fn snapshot_with_revision(&self) -> Result<RuntimeSnapshotReceipt, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Snapshot {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn cancel(&self) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        // Before the command, not after: what this stops may be something the
        // owner is awaiting in its own loop, which is exactly the case where the
        // command itself cannot be read yet.
        self.stop.lock().expect("stop poisoned").cancel();
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Cancel {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn pause_goal(&self) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::PauseGoal {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn set_mode(&self, mode: RuntimeMode) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::SetMode {
                generation: runtime_state_generation(state),
                mode,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn context_stats(&self) -> Result<RuntimeContextStats, RuntimeError> {
        self.context_stats_with(false).await
    }

    /// The same, and with `prompt` the system prompt this session runs on.
    pub async fn context_stats_with(
        &self,
        prompt: bool,
    ) -> Result<RuntimeContextStats, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ContextStats {
                generation: runtime_state_generation(state),
                prompt,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// The execution mode in force, as the flags that govern a tool call have it.
    ///
    /// `Err` is "this runtime cannot answer" — a stopped or unavailable one. It
    /// is not "no mode": a coding runtime always has one, and the caller's
    /// `None` for "a host that does not govern modes at all" is a different
    /// fact, kept by the host that knows it (`atomcode_host_api::HostReply`).
    pub async fn mode(&self) -> Result<RuntimeMode, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Mode {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// Explicit headless readiness policy. Interactive callers should let MCP
    /// connect in the background and observe new tools from the next turn.
    pub async fn wait_mcp_ready(&self, timeout: std::time::Duration) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::WaitMcpReady {
                generation: runtime_state_generation(state),
                timeout,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn mcp_status(&self) -> Result<McpStatusSnapshot, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::McpStatus {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn mcp_tools(&self, server: String) -> Result<McpToolsSnapshot, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::McpTools {
                generation: runtime_state_generation(state),
                server,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// "Always allow" the MCP tool the model calls `alias` (`mcp__<server>__<tool>`):
    /// auto-approve it for the rest of this session and add it to the project's
    /// `autoApprove`, both through THIS runtime's registry — the one whose tools the
    /// model calls, and the only one that can map a sanitised alias back to the
    /// server's own tool name. `None` when no connected server offers `alias`.
    pub async fn approve_mcp_tool(
        &self,
        alias: String,
    ) -> Result<Option<McpToolApproval>, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ApproveMcpTool {
                generation: runtime_state_generation(state),
                alias,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// The management list: every configured server — disabled ones included —
    /// with the live status, source, config path, transport, auth and tool count.
    pub async fn mcp_rows(&self) -> Result<McpRowsSnapshot, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::McpRows {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// One configured server in full. An unknown key answers `detail == None`,
    /// not an error.
    pub async fn mcp_detail(&self, server: String) -> Result<McpDetailSnapshot, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::McpDetail {
                generation: runtime_state_generation(state),
                server,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// Do one thing to one configured MCP server, and make it reach the session.
    /// Answers when it is done — call [`mcp_rows`](Self::mcp_rows) for the state it
    /// left behind.
    ///
    /// Trust and an enabled entry are read when the graph is prepared, and
    /// untrust and sign-out take every MCP tool off; so every action but `Disable`
    /// is followed by the same capability reload `/mcp reload` runs. Without it the
    /// panel would show the server exactly as it was — untrusted after 信任,
    /// disconnected after 启用 — and the person would take the action for a no-op.
    ///
    /// Those four answer [`RuntimeError::Busy`] while a turn is running (both the
    /// withdrawal and the rebuild wait for an idle session); `Disable` runs mid-turn.
    pub async fn mcp_act(
        &self,
        server: String,
        action: crate::parts::McpAction,
    ) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::McpAct {
                generation: runtime_state_generation(state),
                server: server.clone(),
                action,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)??;
        if !action.rebuilds() {
            return Ok(());
        }
        match self.reload_capabilities().await {
            Ok(_) => Ok(()),
            Err(RuntimeError::Busy) => Err(RuntimeError::ReconfigureFailed(format!(
                "the change to MCP server '{server}' is saved, but a turn started before it \
                 could be applied; run /mcp reload when the turn ends"
            ))),
            Err(error) => Err(error),
        }
    }

    /// What the model can call right now, what the person turned off, and what
    /// the tree was configured without (`docs/tool-catalog-policy.md`).
    pub async fn tool_catalog(
        &self,
    ) -> Result<Vec<atomcode_harness::seams::ToolListing>, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ToolCatalog {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// Turn `pattern` off or back on for this session, and answer with the
    /// catalog as it now is — one round trip, so a screen never renders a
    /// switch it only assumes was thrown.
    pub async fn switch_tool(
        &self,
        pattern: String,
        on: bool,
    ) -> Result<Vec<atomcode_harness::seams::ToolListing>, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::SwitchTool {
                generation: runtime_state_generation(state),
                pattern,
                on,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// Remove every MCP tool from the model-facing catalog without reading
    /// mutable config, trust, or auth state. Security-reducing mutations must
    /// await this terminal before changing those inputs.
    pub async fn withdraw_mcp_tools(&self) -> Result<(), RuntimeError> {
        self.send_withdraw_mcp_tools(true).await
    }

    /// `tell`: whether the model is told the person withdrew them. A reload
    /// withdraws on its way to putting them back, and says so itself.
    async fn send_withdraw_mcp_tools(&self, tell: bool) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::WithdrawMcpTools {
                generation: runtime_state_generation(state),
                tell,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn queue_local_context(&self, input: LocalContextInput) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::QueueLocalContext {
                generation: runtime_state_generation(state),
                input,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn reassemble_provider(
        &self,
        next: CodingAgentConfig,
    ) -> Result<RuntimeGeneration, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ReassembleProvider {
                generation: runtime_state_generation(state),
                next,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn deactivate_provider(
        &self,
        reason: ProviderUnavailableReason,
    ) -> Result<RuntimeGeneration, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::DeactivateProvider {
                generation: runtime_state_generation(state),
                reason,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn fresh_session(&self) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Fresh).await
    }

    pub async fn reload_capabilities(&self) -> Result<SessionChanged, RuntimeError> {
        self.reload_capabilities_with_plugin_skills(None).await
    }

    /// Rebuild every prepared capability with `next`, preserving the current
    /// session and runtime continuity stores.
    pub async fn reprepare_config(
        &self,
        next: CodingAgentConfig,
    ) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::ReloadConfig(next))
            .await
    }

    /// Reload the capability graph, optionally replacing the plugin skill
    /// directories captured at runtime startup. Drivers call this after plugin
    /// install/update/uninstall so the replacement generation sees current
    /// disk state rather than the stale startup snapshot.
    pub async fn reload_capabilities_with_plugin_skills(
        &self,
        plugin_skill_dirs: Option<Vec<(std::path::PathBuf, String)>>,
    ) -> Result<SessionChanged, RuntimeError> {
        self.send_withdraw_mcp_tools(false).await?;
        self.reprepare_target(ReprepareTarget::Reload { plugin_skill_dirs })
            .await
    }

    pub async fn resume_session(
        &self,
        id: impl Into<String>,
    ) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Resume(id.into()))
            .await
    }

    /// Resume a session whose catalog cutover lease is already held by the driver.
    /// The same guard is validated and transferred into the replacement
    /// [`SessionBinding`], so legacy import and runtime ownership have no unlocked
    /// window between them.
    pub async fn resume_session_with_lease(
        &self,
        id: impl Into<String>,
        working_dir: std::path::PathBuf,
        lease: SessionLease,
    ) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::ResumeWithLease {
            id: id.into(),
            working_dir,
            lease,
            cancel: None,
        })
        .await
    }

    /// Resume with a driver-owned cancellation signal. Cancellation is honored
    /// only while the replacement is still in preflight; after the persistence
    /// commit point the runtime transition remains authoritative.
    pub async fn resume_session_with_lease_cancelable(
        &self,
        id: impl Into<String>,
        working_dir: std::path::PathBuf,
        lease: SessionLease,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::ResumeWithLease {
            id: id.into(),
            working_dir,
            lease,
            cancel: Some(cancel),
        })
        .await
    }

    pub async fn change_directory(
        &self,
        directory: std::path::PathBuf,
    ) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::ChangeDirectory(directory))
            .await
    }

    pub async fn undo_to_prompt(&self, nth: Option<usize>) -> Result<UndoResult, RuntimeError> {
        let generation = self.status().generation;
        let original = self.snapshot_with_revision().await?;
        let undo = undo_snapshot_to_prompt(&original.undo_snapshot, nth)?;
        self.apply_undo(
            generation,
            original.revision,
            original.undo_snapshot,
            undo,
            None,
        )
        .await
    }

    /// Undo back to before the log's `turn` — a rewind point's turn — rather
    /// than to a prompt ordinal in the conversation as it now stands.
    ///
    /// What a point names is a turn, and the turn is still in the log however
    /// much of the conversation a compaction has folded since; an ordinal is
    /// not (see [`undo_to_turn_in_log`]).
    pub async fn undo_to_turn(&self, turn: u64) -> Result<UndoResult, RuntimeError> {
        let generation = self.status().generation;
        let original = self.snapshot_with_revision().await?;
        let undo = self
            .undo_target(generation, original.revision, turn)
            .await?;
        self.apply_undo(
            generation,
            original.revision,
            original.undo_snapshot,
            undo,
            None,
        )
        .await
    }

    /// The conversation before `turn`, as the control loop reads it off the log.
    async fn undo_target(
        &self,
        generation: u64,
        expected_revision: u64,
        turn: u64,
    ) -> Result<SnapshotUndoResult, RuntimeError> {
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::UndoTarget {
                generation,
                expected_revision,
                turn,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    async fn apply_undo(
        &self,
        generation: u64,
        expected_revision: u64,
        original: Arc<SessionSnapshot>,
        undo: SnapshotUndoResult,
        code_rewound_to: Option<u64>,
    ) -> Result<UndoResult, RuntimeError> {
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ApplyUndo {
                generation,
                expected_revision,
                code_rewound_to,
                original,
                truncated: undo.snapshot,
                restored_prompt: undo.restored_prompt,
                target_n: undo.target_n,
                prompts_before: undo.prompts_before,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn rewind_points(&self) -> Result<RewindCatalog, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::RewindCatalog {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// Whether this session is driving itself, and how far it has got.
    pub async fn autonomy(&self) -> Result<Autonomy, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Autonomy {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// What the account has left to spend, window by window.
    ///
    /// Best-effort and bounded: the source is a network call, and a person
    /// asking "how much have I got left" must not be made to wait on it. No
    /// source, a slow one or a failing one all answer with an empty list, which
    /// says "this host does not meter" in the only way a front end can act on.
    /// The account's windows **and** what it has spent, in one round trip.
    ///
    /// Together because a screen that shows them shows them on one page, and
    /// two trips would be two chances for one of them to be a moment stale
    /// against the other.
    #[allow(clippy::type_complexity)]
    pub async fn usage(&self, windows_only: bool) -> Result<Allowance, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Usage {
                generation: runtime_state_generation(state),
                windows_only,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// What this session has changed in the workspace — every file, or one
    /// file's diff.
    pub async fn workspace_changes(
        &self,
        file: Option<String>,
        scope: WorkspaceScope,
    ) -> Result<WorkspaceChanges, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::WorkspaceChanges {
                generation: runtime_state_generation(state),
                file,
                scope,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn rewind(
        &self,
        turn_id: u64,
        scope: RewindScope,
    ) -> Result<RewindResult, RuntimeError> {
        let catalog = self.rewind_points().await?;
        self.rewind_from_catalog(catalog, turn_id, scope).await
    }

    /// Execute a choice made from a previously rendered catalog.
    ///
    /// Keeping the catalog's generation and conversation revision is
    /// intentional: a modal must not silently reinterpret an old turn id
    /// against a session selected while that modal was open.
    pub async fn rewind_from_catalog(
        &self,
        catalog: RewindCatalog,
        turn_id: u64,
        scope: RewindScope,
    ) -> Result<RewindResult, RuntimeError> {
        let point = catalog
            .points
            .iter()
            .find(|point| point.turn_id == turn_id)
            .cloned()
            .ok_or(RuntimeError::RewindPointUnavailable { turn_id })?;
        if scope.restores_code() {
            if let Some(reason) = catalog.code_unavailable {
                // The error carries a sentence because that is what a
                // `RuntimeError` is; the *kind* reached the front end on the
                // catalog, which is where a screen looks.
                return Err(RuntimeError::CodeRewindUnavailable(reason.say()));
            }
        }
        let original = self.snapshot_with_revision().await?;
        if original.revision != catalog.revision {
            return Err(RuntimeError::Busy);
        }
        let undo = if scope.restores_conversation() {
            Some(
                self.undo_target(catalog.generation.0, catalog.revision, point.turn_id)
                    .await?,
            )
        } else {
            None
        };
        let (begin_done, begin_result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::BeginRewind {
                generation: catalog.generation.0,
                expected_revision: catalog.revision,
                point: point.clone(),
                restore_code: scope.restores_code(),
                target_snapshot: undo.as_ref().map(|undo| undo.snapshot.clone()),
                recovery_tx: self.tx.clone(),
                done: begin_done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        let mut transaction = begin_result
            .await
            .map_err(|_| RuntimeError::Unavailable)??;
        if scope == RewindScope::Code {
            let restored_files = transaction.receipt().restored_files().to_vec();
            let receipt = transaction.commit();
            self.finish_rewind(catalog.generation.0, receipt, RewindFinalization::Commit)
                .await?;
            return Ok(RewindResult {
                generation: catalog.generation,
                scope,
                point,
                snapshot: original.undo_snapshot,
                restored_prompt: None,
                restored_files,
            });
        }
        let undo = undo.expect("conversation rewind must have an undo plan");
        match self
            .apply_undo(
                catalog.generation.0,
                catalog.revision,
                original.undo_snapshot,
                undo,
                scope.restores_code().then_some(point.turn_id),
            )
            .await
        {
            Ok(result) => {
                let receipt = transaction.commit();
                let restored_files = receipt.restored_files().to_vec();
                self.finish_rewind(result.generation.0, receipt, RewindFinalization::Commit)
                    .await?;
                Ok(RewindResult {
                    generation: result.generation,
                    scope,
                    point,
                    snapshot: result.snapshot,
                    restored_prompt: Some(result.restored_prompt),
                    restored_files,
                })
            }
            Err(error) => {
                let receipt = transaction.take_for_compensation();
                match self
                    .finish_rewind(
                        catalog.generation.0,
                        receipt,
                        RewindFinalization::Compensate,
                    )
                    .await
                {
                    Ok(()) => Err(error),
                    Err(compensation) => Err(RuntimeError::ReconfigureFailed(format!(
                        "{error}; rewind compensation failed: {compensation}"
                    ))),
                }
            }
        }
    }

    async fn finish_rewind(
        &self,
        generation: u64,
        receipt: RewindTransactionReceipt,
        outcome: RewindFinalization,
    ) -> Result<(), RuntimeError> {
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::FinishRewind {
                generation,
                receipt,
                outcome,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn restore_snapshot(
        &self,
        snapshot: SessionSnapshot,
    ) -> Result<SessionChanged, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::RestoreSnapshot {
                generation: runtime_state_generation(state),
                snapshot,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn start_goal(&self, condition: impl Into<UserInput>) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        let condition = condition.into();
        self.tx
            .send(CodingRuntimeControl::StartGoal {
                generation: runtime_state_generation(state),
                condition: condition.text,
                images: condition.images,
                done,
                recovery_tx: self.tx.clone(),
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn stop_goal(&self) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::StopGoal {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn start_loop(
        &self,
        prompt: impl Into<String>,
        every: Option<u32>,
    ) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::StartLoop {
                generation: runtime_state_generation(state),
                prompt: prompt.into(),
                every,
                done,
                recovery_tx: self.tx.clone(),
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    pub async fn stop_loop(&self) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::StopLoop {
                generation: runtime_state_generation(state),
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    async fn reprepare_target(
        &self,
        target: ReprepareTarget,
    ) -> Result<SessionChanged, RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Reprepare {
                generation: runtime_state_generation(state),
                target,
                done,
            })
            .map_err(|_| RuntimeError::Unavailable)?;
        result.await.map_err(|_| RuntimeError::Unavailable)?
    }

    /// Stop the runtime. All concurrent callers observe the same terminal result.
    pub async fn shutdown(&self) -> Result<RuntimeExit, RuntimeUnavailable> {
        let mut terminal = self.terminal.clone();
        if let Some(exit) = *terminal.borrow() {
            return Ok(exit);
        }

        let state = self.state.load(Ordering::Acquire);
        let sent = self.tx.send(CodingRuntimeControl::Shutdown {
            generation: runtime_state_generation(state),
        });
        if sent.is_err() {
            if let Some(exit) = *terminal.borrow() {
                return Ok(exit);
            }
            return Err(RuntimeUnavailable);
        }

        loop {
            if let Some(exit) = *terminal.borrow() {
                return Ok(exit);
            }
            terminal.changed().await.map_err(|_| RuntimeUnavailable)?;
        }
    }

    async fn wait_for_terminal(&self) -> Result<RuntimeExit, RuntimeUnavailable> {
        let mut terminal = self.terminal.clone();
        loop {
            if let Some(exit) = *terminal.borrow() {
                return Ok(exit);
            }
            terminal.changed().await.map_err(|_| RuntimeUnavailable)?;
        }
    }
}

impl CodingRuntime {
    /// Build and start a native runtime. Startup errors are explicit; no inert handle is
    /// returned.
    pub async fn start(input: CodingRuntimeStart) -> Result<Self, RuntimeStartError> {
        Self::start_with_bootstrap(input, ProviderBootstrap::Required).await
    }

    pub async fn start_with_bootstrap(
        input: CodingRuntimeStart,
        bootstrap: ProviderBootstrap,
    ) -> Result<Self, RuntimeStartError> {
        Self::start_with_bootstrap_and_session_lease(input, bootstrap, None).await
    }

    /// Start while reusing a lease held across legacy import/ownership commit.
    /// The guard is validated against the prepared project bucket before reuse.
    pub async fn start_with_session_lease(
        input: CodingRuntimeStart,
        bootstrap: ProviderBootstrap,
        lease: SessionLease,
    ) -> Result<Self, RuntimeStartError> {
        Self::start_with_bootstrap_and_session_lease(input, bootstrap, Some(lease)).await
    }

    async fn start_with_bootstrap_and_session_lease(
        input: CodingRuntimeStart,
        bootstrap: ProviderBootstrap,
        session_lease: Option<SessionLease>,
    ) -> Result<Self, RuntimeStartError> {
        let CodingRuntimeStart {
            mut agent,
            prepare,
            provider_factory,
            plugin_hooks,
            image_preprocessor,
        } = input;
        if let Some(config) = agent.subagent_config.clone() {
            crate::provider_factory::install_subagent_tiers(
                provider_factory.clone(),
                &mut agent,
                config.as_ref(),
            );
        }
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let loop_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut parts = prepare_with_plugin_hook_source_reusing_lease(
            &agent,
            prepare.clone(),
            plugin_hooks.as_ref(),
            session_lease,
            true,
        )
        .await
        .map_err(runtime_start_prepare_error)?;
        parts.register_extra_tool(Arc::new(ScheduleWakeupTool::new(
            wakeup_tx.clone(),
            Arc::clone(&loop_active),
        )));
        // Before the mount, because a row mounted in it offers the runtime's own
        // capabilities as commands (`docs/adr/0021` §3) and needs somewhere to
        // send them. The channel is usable the moment it exists; the loop that
        // reads it starts below, and a command that arrives before then waits in
        // it like any other.
        let (handle, controls) = coding_runtime_control_channel();
        parts.set_runtime_commands(Arc::new(handle.clone()));
        let session_id = parts.session.as_ref().map(|binding| binding.id.as_str());
        let session = parts.session.as_ref().map(|binding| RuntimeSessionInfo {
            id: binding.id.clone(),
            resumed: binding.resume.is_some(),
        });
        // Mounted only on the harness engine, and kept for exactly one reason:
        // dropping the `App` unloads every row, and the next command would
        // reach a conversation whose services are gone. It rides in
        // `RuntimeResources` with `parts` because it is the same kind of thing —
        // what a respawn must not lose.
        let mut harness_app: Option<atomcode_plexus::App> = None;
        let mut harness_providers: Option<Arc<crate::on_harness::ProviderSlots>> = None;
        let (kernel_agent, unavailable_reason) =
            match bootstrap {
                ProviderBootstrap::Unavailable(reason) => (None, Some(reason)),
                ProviderBootstrap::Required | ProviderBootstrap::RecoverAuthentication => {
                    match provider_factory.build(&agent, session_id) {
                        Ok(provider) => (
                            Some({
                                let mounted =
                                    mount(&parts, &agent, &prepare, provider).await.map_err(
                                        |e| RuntimeStartError::Assemble(std::io::Error::other(e)),
                                    )?;
                                harness_app = Some(mounted.app);
                                harness_providers = Some(mounted.providers);
                                mounted.handle
                            }),
                            None,
                        ),
                        Err(crate::ProviderBuildError::Authentication(_))
                            if bootstrap == ProviderBootstrap::RecoverAuthentication =>
                        {
                            (
                                None,
                                Some(ProviderUnavailableReason::AuthenticationRequired),
                            )
                        }
                        Err(crate::ProviderBuildError::SourceBuildGatewayUnsupported {
                            ..
                        }) if bootstrap == ProviderBootstrap::RecoverAuthentication => {
                            (None, Some(ProviderUnavailableReason::UnsupportedBuild))
                        }
                        Err(error) => return Err(RuntimeStartError::Provider(error)),
                    }
                }
            };
        parts
            .publish_staged_session()
            .map_err(runtime_start_prepare_error)?;

        let (raw_event_tx, _raw_events) = mpsc::unbounded_channel();
        let (tagged_event_tx, mut tagged_events) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner_with_optional_agent(
            kernel_agent,
            controls,
            raw_event_tx,
            if unavailable_reason.is_some() {
                RuntimePhase::AwaitingProvider
            } else {
                RuntimePhase::Ready
            },
            unavailable_reason,
            true,
            Some(tagged_event_tx),
            Some(RuntimeResources {
                config: agent,
                prepare,
                provider_factory,
                plugin_hooks,
                parts,
                harness_app,
                harness_providers,
                wakeup_tx,
                loop_active,
                image_preprocessor,
            }),
            Some(wakeup_rx),
        );
        let KernelRuntimeAdapter {
            commands: kernel_commands,
            events: mut kernel_events,
            owner_tx,
            owner_task,
        } = adapter;
        let (event_tx, events) = mpsc::unbounded_channel();
        let task_handle = handle.clone();
        let task = tokio::spawn(async move {
            let _owner_lifetime = (kernel_commands, owner_tx);
            let mut sequence = 0u64;
            let mut raw_open = true;
            let mut kernel_open = true;
            let mut receiver_dropped = false;

            while raw_open || kernel_open {
                tokio::select! {
                    _ = event_tx.closed() => {
                        receiver_dropped = true;
                        break;
                    }
                    event = tagged_events.recv(), if raw_open => match event {
                        Some(tagged) => {
                            let envelope = SequencedRuntimeEvent {
                                generation: tagged.generation,
                                sequence,
                                event: tagged.event,
                            };
                            sequence = sequence.wrapping_add(1);
                            if event_tx.send(envelope).is_err() {
                                receiver_dropped = true;
                                break;
                            }
                        }
                        None => raw_open = false,
                    },
                    event = kernel_events.recv(), if kernel_open => match event {
                        Some(event) => {
                            let envelope = SequencedRuntimeEvent {
                                generation: task_handle.status().generation,
                                sequence,
                                event: CodingRuntimeEvent::Agent(event),
                            };
                            sequence = sequence.wrapping_add(1);
                            if event_tx.send(envelope).is_err() {
                                receiver_dropped = true;
                                break;
                            }
                        }
                        None => kernel_open = false,
                    },
                }
            }

            let exit = if receiver_dropped {
                task_handle.shutdown().await.unwrap_or(RuntimeExit {
                    reason: RuntimeExitReason::OwnerStopped,
                    forced: true,
                })
            } else {
                task_handle
                    .wait_for_terminal()
                    .await
                    .unwrap_or(RuntimeExit {
                        reason: RuntimeExitReason::OwnerStopped,
                        forced: true,
                    })
            };
            let _ = owner_task.await;
            if !receiver_dropped {
                let _ = event_tx.send(SequencedRuntimeEvent {
                    generation: task_handle.status().generation,
                    sequence,
                    event: CodingRuntimeEvent::RuntimeStopped(exit),
                });
            }
            exit
        });

        Ok(Self {
            handle,
            events,
            task,
            session,
        })
    }
}

/// Public runtime lifecycle phase. The actor is the sole writer; handles only observe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimePhase {
    Ready,
    InTurn,
    WaitingApproval,
    Reconfiguring,
    AwaitingProvider,
    ShuttingDown,
    Stopped,
    Failed,
}

/// Generation-bound lifecycle snapshot exposed by [`CodingRuntimeHandle::status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub generation: u64,
    pub phase: RuntimePhase,
}

/// Why the runtime owner terminated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeExitReason {
    ShutdownRequested,
    OwnerStopped,
}

/// Stable terminal shared by all shutdown waiters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeExit {
    pub reason: RuntimeExitReason,
    /// The kernel task exceeded the bounded shutdown window and was aborted.
    pub forced: bool,
}

/// The runtime owner side of [`CodingRuntimeHandle`].
///
/// This type intentionally hides the Tokio receiver so ownership stays singular.
#[derive(Debug)]
pub struct CodingRuntimeControlReceiver {
    rx: mpsc::UnboundedReceiver<CodingRuntimeControl>,
    state: Arc<AtomicU64>,
    provider_unavailable_reason: Arc<AtomicU8>,
    terminal_tx: watch::Sender<Option<RuntimeExit>>,
    /// The other end of [`CodingRuntimeHandle::stop`].
    stop: Arc<Mutex<CancellationToken>>,
}

impl CodingRuntimeControlReceiver {
    /// The stop as it stands: cloned before a long await, so firing it later
    /// reaches that await.
    fn stop_now(&self) -> CancellationToken {
        self.stop.lock().expect("stop poisoned").clone()
    }

    /// Put a fresh one in place — the stop that was asked for has been handled,
    /// and the next piece of work is not the one it was aimed at.
    fn stop_handled(&self) {
        *self.stop.lock().expect("stop poisoned") = CancellationToken::new();
    }
}

impl CodingRuntimeControlReceiver {
    pub async fn recv(&mut self) -> Option<CodingRuntimeControl> {
        self.rx.recv().await
    }
}

/// Internal control envelope consumed by the current runtime owner.
///
/// Drivers should use capability methods on [`CodingRuntimeHandle`].
#[doc(hidden)]
pub enum CodingRuntimeControl {
    Compact {
        generation: u64,
        focus: Option<String>,
    },
    Shutdown {
        generation: u64,
    },
    Submit {
        generation: u64,
        input: UserInput,
        done: oneshot::Sender<Result<SubmitReceipt, RuntimeError>>,
    },
    Respond {
        generation: u64,
        id: RequestId,
        value: serde_json::Value,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    ResolvePolicyIntervention {
        generation: u64,
        intervention_id: u64,
        action: PolicyRecoveryAction,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    Snapshot {
        generation: u64,
        done: oneshot::Sender<Result<RuntimeSnapshotReceipt, RuntimeError>>,
    },
    Cancel {
        generation: u64,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    PauseGoal {
        generation: u64,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    SetMode {
        generation: u64,
        mode: RuntimeMode,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    ContextStats {
        generation: u64,
        /// 连系统提示词一起答。见 [`RuntimeContextStats::system_prompt`]。
        prompt: bool,
        done: oneshot::Sender<Result<RuntimeContextStats, RuntimeError>>,
    },
    /// The execution mode in force, decoded from the same three flags the
    /// approval and plan middlewares read.
    ///
    /// Read here rather than mirrored onto the handle, deliberately: a second
    /// copy of "which mode is on" could disagree with the flags that actually
    /// govern a tool call, and the one a person reads on screen would be the
    /// copy. Answering through the owner also orders it against `SetMode` on
    /// the same queue, so a read that follows a set sees it.
    Mode {
        generation: u64,
        done: oneshot::Sender<Result<RuntimeMode, RuntimeError>>,
    },
    WaitMcpReady {
        generation: u64,
        timeout: std::time::Duration,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    McpStatus {
        generation: u64,
        done: oneshot::Sender<Result<McpStatusSnapshot, RuntimeError>>,
    },
    McpTools {
        generation: u64,
        server: String,
        done: oneshot::Sender<Result<McpToolsSnapshot, RuntimeError>>,
    },
    /// "Always allow" one MCP tool: auto-approve it in this session and write it
    /// into the project's `autoApprove`. Runs mid-turn — it is answered while an
    /// approval for that very tool is pending — and needs no rebuild.
    ApproveMcpTool {
        generation: u64,
        alias: String,
        done: oneshot::Sender<Result<Option<McpToolApproval>, RuntimeError>>,
    },
    /// The panel's list: every configured server — disabled ones included — with
    /// the live status, source, config path, transport, auth and tool count.
    McpRows {
        generation: u64,
        done: oneshot::Sender<Result<McpRowsSnapshot, RuntimeError>>,
    },
    /// One configured server in full. An unknown key answers `detail == None`,
    /// which is not an error.
    McpDetail {
        generation: u64,
        server: String,
        done: oneshot::Sender<Result<McpDetailSnapshot, RuntimeError>>,
    },
    /// Do one thing to one configured server:
    /// `docs/mcp-panel-design.md` §5.2's six actions. A refusal (unknown server,
    /// uneditable config, OAuth failure) answers `ReconfigureFailed` carrying the
    /// reason verbatim; `Untrust` and `Logout` answer `Busy` while a turn is
    /// running, because they withdraw the tools before changing trust or auth.
    McpAct {
        generation: u64,
        server: String,
        action: crate::parts::McpAction,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    WithdrawMcpTools {
        generation: u64,
        /// Whether the model is told (see [`crate::told`]).
        tell: bool,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    ToolCatalog {
        generation: u64,
        done: oneshot::Sender<Result<Vec<atomcode_harness::seams::ToolListing>, RuntimeError>>,
    },
    SwitchTool {
        generation: u64,
        pattern: String,
        on: bool,
        done: oneshot::Sender<Result<Vec<atomcode_harness::seams::ToolListing>, RuntimeError>>,
    },
    QueueLocalContext {
        generation: u64,
        input: LocalContextInput,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    ReassembleProvider {
        generation: u64,
        next: CodingAgentConfig,
        done: oneshot::Sender<Result<RuntimeGeneration, RuntimeError>>,
    },
    DeactivateProvider {
        generation: u64,
        reason: ProviderUnavailableReason,
        done: oneshot::Sender<Result<RuntimeGeneration, RuntimeError>>,
    },
    Reprepare {
        generation: u64,
        target: ReprepareTarget,
        done: oneshot::Sender<Result<SessionChanged, RuntimeError>>,
    },
    /// What a capability row asks before it resolves one: the intervention
    /// waiting now, or nothing.
    PendingPolicyIntervention {
        done: oneshot::Sender<Option<PolicyIntervention>>,
    },
    ApplyUndo {
        generation: u64,
        expected_revision: u64,
        /// The turn whose checkpoint the workspace was restored from, when the
        /// same rewind took the workspace back too: recorded beside the
        /// conversation's own fact (`docs/adr/0024` §17).
        code_rewound_to: Option<u64>,
        original: Arc<SessionSnapshot>,
        truncated: SessionSnapshot,
        restored_prompt: String,
        target_n: usize,
        prompts_before: usize,
        done: oneshot::Sender<Result<UndoResult, RuntimeError>>,
    },
    RewindCatalog {
        generation: u64,
        done: oneshot::Sender<Result<RewindCatalog, RuntimeError>>,
    },
    /// The conversation as it stood before `turn`, read off the session log
    /// (see [`undo_to_turn_in_log`]).
    UndoTarget {
        generation: u64,
        expected_revision: u64,
        turn: u64,
        done: oneshot::Sender<Result<SnapshotUndoResult, RuntimeError>>,
    },
    /// Whether a goal or a loop is running, and how far it has got.
    Autonomy {
        generation: u64,
        done: oneshot::Sender<Result<Autonomy, RuntimeError>>,
    },
    /// The account's remaining allowance, as rolling windows.
    Usage {
        generation: u64,
        /// Fetch the windows alone, as the host contract's field of the same
        /// name asks: one call on the account instead of three.
        windows_only: bool,
        done: oneshot::Sender<Result<Allowance, RuntimeError>>,
    },
    /// What this session has changed in the workspace. `file` asks for one
    /// file's diff text instead of the summary of all of them.
    WorkspaceChanges {
        /// Which two things to compare.
        scope: WorkspaceScope,
        generation: u64,
        file: Option<String>,
        done: oneshot::Sender<Result<WorkspaceChanges, RuntimeError>>,
    },
    BeginRewind {
        generation: u64,
        expected_revision: u64,
        point: RewindPoint,
        restore_code: bool,
        target_snapshot: Option<SessionSnapshot>,
        recovery_tx: mpsc::UnboundedSender<CodingRuntimeControl>,
        done: oneshot::Sender<Result<RewindTransactionGuard, RuntimeError>>,
    },
    FinishRewind {
        generation: u64,
        receipt: RewindTransactionReceipt,
        outcome: RewindFinalization,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    RestoreSnapshot {
        generation: u64,
        snapshot: SessionSnapshot,
        done: oneshot::Sender<Result<SessionChanged, RuntimeError>>,
    },
    StartGoal {
        generation: u64,
        condition: String,
        /// What the person attached to the condition. The first round carries
        /// them; later rounds are the evaluator's own words and carry none.
        images: Vec<ImageContent>,
        done: oneshot::Sender<Result<(), RuntimeError>>,
        /// Self-send channel. The owner loop posts the goal's own first round
        /// on it as a [`Submit`](CodingRuntimeControl::Submit), and an
        /// [`AdjustGoalRounds`] once the live per-plan round budget has been
        /// resolved off the loop.
        recovery_tx: mpsc::UnboundedSender<CodingRuntimeControl>,
    },
    /// Self-sent from a background task after goal start: applies the live per-plan
    /// round budget that was resolved off the owner loop, so goal start never blocks
    /// the loop on a quota round-trip. A no-op if the target goal is gone or a newer
    /// generation/controller has taken over.
    AdjustGoalRounds {
        generation: u64,
        controller_id: u64,
        max_rounds: u32,
    },
    StopGoal {
        generation: u64,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    StartLoop {
        /// The person's own cadence, in seconds; `None` leaves it to the model.
        every: Option<u32>,
        generation: u64,
        prompt: String,
        done: oneshot::Sender<Result<(), RuntimeError>>,
        /// Self-send channel, for the loop's own first round.
        recovery_tx: mpsc::UnboundedSender<CodingRuntimeControl>,
    },
    StopLoop {
        generation: u64,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum RewindFinalization {
    Commit,
    Compensate,
    Recover,
}

#[doc(hidden)]
#[derive(Clone)]
pub enum ReprepareTarget {
    Reload {
        plugin_skill_dirs: Option<Vec<(std::path::PathBuf, String)>>,
    },
    ReloadConfig(CodingAgentConfig),
    Fresh,
    Resume(String),
    ResumeWithLease {
        id: String,
        working_dir: std::path::PathBuf,
        lease: SessionLease,
        cancel: Option<tokio_util::sync::CancellationToken>,
    },
    ChangeDirectory(std::path::PathBuf),
}

/// Build the two ends of the stable runtime control channel.
#[doc(hidden)]
pub fn coding_runtime_control_channel() -> (CodingRuntimeHandle, CodingRuntimeControlReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (terminal_tx, terminal) = watch::channel(None);
    // A standalone channel is immediately usable. The runtime owner overrides
    // this flag at spawn time when startup produced only a degraded placeholder.
    let state = Arc::new(AtomicU64::new(runtime_state(0, true)));
    let provider_unavailable_reason = Arc::new(AtomicU8::new(0));
    let stop = Arc::new(Mutex::new(CancellationToken::new()));
    (
        CodingRuntimeHandle {
            tx,
            state: Arc::clone(&state),
            provider_unavailable_reason: Arc::clone(&provider_unavailable_reason),
            terminal,
            stop: Arc::clone(&stop),
        },
        CodingRuntimeControlReceiver {
            rx,
            state,
            provider_unavailable_reason,
            terminal_tx,
            stop,
        },
    )
}

fn encode_provider_unavailable_reason(reason: Option<ProviderUnavailableReason>) -> u8 {
    match reason {
        None => 0,
        Some(ProviderUnavailableReason::NotConfigured) => 1,
        Some(ProviderUnavailableReason::AuthenticationRequired) => 2,
        Some(ProviderUnavailableReason::UnsupportedBuild) => 3,
    }
}

fn decode_provider_unavailable_reason(value: u8) -> Option<ProviderUnavailableReason> {
    match value {
        1 => Some(ProviderUnavailableReason::NotConfigured),
        2 => Some(ProviderUnavailableReason::AuthenticationRequired),
        3 => Some(ProviderUnavailableReason::UnsupportedBuild),
        _ => None,
    }
}

const RUNTIME_PHASE_BITS: u64 = 3;

fn runtime_state(generation: u64, available: bool) -> u64 {
    runtime_phase_state(
        generation,
        if available {
            RuntimePhase::Ready
        } else {
            RuntimePhase::Failed
        },
    )
}

fn runtime_phase_state(generation: u64, phase: RuntimePhase) -> u64 {
    (generation << RUNTIME_PHASE_BITS) | phase_code(phase)
}

fn phase_code(phase: RuntimePhase) -> u64 {
    match phase {
        RuntimePhase::Ready => 0,
        RuntimePhase::InTurn => 1,
        RuntimePhase::WaitingApproval => 2,
        RuntimePhase::Reconfiguring => 3,
        RuntimePhase::ShuttingDown => 4,
        RuntimePhase::Stopped => 5,
        RuntimePhase::Failed => 6,
        RuntimePhase::AwaitingProvider => 7,
    }
}

fn runtime_status(state: u64) -> RuntimeStatus {
    let phase = match state & ((1 << RUNTIME_PHASE_BITS) - 1) {
        0 => RuntimePhase::Ready,
        1 => RuntimePhase::InTurn,
        2 => RuntimePhase::WaitingApproval,
        3 => RuntimePhase::Reconfiguring,
        4 => RuntimePhase::ShuttingDown,
        5 => RuntimePhase::Stopped,
        6 => RuntimePhase::Failed,
        _ => RuntimePhase::AwaitingProvider,
    };
    RuntimeStatus {
        generation: runtime_state_generation(state),
        phase,
    }
}

fn runtime_state_generation(state: u64) -> u64 {
    state >> RUNTIME_PHASE_BITS
}

fn runtime_state_available(state: u64) -> bool {
    matches!(
        runtime_status(state).phase,
        RuntimePhase::Ready | RuntimePhase::InTurn | RuntimePhase::WaitingApproval
    )
}

fn runtime_phase_accepts_command(phase: RuntimePhase, command: &DriverCommand) -> bool {
    if matches!(command, DriverCommand::ResolvePolicyIntervention { .. }) {
        return phase == RuntimePhase::Ready;
    }
    match phase {
        RuntimePhase::Ready | RuntimePhase::InTurn | RuntimePhase::WaitingApproval => true,
        RuntimePhase::AwaitingProvider | RuntimePhase::Failed => matches!(
            command,
            DriverCommand::ReloadProvider(_)
                | DriverCommand::ReprepareConfig(_)
                | DriverCommand::DeactivateProvider(_)
                | DriverCommand::Shutdown
        ),
        RuntimePhase::Reconfiguring | RuntimePhase::ShuttingDown => {
            matches!(command, DriverCommand::Shutdown)
        }
        RuntimePhase::Stopped => false,
    }
}

/// Internal kernel-facing owner adapter wrapped by [`CodingRuntime`].
pub struct KernelRuntimeAdapter {
    pub commands: mpsc::UnboundedSender<AgentCommand>,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    owner_tx: mpsc::UnboundedSender<OwnerControl>,
    owner_task: tokio::task::JoinHandle<()>,
}

impl KernelRuntimeAdapter {
    /// Reject new native compaction controls while a coordinator rebuilds the
    /// underlying agent. Accepted controls from the prior generation terminate as
    /// interrupted rather than crossing into the replacement agent.
    pub async fn suspend_compaction(&self) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::SuspendCompaction { done })
            .await
    }

    /// Resume delivery of native compaction controls after a replacement agent
    /// has been installed successfully.
    pub async fn resume_compaction(&self) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::ResumeCompaction { done })
            .await
    }

    /// Stop the current agent and install an inert placeholder. Used before a
    /// session/provider rebuild whose prepare phase must run after persistence.
    pub async fn stop_agent(&self) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::Stop { done }).await
    }

    /// Atomically replace the current agent after shutting the previous one down.
    pub async fn replace_agent(&self, agent: AgentHandle) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::Replace { agent, done })
            .await
    }

    /// Stop the current agent and terminate the runtime owner.
    pub async fn shutdown(&self) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::Shutdown { done }).await
    }

    async fn manage(
        &self,
        build: impl FnOnce(oneshot::Sender<()>) -> OwnerControl,
    ) -> Result<(), RuntimeUnavailable> {
        let (done_tx, done_rx) = oneshot::channel();
        self.owner_tx
            .send(build(done_tx))
            .map_err(|_| RuntimeUnavailable)?;
        done_rx.await.map_err(|_| RuntimeUnavailable)
    }
}

enum OwnerControl {
    SuspendCompaction {
        done: oneshot::Sender<()>,
    },
    ResumeCompaction {
        done: oneshot::Sender<()>,
    },
    Stop {
        done: oneshot::Sender<()>,
    },
    Replace {
        agent: AgentHandle,
        done: oneshot::Sender<()>,
    },
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

#[derive(Debug)]
struct ManualCompactionFlight {
    trigger: CompactTrigger,
    started: bool,
}

#[derive(Debug, Default)]
struct CompactionTracker {
    manual: VecDeque<ManualCompactionFlight>,
    non_manual_started: Option<CompactTrigger>,
}

impl CompactionTracker {
    fn is_active(&self) -> bool {
        !self.manual.is_empty() || self.non_manual_started.is_some()
    }

    fn accepted_manual(&mut self, trigger: CompactTrigger) {
        self.manual.push_back(ManualCompactionFlight {
            trigger,
            started: false,
        });
    }

    fn started(&mut self, trigger: &CompactTrigger) {
        match trigger {
            CompactTrigger::Manual { .. } => {
                if let Some(flight) = self
                    .manual
                    .iter_mut()
                    .find(|flight| !flight.started && flight.trigger == *trigger)
                {
                    flight.started = true;
                } else {
                    self.manual.push_back(ManualCompactionFlight {
                        trigger: trigger.clone(),
                        started: true,
                    });
                }
            }
            CompactTrigger::Auto { .. } | CompactTrigger::Overflow { .. } => {
                self.non_manual_started = Some(trigger.clone());
            }
        }
    }

    fn finished(&mut self, trigger: &CompactTrigger) {
        match trigger {
            CompactTrigger::Manual { .. } => {
                if let Some(index) = self
                    .manual
                    .iter()
                    .position(|flight| flight.trigger == *trigger)
                {
                    self.manual.remove(index);
                }
            }
            CompactTrigger::Auto { .. } | CompactTrigger::Overflow { .. } => {
                self.non_manual_started = None;
            }
        }
    }

    fn interrupt_all(
        &mut self,
        reason: CompactionInterruption,
        runtime_event_tx: &RuntimeEventEmitter,
    ) {
        for flight in self.manual.drain(..) {
            emit_compaction_interrupted(runtime_event_tx, flight.trigger, reason);
        }
        if let Some(trigger) = self.non_manual_started.take() {
            emit_compaction_interrupted(runtime_event_tx, trigger, reason);
        }
    }
}

/// Start the long-lived owner of the replaceable kernel agent.
///
/// The returned adapter receives every non-compaction kernel event. Native
/// compaction events go straight to `runtime_event_tx`, and controls received on
/// `controls` go straight to whichever kernel agent is current.
pub fn spawn_runtime_owner(
    initial: AgentHandle,
    controls: CodingRuntimeControlReceiver,
    runtime_event_tx: mpsc::UnboundedSender<CodingRuntimeEvent>,
    initial_agent_available: bool,
) -> KernelRuntimeAdapter {
    spawn_runtime_owner_with_protocol(
        initial,
        controls,
        runtime_event_tx,
        initial_agent_available,
        false,
        None,
        None,
        None,
    )
}

fn spawn_runtime_owner_with_protocol(
    initial: AgentHandle,
    controls: CodingRuntimeControlReceiver,
    runtime_event_tx: mpsc::UnboundedSender<CodingRuntimeEvent>,
    initial_agent_available: bool,
    native_protocol: bool,
    tagged_event_tx: Option<mpsc::UnboundedSender<GenerationTaggedRuntimeEvent>>,
    resources: Option<RuntimeResources>,
    wakeup_rx: Option<mpsc::UnboundedReceiver<WakeupRequest>>,
) -> KernelRuntimeAdapter {
    spawn_runtime_owner_with_optional_agent(
        Some(initial),
        controls,
        runtime_event_tx,
        if initial_agent_available {
            RuntimePhase::Ready
        } else {
            RuntimePhase::Failed
        },
        None,
        native_protocol,
        tagged_event_tx,
        resources,
        wakeup_rx,
    )
}

fn spawn_runtime_owner_with_optional_agent(
    initial: Option<AgentHandle>,
    mut controls: CodingRuntimeControlReceiver,
    runtime_event_tx: mpsc::UnboundedSender<CodingRuntimeEvent>,
    initial_phase: RuntimePhase,
    initial_unavailable_reason: Option<ProviderUnavailableReason>,
    native_protocol: bool,
    tagged_event_tx: Option<mpsc::UnboundedSender<GenerationTaggedRuntimeEvent>>,
    mut resources: Option<RuntimeResources>,
    wakeup_rx: Option<mpsc::UnboundedReceiver<WakeupRequest>>,
) -> KernelRuntimeAdapter {
    let (kernel_command_tx, mut kernel_command_rx) = mpsc::unbounded_channel();
    let (kernel_event_tx, kernel_event_rx) = mpsc::unbounded_channel();
    let (owner_tx, mut owner_rx) = mpsc::unbounded_channel();
    let (_closed_wakeup_tx, closed_wakeup_rx) = mpsc::unbounded_channel();
    let mut wakeup_rx = wakeup_rx.unwrap_or(closed_wakeup_rx);
    let (goal_eval_tx, mut goal_eval_rx) = mpsc::unbounded_channel::<EvalOutcome>();
    let (loop_fire_tx, mut loop_fire_rx) = mpsc::unbounded_channel::<(u64, u64, WakeupRequest)>();
    let (next_prompt_tx, mut next_prompt_rx) =
        mpsc::unbounded_channel::<NextPromptSuggestionOutcome>();
    let (team_event_tx, mut team_event_rx) = mpsc::unbounded_channel();
    if let Some(runtime) = resources.as_ref() {
        runtime
            .parts
            .team_manager
            .set_event_sender(team_event_tx.clone());
    }
    let mut generation = 0;
    let event_generation = Arc::new(AtomicU64::new(generation));
    let runtime_event_tx = RuntimeEventEmitter {
        raw: runtime_event_tx,
        tagged: tagged_event_tx,
        generation: Arc::clone(&event_generation),
    };
    controls.state.store(
        runtime_phase_state(generation, initial_phase),
        Ordering::Release,
    );
    controls.provider_unavailable_reason.store(
        encode_provider_unavailable_reason(initial_unavailable_reason),
        Ordering::Release,
    );

    let owner_task = tokio::spawn(async move {
        // Keep the event bus alive while the runtime can install a replacement
        // TeamRunManager during reprepare.
        let _team_event_guard = team_event_tx.clone();
        let mut team_sequences = BTreeMap::new();
        // Keep the fallback receiver pending for transitional owner tests/adapters
        // that do not mount the runtime-owned schedule_wakeup tool.
        let _wakeup_guard = _closed_wakeup_tx;
        let mut agent = initial;
        let mut observed_tokens = None;
        let mut controls_open = true;
        let mut compaction_suspended = false;
        let mut agent_available = matches!(initial_phase, RuntimePhase::Ready);
        let mut provider_unavailable_reason = initial_unavailable_reason;
        let mut compactions = CompactionTracker::default();
        let mut shutdown_was_handled = false;
        let mut forced_shutdown = false;
        let mut exit_reason = RuntimeExitReason::OwnerStopped;
        let mut next_turn_id = 0u64;
        let mut conversation_revision = 0u64;
        let mut active_turn = None;
        // Keep each request kind until its correlated response is delivered.
        // `request_user_input` image answers need runtime-owned preprocessing.
        let mut pending_requests = BTreeMap::new();
        let mut pending_policy_intervention: Option<PolicyIntervention> = None;
        let mut snapshot_waiters: Vec<RuntimeSnapshotWaiter> = Vec::new();
        let mut snapshot_in_flight = false;
        let mut terminal_reason = None;
        // A cancel is not complete when the command is merely delivered. Keep the
        // runtime closed to new submits until the cancelled conversation snapshot
        // arrives, otherwise a prompt can be accepted as a steer and then cleared by
        // the kernel's cancel path.
        let mut cancel_pending = false;
        let mut turn_stats = RuntimeTurnStats::default();
        let mut turn_started_at: Option<std::time::Instant> = None;
        let mut pending_steer_acknowledgements = VecDeque::new();
        let mut next_prompt_task: Option<tokio::task::JoinHandle<()>> = None;
        let mut pending_local_context = Vec::new();
        let mut next_controller_id = 0u64;
        let mut goal: Option<GoalState> = None;
        let mut loop_state: Option<LoopState> = None;
        let mut pending_wakeup: Option<WakeupRequest> = None;
        let mut held_turn: Option<(u64, StopReason, Arc<SessionSnapshot>, RuntimeTurnStats)> = None;
        // Whether the agent itself has work: a turn open, or a message handed
        // to it that it will open one for. Not the same as `active_turn`: a held
        // turn is one this owner keeps open after the agent finished it, and the
        // agent can open another under it (a message typed while `/loop` waits).
        let mut kernel_turn_open = false;
        let mut persistence_failure = None;
        // What each panel `Disable` held back, by server, for the matching
        // `Enable` to give back. Owned here rather than by the parts because a
        // rebuild replaces the parts and the person's switches outlive it.
        let mut mcp_disable_holds: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if agent_available {
            replay_pending_resume_prompt(
                &agent,
                resources.as_mut(),
                controls.state.as_ref(),
                generation,
                &mut next_turn_id,
                &mut active_turn,
                &mut turn_stats,
                &mut agent_available,
            );
        }
        if let Some(reason) = provider_unavailable_reason {
            let _ = runtime_event_tx.send(CodingRuntimeEvent::ProviderUnavailable {
                reason,
                forced: false,
            });
        }
        loop {
            tokio::select! {
                biased;
                team_event = team_event_rx.recv() => {
                    // Team runs persist across turns, so gate their events by the TEAM
                    // manager's own generation (advanced only at `begin_generation` — an
                    // agent rebuild/retask), NOT the per-turn runtime `generation`. A turn
                    // bump that does not rebuild the agent (compaction suspend, stop) would
                    // otherwise drop every subsequent team event and wipe the team panel.
                    let team_generation = resources
                        .as_ref()
                        .map(|runtime| runtime.parts.team_manager.generation())
                        .unwrap_or(generation);
                    if let Some(event) = team_event.and_then(|event| {
                        project_team_event(team_generation, &mut team_sequences, event)
                    }) {
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Team {
                            generation: RuntimeGeneration(team_generation),
                            event,
                        });
                    }
                }
                management = owner_rx.recv() => match management {
                    Some(OwnerControl::SuspendCompaction { done }) => {
                        if !compaction_suspended {
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
                            pending_steer_acknowledgements.clear();
                            compaction_suspended = true;
                            controls
                                .state
                                .store(
                                    runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                                    Ordering::Release,
                                );
                        }
                        let _ = done.send(());
                    }
                    Some(OwnerControl::ResumeCompaction { done }) => {
                        compaction_suspended = false;
                        controls.state.store(
                            runtime_phase_state(
                                generation,
                                if agent_available {
                                    RuntimePhase::Ready
                                } else {
                                    RuntimePhase::Failed
                                },
                            ),
                            Ordering::Release,
                        );
                        let _ = done.send(());
                    }
                    Some(OwnerControl::Stop { done }) => {
                        if !compaction_suspended {
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
                            pending_steer_acknowledgements.clear();
                        }
                        compaction_suspended = true;
                        controls
                            .state
                            .store(
                                runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                                Ordering::Release,
                            );
                        if native_protocol {
                            fail_close_pending_requests(
                                &agent,
                                &mut pending_requests,
                                active_turn.is_some(),
                            );
                        }
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                            resources.as_ref().map(|runtime| &runtime.parts.team_manager),
                            resources
                                .as_ref()
                                .and_then(|runtime| runtime.parts.snapshot_persistence_status()),
                        )
                        .await;
                        if fail_close_after_stopped_persistence(
                            &stop_report,
                            resources.as_ref(),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        )
                        .is_some()
                        {
                            persistence_failure = stop_report.persistence_failure.clone();
                        }
                        agent = None;
                        agent_available = false;
                        observed_tokens = None;
                        let _ = done.send(());
                    }
                    Some(OwnerControl::Replace { agent: replacement, done }) => {
                        if persistence_failure.is_some() {
                            agent = None;
                            agent_available = false;
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Failed),
                                Ordering::Release,
                            );
                            let _ = done.send(());
                            continue;
                        }
                        let resume_after_replace = !compaction_suspended;
                        if resume_after_replace {
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
                            pending_steer_acknowledgements.clear();
                            compaction_suspended = true;
                            controls
                                .state
                                .store(
                                    runtime_phase_state(
                                        generation,
                                        RuntimePhase::Reconfiguring,
                                    ),
                                    Ordering::Release,
                                );
                        }
                        if native_protocol {
                            fail_close_pending_requests(
                                &agent,
                                &mut pending_requests,
                                active_turn.is_some(),
                            );
                        }
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                            resources.as_ref().map(|runtime| &runtime.parts.team_manager),
                            resources
                                .as_ref()
                                .and_then(|runtime| runtime.parts.snapshot_persistence_status()),
                        )
                        .await;
                        if fail_close_after_stopped_persistence(
                            &stop_report,
                            resources.as_ref(),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        )
                        .is_some()
                        {
                            persistence_failure = stop_report.persistence_failure.clone();
                            agent = None;
                            compaction_suspended = false;
                            observed_tokens = None;
                            let _ = done.send(());
                            continue;
                        }
                        agent = Some(replacement);
                        agent_available = true;
                        observed_tokens = None;
                        if let Some(runtime) = resources.as_ref() {
                            runtime.parts.team_manager.begin_generation(generation);
                        }
                        if resume_after_replace {
                            compaction_suspended = false;
                            controls
                                .state
                                .store(
                                    runtime_phase_state(generation, RuntimePhase::Ready),
                                    Ordering::Release,
                                );
                        }
                        let _ = done.send(());
                    }
                    Some(OwnerControl::Shutdown { done }) => {
                        if !compaction_suspended {
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
                            pending_steer_acknowledgements.clear();
                        }
                        controls
                            .state
                            .store(
                                runtime_phase_state(generation, RuntimePhase::ShuttingDown),
                                Ordering::Release,
                            );
                        if native_protocol {
                            fail_close_pending_requests(
                                &agent,
                                &mut pending_requests,
                                active_turn.is_some(),
                            );
                        }
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeShutdown,
                            resources.as_ref().map(|runtime| &runtime.parts.team_manager),
                            resources
                                .as_ref()
                                .and_then(|runtime| runtime.parts.snapshot_persistence_status()),
                        )
                        .await;
                        forced_shutdown = stop_report.forced;
                        if native_protocol {
                            finish_stopped_native_turn(
                                &stop_report,
                                resources.as_ref(),
                                &mut active_turn,
                                &mut terminal_reason,
                                &mut turn_stats,
                                &mut conversation_revision,
                                &mut snapshot_waiters,
                                &runtime_event_tx,
                            );
                        }
                        let _ = fail_close_after_stopped_persistence(
                            &stop_report,
                            resources.as_ref(),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        );
                        interrupt_queued_controls(
                            &mut controls,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeShutdown,
                        );
                        let _ = done.send(());
                        shutdown_was_handled = true;
                        exit_reason = RuntimeExitReason::ShutdownRequested;
                        break;
                    }
                    None => break,
                },
                wakeup = wakeup_rx.recv(), if native_protocol => {
                    if let Some(wakeup) = wakeup {
                        if loop_state.as_ref().is_some_and(|state| state.active) {
                            if let Some(state) = loop_state.as_mut() {
                                state.last_reason = Some(format!("scheduled in {}s: {}", wakeup.delay_seconds, wakeup.reason));
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                            }
                            pending_wakeup = Some(wakeup);
                        }
                    }
                }
                suggestion = next_prompt_rx.recv(), if native_protocol => {
                    let Some(suggestion) = suggestion else { continue };
                    if suggestion.generation == generation
                        && suggestion.revision == conversation_revision
                        && active_turn.is_none()
                    {
                        let _ = runtime_event_tx.send(
                            CodingRuntimeEvent::NextPromptSuggested {
                                generation: RuntimeGeneration(suggestion.generation),
                                session_id: suggestion.session_id,
                                turn_id: suggestion.turn_id,
                                text: suggestion.text,
                            },
                        );
                    }
                }
                outcome = goal_eval_rx.recv(), if native_protocol => {
                    let Some(outcome) = outcome else { continue };
                    if outcome.generation != generation
                        || goal.as_ref().map(|state| state.id) != Some(outcome.controller_id)
                        || held_turn.is_none()
                    {
                        continue;
                    }
                    if let Some(usage) = outcome.usage {
                        if let Some(state) = goal.as_mut() {
                            state.tokens_used = state.tokens_used.saturating_add((usage.prompt + usage.completion) as u64);
                        }
                    }
                    let mut finish_reason = None;
                    let mut continuation = None;
                    match outcome.result {
                        GoalResult::Met(verdict) => {
                            // Met says it once — phase Satisfied, terminal Met, which is
                            // what a screen draws as "已达成" — and then the goal is
                            // CLOSED like every other terminal below. It does not linger
                            // registered waiting to be re-engaged by the next thing a
                            // person types: the badge is gone, so a loop that came back
                            // on its own would be one nobody was told about. `/goal`
                            // starts another.
                            if let Some(state) = goal.as_mut() {
                                state.mark_satisfied(verdict);
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            finish_reason = Some(StopReason::Stopped);
                        }
                        GoalResult::NotMet(verdict) => {
                            // A round that made ZERO tool calls did nothing but talk. When the
                            // evaluator ALSO judges the goal unmet, re-injecting "keep working"
                            // just spins — usually because the goal isn't a concrete, verifiable
                            // objective (e.g. an empty/vague goal like "需要"). Stop after
                            // MAX_STALLED_ROUNDS such rounds instead of burning every round up to
                            // max_rounds.
                            let made_progress = held_turn
                                .as_ref()
                                .map(|(_, _, _, stats)| stats.tool_call_count > 0)
                                .unwrap_or(false);
                            if let Some(state) = goal.as_mut() {
                                state.last_reason = Some(verdict.clone());
                                if let Some((_, _, snapshot, _)) = held_turn.as_ref() {
                                    state.update_progress_recap(summarize_for_goal(
                                        &snapshot.messages,
                                        Some(&verdict),
                                    ));
                                }
                                if state.note_not_met(made_progress) {
                                    // Terminal stall — finish WITHOUT bumping `round` (mirrors the
                                    // Inconclusive/Error arms; the stalled round isn't a new run).
                                    let note = format!(
                                        "stopped: no tool calls for {} consecutive rounds and the goal is still unmet — it likely isn't a concrete, verifiable objective. Give a specific goal and run /loop again.",
                                        state.no_progress
                                    );
                                    state.finish(GoalTerminal::Stopped, note.clone());
                                    finish_reason = Some(StopReason::Stopped);
                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(format!("goal {note}")));
                                } else {
                                    // A new round begins — count it, then re-inject continuation.
                                    state.round = state.round.saturating_add(1);
                                    continuation =
                                        Some(goal_continuation_message(&verdict, &state.condition));
                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                }
                            }
                        }
                        GoalResult::Inconclusive(reason) => {
                            // The evaluator returned no usable verdict (e.g. an
                            // empty stream). This is NOT a failure of the agent's
                            // work and NOT a provider error: the evaluator simply
                            // produced nothing to judge. End the goal as Stopped
                            // (not Failed) and skip the red ControllerWarning —
                            // repeatedly yelling about an empty verdict made the
                            // agent spin in pointless retries (issue #17).
                            if let Some(state) = goal.as_mut() {
                                state.finish(
                                    GoalTerminal::Stopped,
                                    format!("goal evaluation inconclusive: {reason}"),
                                );
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            finish_reason = Some(StopReason::Stopped);
                        }
                        GoalResult::Error(error) => {
                            if let Some(state) = goal.as_mut() {
                                state.finish(
                                    GoalTerminal::Failed,
                                    format!("evaluator failed: {error}"),
                                );
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            finish_reason = Some(StopReason::ProviderError);
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(format!("goal evaluator failed: {error}")));
                        }
                    }
                    if let Some(reason) = finish_reason {
                        if let Some((turn_id, _held_reason, snapshot, stats)) = held_turn.take() {
                            active_turn = None;
                            // Every verdict that ends the goal closes it — Met included.
                            // The GoalChanged above already said which terminal it was.
                            goal = None;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed { turn_id, reason, snapshot, stats }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        }
                    } else if let Some(text) = continuation {
                        if send_agent_command(&agent, AgentCommand::SendSyntheticMessage { text }) {
                            held_turn = None;
                            terminal_reason = None;
                            turn_stats = RuntimeTurnStats::default();
                        } else {
                            agent_available = false;
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.finish(
                                    GoalTerminal::Failed,
                                    "continuation dispatch failed",
                                );
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(
                                "goal stopped: continuation dispatch failed".into(),
                            ));
                            if let Some((turn_id, _held_reason, snapshot, stats)) = held_turn.take() {
                                active_turn = None;
                                terminal_reason = None;
                                turn_stats = RuntimeTurnStats::default();
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                                    TurnCompletion::Completed {
                                        turn_id,
                                        reason: StopReason::ProviderError,
                                        snapshot,
                                        stats,
                                    },
                                ));
                            }
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Failed), Ordering::Release);
                        }
                    }
                }
                fired = loop_fire_rx.recv(), if native_protocol => {
                    let Some((fire_generation, controller_id, wakeup)) = fired else { continue };
                    if fire_generation != generation
                        || loop_state.as_ref().map(|state| state.id) != Some(controller_id)
                        || held_turn.is_none()
                    {
                        continue;
                    }
                    let at_limit = loop_state
                        .as_ref()
                        .is_some_and(LoopState::round_limit_reached);
                    if at_limit {
                        if let Some(mut state) = loop_state.take() {
                            state.active = false;
                            state.last_reason = Some("round limit".into());
                            state.cancel.cancel();
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                        }
                        if let Some(runtime) = resources.as_ref() { runtime.loop_active.store(false, Ordering::Release); }
                        if let Some((turn_id, _held_reason, snapshot, stats)) = held_turn.take() {
                            active_turn = None;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                                turn_id,
                                reason: StopReason::MaxRounds,
                                snapshot,
                                stats,
                            }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        }
                    } else {
                        if let Some(state) = loop_state.as_mut() {
                            state.round = state.round.saturating_add(1);
                            state.last_reason = Some(wakeup.reason);
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                        }
                        if send_agent_command(&agent, AgentCommand::SendMessage { text: wakeup.prompt, images: vec![] }) {
                            held_turn = None;
                            terminal_reason = None;
                            turn_stats = RuntimeTurnStats::default();
                        } else {
                            agent_available = false;
                            if let Some(mut state) = loop_state.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("continuation dispatch failed".into());
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                            }
                            if let Some(runtime) = resources.as_ref() {
                                runtime.loop_active.store(false, Ordering::Release);
                            }
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(
                                "loop stopped: continuation dispatch failed".into(),
                            ));
                            if let Some((turn_id, _held_reason, snapshot, stats)) = held_turn.take() {
                                active_turn = None;
                                terminal_reason = None;
                                turn_stats = RuntimeTurnStats::default();
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                                    TurnCompletion::Completed {
                                        turn_id,
                                        reason: StopReason::ProviderError,
                                        snapshot,
                                        stats,
                                    },
                                ));
                            }
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Failed), Ordering::Release);
                        }
                    }
                }
                control = controls.recv(), if controls_open => match control {
                    Some(control)
                        if persistence_failure.is_some()
                            && !matches!(&control, CodingRuntimeControl::Shutdown { .. }) =>
                    {
                        reject_runtime_control(
                            control,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeUnavailable,
                        );
                    }
                    Some(CodingRuntimeControl::Compact { generation: request_generation, focus }) => {
                        let trigger = CompactTrigger::Manual { focus: focus.clone() };
                        if request_generation != generation || compaction_suspended {
                            emit_compaction_interrupted(
                                &runtime_event_tx,
                                trigger,
                                CompactionInterruption::RuntimeReconfigured,
                            );
                        } else if !agent_available {
                            emit_compaction_interrupted(
                                &runtime_event_tx,
                                trigger,
                                CompactionInterruption::RuntimeUnavailable,
                            );
                        } else if send_agent_command(&agent, AgentCommand::Compact { focus }) {
                            compactions.accepted_manual(trigger);
                        } else {
                            agent_available = false;
                            controls
                                .state
                                .store(
                                    runtime_phase_state(generation, RuntimePhase::Failed),
                                    Ordering::Release,
                                );
                            emit_compaction_interrupted(
                                &runtime_event_tx,
                                trigger,
                                CompactionInterruption::RuntimeUnavailable,
                            );
                        }
                    }
                    Some(CodingRuntimeControl::Submit {
                        generation: request_generation,
                        mut input,
                        done,
                    }) => {
                        if !native_protocol || request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        }
                        if !agent_available {
                            let error = provider_unavailable_reason
                                .map(RuntimeError::ProviderUnavailable)
                                .unwrap_or(RuntimeError::Unavailable);
                            let _ = done.send(Err(error));
                            continue;
                        }
                        if cancel_pending {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        if pending_policy_intervention.is_some() {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        // A goal that is NOT done takes the next message as the nudge
                        // to go on: user-paused (`/goal` with no argument) or paused at
                        // its round / time cap. It resumes into Pursuing, so the badge
                        // shows `◎ <cond> · round 1` again, and the message it resumed
                        // on is that round's prompt.
                        //
                        // A goal that IS done does NOT come back. It was closed when the
                        // evaluator said so (see the Met arm of the eval outcome) and the
                        // badge went with it; starting an autonomous loop again is
                        // `/goal`, which is one word and cannot be guessed wrong.
                        //
                        // Nothing here may wait on a model. This is the keypress path:
                        // what a person typed reaches the agent now, not after a network
                        // round-trip — the budget is the one derived at goal-START time
                        // (`state.max_rounds`), and deciding "does this follow-up
                        // continue the goal?" by asking a classifier here is what used to
                        // sit on this loop for up to 4s with nothing on screen to say so.
                        let mut recovery_context = None;
                        // PausedAtCap and Paused resume even on an empty submit: neither
                        // is done, so any nudge should let it keep going.
                        let reengage = matches!(
                            goal.as_ref().map(|state| state.phase),
                            Some(GoalPhase::Paused | GoalPhase::PausedAtCap)
                        );
                        if reengage {
                            if let Some(state) = goal.as_mut() {
                                let was_user_paused = state.phase == GoalPhase::Paused;
                                recovery_context = state.recovery_context();
                                if was_user_paused {
                                    state.resume_paused();
                                } else {
                                    let keep = state.max_rounds.unwrap_or(0);
                                    state.resume(keep);
                                }
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                        }
                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }
                        // The real-user submit boundary owns per-turn execution intent.
                        // Update before forwarding (including steer) so a newly received
                        // "do not compile/run scripts" instruction blocks later Bash calls
                        // from an already-active turn without waiting for another LLM round.
                        // Skip an empty-text submit (e.g. an image-only steer): it carries no
                        // new intent and must not clear a restriction the user set earlier in
                        // this turn.
                        if let Some(runtime) = resources.as_ref() {
                            if !input.text.trim().is_empty() {
                                runtime
                                    .parts
                                    .turn_execution_policy
                                    .update_from_user_text(&input.text);
                            }
                        }
                        // A HELD turn is one the agent has already finished — the runtime
                        // is keeping it open while a controller decides what comes next
                        // (a goal round's evaluator). A person who types now is not
                        // steering anything: there is no live turn at the agent to fold
                        // into, so the message would open one of its own while this
                        // runtime still claimed the old turn was live. Two things went
                        // wrong from that: the driver was told `Steered` and then never
                        // got the `Steered` event that closes a steering panel, and a
                        // verdict arriving later finished the HELD turn while the agent
                        // was busy with the person's new one — the screen went idle in
                        // the middle of work.
                        //
                        // So a GOAL's hold ends here: the round is over, its terminal is
                        // reported, and the message starts a fresh turn. The goal itself
                        // is untouched and still Pursuing, so the END of that turn
                        // evaluates and continues it exactly as any other round's would —
                        // no round is lost. The evaluation now in flight resolves against
                        // `held_turn.is_none()` and is dropped.
                        //
                        // A `/loop`'s hold is NOT ended: what resolves it is a timer, and
                        // the loop's next round exists only as that pending wakeup —
                        // dropping it would stop the loop rather than interrupt a round.
                        // Its own receipt is handled below (no steer acknowledgement is
                        // registered while a turn is held, because no `Steered` is coming).
                        let goal_holds_the_turn = goal.as_ref().is_some_and(|state| state.active);
                        if let Some((turn_id, held_reason, snapshot, stats)) =
                            goal_holds_the_turn.then(|| held_turn.take()).flatten()
                        {
                            active_turn = None;
                            terminal_reason = None;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                                TurnCompletion::Completed {
                                    turn_id,
                                    reason: held_reason,
                                    snapshot,
                                    stats,
                                },
                            ));
                        }
                        let receipt = if let Some(turn_id) = active_turn {
                            SubmitReceipt::Steered { generation, turn_id }
                        } else {
                            next_turn_id = next_turn_id.wrapping_add(1);
                            active_turn = Some(next_turn_id);
                            turn_stats = RuntimeTurnStats::default();
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::InTurn),
                                Ordering::Release,
                            );
                            SubmitReceipt::Started {
                                generation,
                                turn_id: next_turn_id,
                            }
                        };
                        // A steer acknowledgement is only owed when the kernel will fold
                        // this input into a live turn. With the turn HELD the agent has no
                        // turn to fold into, so no `Steered` will ever arrive — and an
                        // entry that can never match sits at the head of this FIFO and
                        // blocks the acknowledgement of every later input behind it.
                        let original_steer_input = (matches!(receipt, SubmitReceipt::Steered { .. })
                            && held_turn.is_none())
                            .then(|| input.clone());
                        if !pending_local_context.is_empty() {
                            let prefix = pending_local_context.drain(..).collect::<Vec<_>>().join("\n\n");
                            input.text = if input.text.is_empty() {
                                prefix
                            } else {
                                format!("{prefix}\n\n{}", input.text)
                            };
                        }
                        // Vision (VL) preprocessing: when the turn carries images and a
                        // preprocessor is installed, rewrite `(text, images)` before the
                        // kernel turn — a non-vision model gets a VL text description with
                        // images cleared; a vision model passes through. Awaited HERE (turn
                        // already marked in-progress above, spinner showing) so the
                        // multi-second VL call never blocks the caller. `None` preprocessor
                        // (or empty images) is a no-op.
                        if !input.images.is_empty() {
                            if let Some(pp) =
                                resources.as_ref().and_then(|r| r.image_preprocessor.clone())
                            {
                                // Authoritative active-turn model + session id come
                                // from the runtime's own resolved resources, not a
                                // re-read config default (which would miss a
                                // `--provider` override).
                                let supports_vision = resources
                                    .as_ref()
                                    .is_some_and(|r| r.config.supports_vision);
                                let session_id = resources
                                    .as_ref()
                                    .and_then(|r| r.parts.session.as_ref())
                                    .map(|b| b.id.clone());
                                // Raced against the stop, because this await is
                                // inside the owner's own loop: a `Cancel` sent
                                // now sits behind it on the channel, unread, and
                                // recognition has no overall time cap (only a
                                // 30s gap-between-chunks one). That is how esc
                                // under a pasted image did nothing for as long
                                // as the VL model took.
                                let stop = controls.stop_now();
                                let recognised = tokio::select! {
                                    biased;
                                    _ = stop.cancelled() => None,
                                    done = pp.preprocess(
                                        std::mem::take(&mut input.text),
                                        std::mem::take(&mut input.images),
                                        supports_vision,
                                        session_id,
                                    ) => Some(done),
                                };
                                let Some((new_input, notice)) = recognised else {
                                    // Nothing is sent to the agent: the turn the
                                    // person stopped never reaches it, and the
                                    // `Cancel` right behind this on the channel
                                    // ends the turn this arm opened.
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::VisionPreprocessFailed {
                                            reason: "stopped before the picture was read".into(),
                                        },
                                    );
                                    let _ = done.send(Ok(receipt));
                                    continue;
                                };
                                input = new_input;
                                // Surface the outcome as a status line, emitted
                                // BEFORE SendMessage so it renders right under the
                                // user message, ahead of the assistant response.
                                match notice {
                                    Some(VisionNotice::Recognised { vl_model, char_count }) => {
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::VisionPreprocessSuccess {
                                                vl_model,
                                                char_count,
                                            },
                                        );
                                    }
                                    Some(VisionNotice::Failed { reason }) => {
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::VisionPreprocessFailed { reason },
                                        );
                                    }
                                    None => {}
                                }
                            }
                        }
                        let command = match recovery_context {
                            Some(context) => AgentCommand::SendMessageWithContext {
                                text: input.text,
                                images: input.images,
                                context,
                            },
                            None => AgentCommand::SendMessage {
                                text: input.text,
                                images: input.images,
                            },
                        };
                        // Only a plain `SendMessage` can enter the kernel's
                        // in-turn steer buffer. A context-bearing message is
                        // deliberately queued by the kernel for the next turn,
                        // so registering it here would leave an acknowledgement
                        // at the FIFO head that can never match `Steered` and
                        // would block every later acknowledgement behind it.
                        let forwarded_steer_input = forwarded_steer_for_acknowledgement(
                            original_steer_input.as_ref(),
                            &command,
                        );
                        if send_agent_command(&agent, command) {
                            // The agent opens a turn for it (or folds it into
                            // the one it has): as far as a stop is concerned it
                            // has work from here, not from its `TurnStarted`,
                            // which can arrive after the stop does.
                            kernel_turn_open = true;
                            if let (Some(original), Some(forwarded)) =
                                (original_steer_input, forwarded_steer_input)
                            {
                                pending_steer_acknowledgements.push_back(
                                    PendingSteerAcknowledgement {
                                        generation,
                                        original,
                                        forwarded,
                                    },
                                );
                            }
                            let _ = done.send(Ok(receipt));
                        } else {
                            agent_available = false;
                            active_turn = None;
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Failed),
                                Ordering::Release,
                            );
                            let _ = done.send(Err(RuntimeError::DeliveryFailed));
                        }
                    }
                    Some(CodingRuntimeControl::Respond {
                        generation: request_generation,
                        id,
                        value,
                        done,
                    }) => {
                        if !native_protocol || request_generation != generation || !agent_available {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                        } else if !pending_requests.contains_key(&id) {
                            let _ = done.send(Err(RuntimeError::StaleRequest { id }));
                        } else {
                            let kind = pending_requests
                                .remove(&id)
                                .expect("pending request checked above");
                            let value = preprocess_request_response(
                                &kind,
                                value,
                                resources.as_ref(),
                                &runtime_event_tx,
                            )
                            .await;
                            if !send_agent_command(&agent, AgentCommand::Respond { id, value }) {
                                agent_available = false;
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Failed),
                                    Ordering::Release,
                                );
                                let _ = done.send(Err(RuntimeError::DeliveryFailed));
                            } else {
                                if pending_requests.is_empty() {
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::InTurn),
                                        Ordering::Release,
                                    );
                                }
                                let _ = done.send(Ok(()));
                            }
                        }
                    }
                    Some(CodingRuntimeControl::PendingPolicyIntervention { done }) => {
                        let _ = done.send(pending_policy_intervention.clone());
                    }
                    Some(CodingRuntimeControl::ResolvePolicyIntervention {
                        generation: request_generation,
                        intervention_id,
                        action,
                        done,
                    }) => {
                        if request_generation != generation
                            || controls.state.load(Ordering::Acquire)
                                != runtime_phase_state(generation, RuntimePhase::Ready)
                        {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                        } else if pending_policy_intervention.as_ref().is_none() {
                            let _ = done.send(Err(RuntimeError::NoPendingPolicyIntervention));
                        } else if pending_policy_intervention
                            .as_ref()
                            .is_some_and(|intervention| intervention.id != intervention_id)
                        {
                            let _ = done.send(Err(RuntimeError::NoPendingPolicyIntervention));
                        } else if !pending_policy_intervention
                            .as_ref()
                            .is_some_and(|intervention| intervention.actions.contains(&action))
                            || matches!(action, PolicyRecoveryAction::ViewSafeInstructions)
                        {
                            let _ = done.send(Err(RuntimeError::InvalidPolicyRecoveryAction));
                        } else {
                            pending_policy_intervention = None;
                            let _ = runtime_event_tx.send(
                                CodingRuntimeEvent::PolicyInterventionResolved {
                                    intervention_id,
                                    action,
                                },
                            );
                            let _ = done.send(Ok(()));
                        }
                    }
                    Some(CodingRuntimeControl::Snapshot {
                        generation: request_generation,
                        done,
                    }) => {
                        if !native_protocol || request_generation != generation || !agent_available {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                        } else {
                            snapshot_waiters.push(done);
                            if active_turn.is_none() && !snapshot_in_flight {
                                if send_agent_command(&agent, AgentCommand::Snapshot) {
                                    snapshot_in_flight = true;
                                } else {
                                    agent_available = false;
                                    let error = RuntimeError::DeliveryFailed;
                                    for waiter in snapshot_waiters.drain(..) {
                                        let _ = waiter.send(Err(error.clone()));
                                    }
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Failed),
                                        Ordering::Release,
                                    );
                                }
                            }
                        }
                    }
                    Some(CodingRuntimeControl::RewindCatalog {
                        generation: request_generation,
                        done,
                    }) => {
                        if request_generation != generation
                            || active_turn.is_some()
                            || compaction_suspended
                            || compactions.is_active()
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let Some(hook) = runtime.parts.snapshot_hook() else {
                            let _ = done.send(Ok(RewindCatalog {
                                generation: RuntimeGeneration(generation),
                                revision: conversation_revision,
                                points: Vec::new(),
                                code_unavailable: Some(CodeUnavailable::NoSession),
                            }));
                            continue;
                        };
                        if let Some(reason) = hook.rewind_transaction_unavailable() {
                            let _ = done.send(Err(RuntimeError::ReconfigureFailed(reason)));
                            continue;
                        }
                        let _ = done.send(Ok(RewindCatalog {
                            generation: RuntimeGeneration(generation),
                            revision: conversation_revision,
                            points: reachable_points(runtime, hook.rewind_points()),
                            code_unavailable: hook.code_rewind_unavailable().map(Into::into),
                        }));
                    }
                    Some(CodingRuntimeControl::UndoTarget {
                        generation: request_generation,
                        expected_revision,
                        turn,
                        done,
                    }) => {
                        if request_generation != generation
                            || expected_revision != conversation_revision
                            || active_turn.is_some()
                            || compaction_suspended
                            || compactions.is_active()
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let events = session_events(runtime);
                        let _ = done.send(events.and_then(|events| undo_to_turn_in_log(&events, turn)));
                    }
                    // The two controllers live as locals of this loop, which is
                    // why this is a message rather than a field somebody reads:
                    // a second copy of "is a goal running" is a second answer.
                    Some(CodingRuntimeControl::Autonomy {
                        generation: request_generation,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let _ = done.send(Ok(Autonomy {
                            goal: goal.as_ref().map(|state| state.progress()),
                            looping: loop_state.as_ref().map(|state| state.progress()),
                        }));
                    }
                    // Bounded, and off the loop: the source is an HTTP call,
                    // and this loop is what every turn goes through. Three
                    // seconds is the same budget `resolve_goal_round_cap` gives
                    // it — a person asking what is left waits no longer than a
                    // goal starting does.
                    Some(CodingRuntimeControl::Usage {
                        generation: request_generation,
                        windows_only,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let source = resources
                            .as_ref()
                            .and_then(|runtime| runtime.parts.rate_limit_source().cloned());
                        tokio::spawn(async move {
                            // No source at all: this really is a host that
                            // counts nothing, and an empty answer says so.
                            let Some(source) = source else {
                                let _ = done.send(Ok(Allowance::default()));
                                return;
                            };
                            // Asked for together, not one after another: these
                            // are three separate calls on the same account, and
                            // a page that waited three times as long to show
                            // one screen is a page that feels broken. One
                            // budget each, but they run at once, so the page is
                            // late by the slowest rather than by the sum.
                            let budget = std::time::Duration::from_secs(3);
                            // The cheap form asks for the windows and stops
                            // there. It exists for the periodic check, which
                            // wants "how much is left" and nothing else —
                            // asking the other two on a timer would triple the
                            // traffic to say the same thing.
                            if windows_only {
                                let asked =
                                    tokio::time::timeout(budget, source.fetch_windows()).await;
                                let _ = done.send(Ok(Allowance {
                                    windows: windows_or_nothing(&asked),
                                    unavailable: why_not(&asked),
                                    ..Allowance::default()
                                }));
                                return;
                            }
                            let (windows, plan, spent) = tokio::join!(
                                tokio::time::timeout(budget, source.fetch_windows()),
                                tokio::time::timeout(budget, source.fetch_plan()),
                                tokio::time::timeout(budget, source.fetch_usage()),
                            );
                            // Each answer stands or falls on its own: a plan
                            // the service would not say is not a reason to draw
                            // no windows.
                            let plan = plan.ok().and_then(|fetched| fetched.ok()).flatten();
                            let spent = spent.ok().and_then(|fetched| fetched.ok()).flatten();
                            let _ = done.send(Ok(Allowance {
                                windows: windows_or_nothing(&windows),
                                plan,
                                spent,
                                // The windows are the answer this question is
                                // about. A plan or a spend that did not come
                                // back leaves its own field empty and says
                                // nothing more — those two are extra.
                                unavailable: why_not(&windows),
                            }));
                        });
                    }
                    // Reading only: unlike the rewind catalog this does not
                    // refuse while a turn is running. Looking at what has
                    // changed so far is exactly what a person does *while* the
                    // model works, and nothing here writes.
                    Some(CodingRuntimeControl::WorkspaceChanges {
                        generation: request_generation,
                        file,
                        scope,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        // The checkout's own answer needs no session history —
                        // only git — which is why "this session does not keep
                        // snapshots" is never its reason for having none.
                        if scope == WorkspaceScope::Git {
                            let at = runtime.config.working_dir.clone();
                            let _ = done.send(Ok(git_workspace_changes(&at, file.as_deref())));
                            continue;
                        }
                        let Some(hook) = runtime.parts.snapshot_hook() else {
                            let _ = done.send(Ok(WorkspaceChanges {
                                unavailable: Some("这个会话不做工作区快照".into()),
                                ..Default::default()
                            }));
                            continue;
                        };
                        let answer = match file {
                            Some(path) => hook.file_diff(&path).map(|diff| WorkspaceChanges {
                                diff: Some(diff),
                                ..Default::default()
                            }),
                            None => hook.changes().map(|files| WorkspaceChanges {
                                files,
                                ..Default::default()
                            }),
                        };
                        let _ = done.send(Ok(answer.unwrap_or_else(|why| WorkspaceChanges {
                            unavailable: Some(why),
                            ..Default::default()
                        })));
                    }
                    Some(CodingRuntimeControl::BeginRewind {
                        generation: request_generation,
                        expected_revision,
                        point,
                        restore_code,
                        target_snapshot,
                        recovery_tx,
                        done,
                    }) => {
                        if request_generation != generation
                            || expected_revision != conversation_revision
                            || active_turn.is_some()
                            || compaction_suspended
                            || compactions.is_active()
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let Some(hook) = runtime.parts.snapshot_hook() else {
                            let _ = done.send(Err(RuntimeError::CodeRewindUnavailable(
                                "rewind requires a persistent session".into(),
                            )));
                            continue;
                        };
                        if !hook
                            .rewind_points()
                            .iter()
                            .any(|candidate| candidate == &point)
                        {
                            let _ = done.send(Err(RuntimeError::RewindPointUnavailable {
                                turn_id: point.turn_id,
                            }));
                            continue;
                        }
                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let hook = Arc::clone(&hook);
                        let point = point.clone();
                        let receipt = match tokio::task::spawn_blocking(move || {
                            hook.begin_rewind(&point, restore_code, target_snapshot)
                        })
                        .await
                        {
                            Ok(Ok(receipt)) => receipt,
                            Ok(Err(error)) => {
                                let compensation_failed = matches!(
                                    &error,
                                    atomcode_capabilities::session::WorkspaceCheckpointError::Compensation { .. }
                                );
                                if compensation_failed {
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Failed),
                                        Ordering::Release,
                                    );
                                    agent_available = false;
                                    let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                                    agent = None;
                                } else {
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Ready),
                                        Ordering::Release,
                                    );
                                }
                                let error = if restore_code {
                                    RuntimeError::CodeRewindUnavailable(error.to_string())
                                } else {
                                    RuntimeError::ReconfigureFailed(format!(
                                        "rewind checkpoint update failed: {error}"
                                    ))
                                };
                                let _ = done.send(Err(error));
                                continue;
                            }
                            Err(error) => {
                                // A panicked blocking transaction may have
                                // mutated the worktree or ledger after writing
                                // its recovery journal. Do not claim Ready.
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Failed),
                                    Ordering::Release,
                                );
                                agent_available = false;
                                let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                                agent = None;
                                let _ = done.send(Err(RuntimeError::CodeRewindUnavailable(
                                    format!("rewind checkpoint task failed: {error}"),
                                )));
                                continue;
                            }
                        };
                        // The guard is installed before crossing the oneshot
                        // boundary. If the receiver disappears before polling
                        // the delivered value, dropping the channel payload
                        // still queues Recover back to this owner.
                        let transaction =
                            RewindTransactionGuard::new(recovery_tx, generation, receipt);
                        let _ = done.send(Ok(transaction));
                    }
                    Some(CodingRuntimeControl::FinishRewind {
                        generation: request_generation,
                        receipt,
                        outcome,
                        done,
                    }) => {
                        if outcome != RewindFinalization::Recover
                            && request_generation != generation
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let Some(hook) = runtime.parts.snapshot_hook() else {
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Failed),
                                Ordering::Release,
                            );
                            agent_available = false;
                            let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                            agent = None;
                            let _ = done.send(Err(RuntimeError::CodeRewindUnavailable(
                                "rewind finalization lost its session checkpoint".into(),
                            )));
                            continue;
                        };
                        // Only the files went back: the conversation above still
                        // describes edits that are no longer on disk.
                        let code_only = (outcome == RewindFinalization::Commit
                            && !receipt.takes_back_conversation()
                            && !receipt.restored_files().is_empty())
                        .then(|| receipt.restored_files().to_vec());
                        let result = tokio::task::spawn_blocking(move || match outcome {
                            RewindFinalization::Commit => hook.commit_rewind(receipt),
                            RewindFinalization::Compensate => hook.compensate_rewind(receipt),
                            RewindFinalization::Recover => hook.recover_rewind(receipt),
                        })
                        .await;
                        let failure = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(error)) => Some(format!(
                                "rewind {} failed: {error}",
                                match outcome {
                                    RewindFinalization::Commit => "commit",
                                    RewindFinalization::Compensate => "compensation",
                                    RewindFinalization::Recover => "recovery",
                                }
                            )),
                            Err(error) => Some(format!(
                                "rewind {} task failed: {error}",
                                match outcome {
                                    RewindFinalization::Commit => "commit",
                                    RewindFinalization::Compensate => "compensation",
                                    RewindFinalization::Recover => "recovery",
                                }
                            )),
                        };
                        if let Some(message) = failure {
                            // The durable journal remains authoritative and will
                            // retry recovery on the next session open. This
                            // runtime must stop accepting work because its
                            // in-memory conversation/worktree relationship is
                            // no longer proven consistent.
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Failed),
                                Ordering::Release,
                            );
                            agent_available = false;
                            let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                            agent = None;
                            let _ = done.send(Err(RuntimeError::ReconfigureFailed(message)));
                            continue;
                        }
                        if let Some(files) = code_only {
                            tell(
                                runtime,
                                Some(crate::told::code_restored_conversation_kept(&files)),
                            );
                        }
                        if controls.state.load(Ordering::Acquire)
                            == runtime_phase_state(generation, RuntimePhase::Reconfiguring)
                        {
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Ready),
                                Ordering::Release,
                            );
                        }
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::Cancel {
                        generation: request_generation,
                        done,
                    }) => {
                        // Whatever the stop reached (or did not), it is answered
                        // from here on; a fresh one waits for the next.
                        controls.stop_handled();
                        if native_protocol
                            && request_generation == generation
                            && agent_available
                        {
                            if let Some(runtime) = resources.as_ref() {
                                runtime.parts.team_manager.stop_all().await;
                            }
                        }
                        if !native_protocol || request_generation != generation || !agent_available {
                            // Which of the three refused is the whole answer to
                            // "esc did not stop the turn": a stale generation
                            // means the runtime was rebuilt under the driver, and
                            // `agent_available == false` means there is no agent
                            // left to stop. Neither is a bug in the cancel path,
                            // and without this line a report of the symptom
                            // cannot be told apart from one that is.
                            tracing::warn!(
                                protocol = native_protocol,
                                requested_generation = request_generation,
                                current_generation = generation,
                                agent_available,
                                "a cancel was refused before it reached the agent"
                            );
                            let _ = done.send(Err(RuntimeError::Unavailable));
                        } else if let Some((turn_id, _, snapshot, stats)) =
                            // A hold with the agent idle under it is closed here
                            // and now. One the agent has opened a turn under is
                            // not: that turn is what the person is stopping, so
                            // it takes the branch that tells the agent (below).
                            held_turn.take_if(|_| !kernel_turn_open)
                        {
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.finish(GoalTerminal::Cancelled, "cancelled by user");
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            if let Some(mut state) = loop_state.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("cancelled by user".into());
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                            }
                            if let Some(runtime) = resources.as_ref() {
                                runtime.loop_active.store(false, Ordering::Release);
                            }
                            pending_wakeup = None;
                            active_turn = None;
                            cancel_pending = false;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                                TurnCompletion::Completed {
                                    turn_id,
                                    reason: StopReason::Cancelled,
                                    snapshot,
                                    stats,
                                },
                            ));
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Ready),
                                Ordering::Release,
                            );
                            let _ = done.send(Ok(()));
                        } else if active_turn.is_none() {
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.finish(GoalTerminal::Cancelled, "cancelled by user");
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            if let Some(mut state) = loop_state.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("cancelled by user".into());
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::LoopChanged(state.progress()));
                            }
                            if let Some(runtime) = resources.as_ref() {
                                runtime.loop_active.store(false, Ordering::Release);
                            }
                            pending_wakeup = None;
                            cancel_pending = false;
                            let _ = done.send(Ok(()));
                        } else {
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.finish(GoalTerminal::Cancelled, "cancelled by user");
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            if let Some(mut state) = loop_state.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("cancelled by user".into());
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                            }
                            if let Some(runtime) = resources.as_ref() {
                                runtime.loop_active.store(false, Ordering::Release);
                            }
                            pending_wakeup = None;
                            // The hold, if there was one, ends with the turn
                            // the agent opened under it: one terminal, for the
                            // turn the person was looking at.
                            held_turn = None;
                            for id in pending_requests.keys().copied() {
                                let _ = send_agent_command(&agent, AgentCommand::Respond {
                                    id,
                                    value: serde_json::Value::Null,
                                });
                            }
                            pending_requests.clear();
                            // Queue Snapshot immediately after Cancel. Mid-turn the kernel
                            // drains it after the cancelled turn; if Cancel races with an
                            // already-idle kernel, Snapshot still supplies the terminal
                            // acknowledgement that the runtime needs to leave InTurn.
                            if request_cancel_snapshot(&agent) {
                                cancel_pending = true;
                                snapshot_in_flight = true;
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::InTurn),
                                    Ordering::Release,
                                );
                                let _ = done.send(Ok(()));
                            } else {
                                agent_available = false;
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Failed),
                                    Ordering::Release,
                                );
                                let _ = done.send(Err(RuntimeError::DeliveryFailed));
                            }
                        }
                    }
                    Some(CodingRuntimeControl::PauseGoal {
                        generation: request_generation,
                        done,
                    }) => {
                        if !native_protocol || request_generation != generation || !agent_available {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        }
                        let Some(state) = goal.as_mut() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        state.pause("paused by user; continue the conversation to resume");
                        let _ = runtime_event_tx
                            .send(CodingRuntimeEvent::GoalChanged(state.progress()));
                        pending_wakeup = None;

                        if let Some((turn_id, _, snapshot, stats)) =
                            held_turn.take_if(|_| !kernel_turn_open)
                        {
                            active_turn = None;
                            cancel_pending = false;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                                TurnCompletion::Completed {
                                    turn_id,
                                    reason: StopReason::Cancelled,
                                    snapshot,
                                    stats,
                                },
                            ));
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Ready),
                                Ordering::Release,
                            );
                            let _ = done.send(Ok(()));
                        } else if held_turn.is_some() {
                        // A turn the agent opened under the hold is not what
                        // this stops: the person asked for the controller to
                        // stop, not for their own message to. The hold goes, so
                        // the turn now running is the one this owner accounts
                        // for, and its own end reports it.
                            held_turn = None;
                            let _ = done.send(Ok(()));
                        } else if active_turn.is_none() {
                            cancel_pending = false;
                            let _ = done.send(Ok(()));
                        } else {
                            for id in pending_requests.keys().copied() {
                                let _ = send_agent_command(&agent, AgentCommand::Respond {
                                    id,
                                    value: serde_json::Value::Null,
                                });
                            }
                            pending_requests.clear();
                            if request_cancel_snapshot(&agent) {
                                cancel_pending = true;
                                snapshot_in_flight = true;
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::InTurn),
                                    Ordering::Release,
                                );
                                let _ = done.send(Ok(()));
                            } else {
                                if let Some(mut state) = goal.take() {
                                    state.finish(
                                        GoalTerminal::Failed,
                                        "pause failed: cancel delivery failed",
                                    );
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::GoalChanged(state.progress()),
                                    );
                                }
                                agent_available = false;
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Failed),
                                    Ordering::Release,
                                );
                                let _ = done.send(Err(RuntimeError::DeliveryFailed));
                            }
                        }
                    }
                    Some(CodingRuntimeControl::SetMode {
                        generation: request_generation,
                        mode,
                        done,
                    }) => {
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        if request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let before = current_mode(&runtime.parts);
                        runtime.parts.plan_mode.store(
                            matches!(mode, RuntimeMode::Plan),
                            Ordering::Release,
                        );
                        runtime.parts.bypass_mode.store(
                            matches!(mode, RuntimeMode::Auto),
                            Ordering::Release,
                        );
                        runtime.parts.accept_edits.store(
                            matches!(mode, RuntimeMode::AcceptEdits),
                            Ordering::Release,
                        );
                        tell(runtime, crate::told::mode_changed(before, mode));
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::ModeChanged { mode });
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::Mode {
                        generation: request_generation,
                        done,
                    }) => {
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let _ = done.send(Ok(current_mode(&runtime.parts)));
                    }
                    Some(CodingRuntimeControl::ContextStats {
                        generation: request_generation,
                        prompt,
                        done,
                    }) => {
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let used_tokens = observed_tokens
                            .unwrap_or_default()
                            .min(u32::MAX as usize) as u32;
                        let context_window = runtime.config.context_window;
                        let utilization = if context_window == 0 {
                            0.0
                        } else {
                            used_tokens as f32 / context_window as f32
                        };
                        // 每一段都是挂着的某一行写的,所以这里渲染的就是模型
                        // 真正收到的那一份 —— 不是这一层照着记忆重拼一份。
                        let system_prompt = prompt
                            .then(|| {
                                let app = runtime.harness_app.as_ref()?;
                                let prompts = app
                                    .context()
                                    .service::<atomcode_harness::seams::SystemPromptSvc>()?;
                                Some(prompts.render())
                            })
                            .flatten();
                        let _ = done.send(Ok(RuntimeContextStats {
                            context_window,
                            used_tokens,
                            utilization,
                            model: runtime.config.model.clone(),
                            working_dir: runtime.config.working_dir.clone(),
                            system_prompt,
                        }));
                    }
                    Some(CodingRuntimeControl::WaitMcpReady {
                        generation: request_generation,
                        timeout,
                        done,
                    }) => {
                        if request_generation != generation
                            || active_turn.is_some()
                            || compaction_suspended
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let mut readiness = runtime.parts.mcp_readiness_receiver();
                        // Waiting for supplemental MCP readiness must not stop the
                        // runtime owner from processing cancel/reload/shutdown.
                        tokio::spawn(async move {
                            if !*readiness.borrow_and_update() {
                                let wait = async {
                                    while !*readiness.borrow_and_update() {
                                        if readiness.changed().await.is_err() {
                                            break;
                                        }
                                    }
                                };
                                let _ = tokio::time::timeout(timeout, wait).await;
                            }
                            let _ = done.send(Ok(()));
                        });
                    }
                    Some(CodingRuntimeControl::McpStatus {
                        generation: request_generation,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let servers = runtime.parts.mcp_statuses().await;
                        let _ = done.send(Ok(McpStatusSnapshot {
                            generation: RuntimeGeneration(generation),
                            servers,
                        }));
                    }
                    Some(CodingRuntimeControl::ToolCatalog {
                        generation: request_generation,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let listing = resources
                            .as_ref()
                            .and_then(|runtime| runtime.parts.tool_catalog())
                            .map(|catalog| catalog.listing());
                        // No catalog means no tree is mounted, which is not an
                        // empty catalog: saying "no tools" would be a lie a
                        // screen would render.
                        let _ = done.send(listing.ok_or(RuntimeError::Unavailable));
                    }
                    Some(CodingRuntimeControl::SwitchTool {
                        generation: request_generation,
                        pattern,
                        on,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let catalog = resources
                            .as_ref()
                            .and_then(|runtime| runtime.parts.tool_catalog());
                        let Some(catalog) = catalog else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        if on {
                            catalog.turn_on(&pattern);
                        } else {
                            catalog.turn_off(&pattern);
                        }
                        if let Some(runtime) = resources.as_ref() {
                            tell(runtime, Some(crate::told::tool_switched(&pattern, on)));
                        }
                        let _ = done.send(Ok(catalog.listing()));
                    }
                    Some(CodingRuntimeControl::McpTools {
                        generation: request_generation,
                        server,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let tools = runtime.parts.mcp_tools_for_server(&server);
                        let statuses = runtime.parts.mcp_statuses().await;
                        let status = statuses
                            .iter()
                            .find_map(|(name, status)| (*name == server).then(|| status.clone()));
                        let mut available: Vec<String> =
                            statuses.into_iter().map(|(name, _)| name).collect();
                        available.sort();
                        let _ = done.send(Ok(McpToolsSnapshot {
                            generation: RuntimeGeneration(generation),
                            server,
                            status,
                            tools,
                            available,
                        }));
                    }
                    Some(CodingRuntimeControl::ApproveMcpTool {
                        generation: request_generation,
                        alias,
                        done,
                    }) => {
                        // Generation only: this answers an approval the running
                        // turn is waiting on, so an active turn is the normal case.
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let Some(registry) = runtime.parts.mcp_registry.as_ref() else {
                            let _ = done.send(Ok(None));
                            continue;
                        };
                        let Some((server, tool)) = registry.split_tool_name(&alias).await else {
                            let _ = done.send(Ok(None));
                            continue;
                        };
                        // The session grant first: a project file that cannot be
                        // rewritten (a commented `.mcp.json` is refused) must not
                        // cost the person the answer they just gave.
                        registry.mark_tool_auto_approved(&alias);
                        let persist_error = atomcode_capabilities::mcp::config::add_auto_approved_tool(
                            &runtime.config.working_dir,
                            &server,
                            &tool,
                        )
                        .err()
                        .map(|error| format!("{error:#}"));
                        let _ = done.send(Ok(Some(McpToolApproval {
                            server,
                            tool,
                            persist_error,
                        })));
                    }
                    Some(CodingRuntimeControl::McpRows {
                        generation: request_generation,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let _ = done.send(match mcp_rows_of(runtime).await {
                            Ok(rows) => Ok(McpRowsSnapshot {
                                generation: RuntimeGeneration(generation),
                                rows,
                            }),
                            Err(message) => Err(RuntimeError::ReconfigureFailed(message)),
                        });
                    }
                    Some(CodingRuntimeControl::McpDetail {
                        generation: request_generation,
                        server,
                        done,
                    }) => {
                        if request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_ref() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let _ = done.send(match mcp_rows_of(runtime).await {
                            Ok(rows) => Ok(McpDetailSnapshot {
                                generation: RuntimeGeneration(generation),
                                detail: rows.into_iter().find(|row| row.name == server),
                            }),
                            Err(message) => Err(RuntimeError::ReconfigureFailed(message)),
                        });
                    }
                    Some(CodingRuntimeControl::McpAct {
                        generation: request_generation,
                        server,
                        action,
                        done,
                    }) => {
                        // Every action but `Disable` either withdraws the tools
                        // (untrust, sign out) — the security-reducing mutation
                        // `withdraw_mcp_tools` awaits an idle terminal for — or only
                        // reaches the session through the rebuild that follows it
                        // (trust, enable), which a running turn refuses. So
                        // they take the same refusal up front, rather than writing
                        // the change and then failing to apply it. `Disable` holds
                        // tools back in place and runs mid-turn (design §5.4).
                        if request_generation != generation
                            || (action.rebuilds()
                                && (compaction_suspended || active_turn.is_some()))
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_mut() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let said = crate::told::mcp_acted(&server, action);
                        let outcome =
                            apply_mcp_action(runtime, server, action, &mut mcp_disable_holds).await;
                        if outcome.is_ok() {
                            tell(runtime, Some(said));
                        }
                        let _ = done.send(match outcome {
                            Ok(()) => Ok(()),
                            Err(message) => Err(RuntimeError::ReconfigureFailed(message)),
                        });
                    }
                    Some(CodingRuntimeControl::WithdrawMcpTools {
                        generation: request_generation,
                        tell: told,
                        done,
                    }) => {
                        if request_generation != generation
                            || compaction_suspended
                            || active_turn.is_some()
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(runtime) = resources.as_mut() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        runtime.parts.withdraw_mcp_tools().await;
                        tell(runtime, told.then(crate::told::mcp_withdrawn));
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::QueueLocalContext {
                        generation: request_generation,
                        input,
                        done,
                    }) => {
                        if request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                        } else {
                            if !input.content.is_empty() {
                                pending_local_context.push(input.content);
                            }
                            let _ = done.send(Ok(()));
                        }
                    }
                    Some(CodingRuntimeControl::ReassembleProvider {
                        generation: request_generation,
                        mut next,
                        done,
                    }) => {
                        if request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(mut runtime) = resources.take() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        let routing = next
                            .subagent_config
                            .clone()
                            .or_else(|| runtime.config.subagent_config.clone());
                        next.subagent_config = routing.clone();
                        next.subagent_fast_provider = runtime.config.subagent_fast_provider.clone();
                        next.subagent_capable_provider =
                            runtime.config.subagent_capable_provider.clone();
                        next.subagent_model_providers =
                            runtime.config.subagent_model_providers.clone();
                        let refresh_routing = if let Some(config) = routing {
                            if next.subagent_fast_provider.is_none()
                                && next.subagent_capable_provider.is_none()
                                && next.subagent_model_providers.is_none()
                            {
                                crate::provider_factory::install_subagent_tiers(
                                    runtime.provider_factory.clone(),
                                    &mut next,
                                    config.as_ref(),
                                );
                                None
                            } else {
                                Some(config)
                            }
                        } else {
                            None
                        };
                        let session_id = runtime
                            .parts
                            .session
                            .as_ref()
                            .map(|binding| binding.id.as_str());
                        let candidate_provider = match runtime
                            .provider_factory
                            .build(&next, session_id)
                        {
                            Ok(provider) => provider,
                            Err(error) => {
                                resources = Some(runtime);
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                    error.to_string(),
                                )));
                                continue;
                            }
                        };

                        // On the harness, a `/model` switch is a patch, not a
                        // rebuild.
                        //
                        // Everything below this branch — stop the agent, verify
                        // its terminal, fail-close pending requests, reassemble,
                        // restore the snapshot — exists because the CHAIN has to
                        // replace one `AgentHandle` with another. Here the handle
                        // does not change: the provider lives behind the `llm`
                        // seam, `agent-loop` resolves that seam per turn, and
                        // `App::patch` remounts only the row whose config moved.
                        // Rows are sibling fibers under `ROOT_FIBER` and
                        // `Fibers::unload` cascades to children rather than to
                        // consumers, so `agent-loop` and `ui-handle` keep running
                        // across it.
                        //
                        // What is NOT skipped is the contract with whoever asked.
                        // A first attempt at this branch dropped all of it on the
                        // theory that it belonged to the rebuild; the runtime's
                        // own tests named every piece, one failure at a time:
                        // the generation is the RECEIPT `reassemble_provider`
                        // returns, `controls.state` is what `status()` reads, the
                        // driver renders four events in order, and a sessionless
                        // run still has a snapshot to keep.
                        // `agent.is_some()` is load-bearing. A patch replaces
                        // what is behind a seam; it cannot bring back an agent
                        // that was torn down, and `ui-handle` hands its handle
                        // out exactly once — so after a `DeactivateProvider`
                        // (what `/logout` does) there is nothing to patch
                        // underneath. Recovery from that has to rebuild, which
                        // is the chain path below.
                        //
                        // Without this guard the runtime reported Ready after a
                        // `/logout` → `/login` and then refused every turn with
                        // `ProviderUnavailable`: the provider had been swapped
                        // behind a seam nobody was reading.
                        if agent.is_some() {
                            if let (Some(app), Some(slots)) = (
                                runtime.harness_app.as_mut(),
                                runtime.harness_providers.clone(),
                            ) {
                                // What the patch interrupts is put back when it
                                // is done, under whichever generation is current
                                // then. A patch does not end a running turn, and
                                // a phase left at `Reconfiguring` carries the old
                                // generation: every cancel read from it was
                                // refused as stale while the turn ran on (`/model`
                                // mid-turn, then esc → "unavailable").
                                let resumed = if active_turn.is_none() {
                                    RuntimePhase::Ready
                                } else if pending_requests.is_empty() {
                                    RuntimePhase::InTurn
                                } else {
                                    RuntimePhase::WaitingApproval
                                };
                                controls.state.store(
                                    runtime_phase_state(
                                        generation,
                                        RuntimePhase::Reconfiguring,
                                    ),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfiguring {
                                        operation: ReconfigureKind::Provider,
                                    },
                                );
                                let side_provider = candidate_provider.clone();
                                if let Err(error) = crate::on_harness::swap_provider_for(
                                    app,
                                    slots.as_ref(),
                                    candidate_provider,
                                    &next.model,
                                    Some(&next),
                                )
                                .await
                                {
                                    // The tree is unchanged on a failed patch, so
                                    // the old provider is still behind the seam and
                                    // the session carries on with the model it had.
                                    controls.state.store(
                                        runtime_phase_state(generation, resumed),
                                        Ordering::Release,
                                    );
                                    resources = Some(runtime);
                                    let _ = done
                                        .send(Err(RuntimeError::ReconfigureFailed(error)));
                                    continue;
                                }
                                if let Some(config) = refresh_routing {
                                    crate::provider_factory::refresh_subagent_tiers(
                                        runtime.provider_factory.clone(),
                                        &next,
                                        config.as_ref(),
                                    );
                                }
                                // The reviewer and the subagents follow the model.
                                let _ = crate::parts::wire_side_providers(
                                    &runtime.parts,
                                    &next,
                                    &side_provider,
                                );
                                // Turns from here on are billed to the new model.
                                // The chain says the same thing from `assemble`,
                                // which only runs once the swap has succeeded.
                                if let Some(snapshot) = runtime.parts.snapshot_hook() {
                                    snapshot.set_model_attribution(&next.provider_name, &next.model);
                                }
                                tell(&runtime, crate::told::reconfigured(&runtime.config, &next));
                                runtime.config = next;
                                let provider = runtime.config.provider_name.clone();
                                let model = runtime.config.model.clone();
                                let reasoning_effort =
                                    runtime.config.chat_options.reasoning_effort;
                                let reasoning_effort_applicable =
                                    runtime.config.supports_reasoning_effort;
                                resources = Some(runtime);
                                // A provider is available again. Needed for the
                                // LOGIN half of `/logout` → `/login`: a plain
                                // `/model` never leaves these unset, so the first
                                // version of this branch did not touch them — and
                                // a recovered runtime then reported Ready while
                                // refusing every turn as `ProviderUnavailable`.
                                agent_available = true;
                                provider_unavailable_reason = None;
                                controls
                                    .provider_unavailable_reason
                                    .store(0, Ordering::Release);
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                pending_steer_acknowledgements.clear();
                                controls.state.store(
                                    runtime_phase_state(generation, resumed),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::ProviderChanged {
                                        provider: provider.clone(),
                                        model,
                                    },
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::ReasoningEffortChanged {
                                        provider,
                                        effort: reasoning_effort,
                                        applicable: reasoning_effort_applicable,
                                    },
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfigured {
                                        operation: ReconfigureKind::Provider,
                                    },
                                );
                                let _ = done.send(Ok(RuntimeGeneration(generation)));
                                continue;
                            }
                        }

                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }

                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation: ReconfigureKind::Provider,
                        });
                        let had_active_agent = agent_available;
                        fail_close_pending_requests(
                            &agent,
                            &mut pending_requests,
                            active_turn.is_some(),
                        );
                        let had_active_turn = active_turn.is_some();
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                            // Preserve detached Team runs across a `/model` reassemble:
                            // the session continues, so background team work must not die.
                            None,
                            runtime.parts.snapshot_persistence_status(),
                        )
                        .await;
                        if stop_report.forced
                            && had_active_turn
                            && !stop_report.has_verified_turn_terminal()
                        {
                            let message = "provider reconfiguration timed out while stopping the active agent; the latest conversation snapshot could not be verified".to_string();
                            fail_close_after_forced_provider_stop(
                                &message,
                                Some(&runtime),
                                &mut goal,
                                &mut loop_state,
                                &mut pending_wakeup,
                                &mut held_turn,
                                &mut active_turn,
                                &mut terminal_reason,
                                &mut turn_stats,
                                &mut conversation_revision,
                                &mut snapshot_waiters,
                                &mut agent_available,
                                controls.state.as_ref(),
                                generation,
                                &runtime_event_tx,
                            );
                            resources = Some(runtime);
                            let _ = done.send(Err(RuntimeError::ReconfigureFailed(message)));
                            continue;
                        }
                        finish_stopped_native_turn(
                            &stop_report,
                            Some(&runtime),
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &runtime_event_tx,
                        );
                        if let Some(error) = fail_close_after_stopped_persistence(
                            &stop_report,
                            Some(&runtime),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        ) {
                            persistence_failure = stop_report.persistence_failure.clone();
                            resources = Some(runtime);
                            let _ = done.send(Err(error));
                            continue;
                        }
                        preserve_sessionless_snapshot(&mut runtime, &stop_report);

                        let old_config = runtime.config.clone();
                        match build_agent(&mut runtime, &next, candidate_provider).await {
                            Ok(candidate) => {
                                if let Some(config) = refresh_routing {
                                    crate::provider_factory::refresh_subagent_tiers(
                                        runtime.provider_factory.clone(),
                                        &next,
                                        config.as_ref(),
                                    );
                                }
                                runtime.config = next;
                                agent = Some(candidate);
                                // Before the pending prompt is replayed, so the
                                // note precedes the first message on the new model.
                                tell(&runtime, crate::told::reconfigured(&old_config, &runtime.config));
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                pending_steer_acknowledgements.clear();
                                // Do NOT begin a new team generation here: a `/model`
                                // reassemble keeps the session, so in-flight team runs
                                // (and their events) must survive rather than be cancelled.
                                agent_available = true;
                                provider_unavailable_reason = None;
                                controls.provider_unavailable_reason.store(0, Ordering::Release);
                                // Reassembly restores the same conversation snapshot. Keep the
                                // last observed usage until the replacement provider reports a
                                // fresh value, otherwise callers briefly see an empty context and
                                // may mistake a model switch for conversation loss.
                                snapshot_in_flight = false;
                                cancel_pending = false;
                                compaction_suspended = false;
                                let provider = runtime.config.provider_name.clone();
                                let model = runtime.config.model.clone();
                                let reasoning_effort = runtime.config.chat_options.reasoning_effort;
                                let reasoning_effort_applicable =
                                    runtime.config.supports_reasoning_effort;
                                replay_pending_resume_prompt(
                                    &agent,
                                    Some(&mut runtime),
                                    controls.state.as_ref(),
                                    generation,
                                    &mut next_turn_id,
                                    &mut active_turn,
                                    &mut turn_stats,
                                    &mut agent_available,
                                );
                                resources = Some(runtime);
                                if active_turn.is_none() {
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Ready),
                                        Ordering::Release,
                                    );
                                }
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::ProviderChanged {
                                        provider: provider.clone(),
                                        model,
                                    },
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::ReasoningEffortChanged {
                                        provider,
                                        effort: reasoning_effort,
                                        applicable: reasoning_effort_applicable,
                                    },
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfigured {
                                        operation: ReconfigureKind::Provider,
                                    },
                                );
                                let _ = done.send(Ok(RuntimeGeneration(generation)));
                            }
                            Err(candidate_error) => {
                                runtime.config = old_config;
                                let rollback = if had_active_agent {
                                    Some(assemble_runtime_resources(&mut runtime).await)
                                } else {
                                    None
                                };
                                match rollback.transpose() {
                                    Ok(None) => {
                                        agent = None;
                                        agent_available = false;
                                        provider_unavailable_reason = None;
                                        controls
                                            .provider_unavailable_reason
                                            .store(0, Ordering::Release);
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Failed),
                                            Ordering::Release,
                                        );
                                    }
                                    Ok(Some(rollback)) => {
                                        agent = Some(rollback);
                                        agent_available = true;
                                        provider_unavailable_reason = None;
                                        controls
                                            .provider_unavailable_reason
                                            .store(0, Ordering::Release);
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Ready),
                                            Ordering::Release,
                                        );
                                    }
                                    Err(rollback_error) => {
                                        agent = None;
                                        agent_available = false;
                                        provider_unavailable_reason = None;
                                        controls
                                            .provider_unavailable_reason
                                            .store(0, Ordering::Release);
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Failed),
                                            Ordering::Release,
                                        );
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::Agent(AgentEvent::Error {
                                                message: format!(
                                                    "provider reconfigure rollback failed: {rollback_error}"
                                                ),
                                                http_status: None,
                                                code: None,
                                                retryable: None,
                                            }),
                                        );
                                    }
                                }
                                resources = Some(runtime);
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                    candidate_error.to_string(),
                                )));
                            }
                        }
                    }
                    Some(CodingRuntimeControl::DeactivateProvider {
                        generation: request_generation,
                        reason,
                        done,
                    }) => {
                        if request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        if !agent_available && provider_unavailable_reason == Some(reason) {
                            if let Some(intervention) = pending_policy_intervention.take() {
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::PolicyInterventionCleared {
                                        intervention_id: intervention.id,
                                    },
                                );
                            }
                            let _ = done.send(Ok(RuntimeGeneration(generation)));
                            continue;
                        }

                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }

                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation: ReconfigureKind::Provider,
                        });
                        cancel_controllers_and_finish_held(
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            resources
                                .as_ref()
                                .map(|runtime| runtime.loop_active.as_ref()),
                            controls.state.as_ref(),
                            generation,
                            RuntimePhase::Reconfiguring,
                            &runtime_event_tx,
                            "provider deactivated",
                        );
                        fail_close_pending_requests(
                            &agent,
                            &mut pending_requests,
                            active_turn.is_some(),
                        );
                        // On the harness, a logout takes the CREDENTIALS out of
                        // the tree and leaves the agent where it is.
                        //
                        // The chain has to stop the agent because the provider is
                        // baked into the assembled chain; here it lives behind a
                        // seam, so revoking it is swapping what is behind that
                        // seam for something holding nothing. The session, its
                        // conversation and its handle all survive, which is what
                        // lets the LOGIN afterwards be another swap rather than a
                        // rebuild — and therefore what keeps a `/logout` →
                        // `/login` from quietly moving the session back onto the
                        // chain for the rest of its life.
                        let harness_logout = agent.is_some()
                            && resources
                                .as_ref()
                                .is_some_and(|r| r.harness_providers.is_some());
                        if harness_logout {
                            let stop_report = {
                                let live = agent.as_mut().expect("checked above");
                                quiesce_current_agent(
                                    live,
                                    &mut compactions,
                                    &mut observed_tokens,
                                    &runtime_event_tx,
                                    CompactionInterruption::RuntimeReconfigured,
                                    resources
                                        .as_ref()
                                        .map(|runtime| &runtime.parts.team_manager),
                                    resources.as_ref().and_then(|runtime| {
                                        runtime.parts.snapshot_persistence_status()
                                    }),
                                )
                                .await
                            };
                            if let Some(runtime) = resources.as_mut() {
                                if let (Some(app), Some(slots)) = (
                                    runtime.harness_app.as_mut(),
                                    runtime.harness_providers.clone(),
                                ) {
                                    if let Err(error) =
                                        crate::on_harness::deactivate_provider(app, slots.as_ref())
                                            .await
                                    {
                                        // The tree is unchanged on a failed patch,
                                        // so the credentials are still in it. Say
                                        // so rather than reporting a logout that
                                        // did not happen.
                                        //
                                        // The TURN, though, is already over: the
                                        // quiesce above cancelled it and consumed
                                        // its `TurnComplete` into `stop_report`,
                                        // so nothing else will ever end it. Ending
                                        // it here is not bookkeeping — leaving
                                        // `active_turn` set while announcing
                                        // `Ready` makes the next `Submit` a STEER
                                        // (see the `SubmitReceipt::Steered` branch)
                                        // into a turn that no longer exists, and
                                        // the person's message goes nowhere with a
                                        // spinner that never stops.
                                        finish_stopped_native_turn(
                                            &stop_report,
                                            resources.as_ref(),
                                            &mut active_turn,
                                            &mut terminal_reason,
                                            &mut turn_stats,
                                            &mut conversation_revision,
                                            &mut snapshot_waiters,
                                            &runtime_event_tx,
                                        );
                                        controls.state.store(
                                            runtime_phase_state(
                                                generation,
                                                RuntimePhase::Ready,
                                            ),
                                            Ordering::Release,
                                        );
                                        let _ = done
                                            .send(Err(RuntimeError::ReconfigureFailed(error)));
                                        continue;
                                    }
                                }
                                // The reviewer's and the subagents' slots held the
                                // same credentials; they leave with the seam's.
                                let _ = crate::parts::wire_side_providers(
                                    &runtime.parts,
                                    &runtime.config,
                                    &crate::on_harness::signed_out_provider(),
                                );
                                preserve_sessionless_snapshot(runtime, &stop_report);
                                if let Some(provider) =
                                    runtime.config.subagent_fast_provider.as_ref()
                                {
                                    provider.reset(Arc::new(|| None));
                                }
                                if let Some(provider) =
                                    runtime.config.subagent_capable_provider.as_ref()
                                {
                                    provider.reset(Arc::new(|| None));
                                }
                            }
                            finish_stopped_native_turn(
                                &stop_report,
                                resources.as_ref(),
                                &mut active_turn,
                                &mut terminal_reason,
                                &mut turn_stats,
                                &mut conversation_revision,
                                &mut snapshot_waiters,
                                &runtime_event_tx,
                            );
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
                            pending_steer_acknowledgements.clear();
                            agent_available = false;
                            provider_unavailable_reason = Some(reason);
                            controls.provider_unavailable_reason.store(
                                encode_provider_unavailable_reason(Some(reason)),
                                Ordering::Release,
                            );
                            observed_tokens = None;
                            snapshot_in_flight = false;
                            controls.state.store(
                                runtime_phase_state(
                                    generation,
                                    RuntimePhase::AwaitingProvider,
                                ),
                                Ordering::Release,
                            );
                            if let Some(intervention) = pending_policy_intervention.take() {
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::PolicyInterventionCleared {
                                        intervention_id: intervention.id,
                                    },
                                );
                            }
                            // `ProviderUnavailable`, not `Reconfigured`: a logout
                            // is not a reconfiguration that landed, it is a
                            // capability going away, and the driver renders the
                            // two differently. `forced` is false because nothing
                            // was forced — the agent was asked to stop its turn
                            // and it is still there.
                            let _ = runtime_event_tx.send(
                                CodingRuntimeEvent::ProviderUnavailable {
                                    reason,
                                    forced: stop_report.forced,
                                },
                            );
                            let _ = done.send(Ok(RuntimeGeneration(generation)));
                            continue;
                        }
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                            resources.as_ref().map(|runtime| &runtime.parts.team_manager),
                            resources
                                .as_ref()
                                .and_then(|runtime| runtime.parts.snapshot_persistence_status()),
                        )
                        .await;
                        finish_stopped_native_turn(
                            &stop_report,
                            resources.as_ref(),
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &runtime_event_tx,
                        );
                        if let Some(error) = fail_close_after_stopped_persistence(
                            &stop_report,
                            resources.as_ref(),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        ) {
                            persistence_failure = stop_report.persistence_failure.clone();
                            let _ = done.send(Err(error));
                            continue;
                        }
                        if let Some(runtime) = resources.as_mut() {
                            preserve_sessionless_snapshot(runtime, &stop_report);
                            // The reviewer's and the subagents' slots live on
                            // `parts` and survive a rebuild on purpose, so losing
                            // the agent does NOT empty them. A logout taken here
                            // — which is the state expired credentials leave a
                            // person in, and therefore the common one — has to
                            // take the credentials out of them by hand, exactly
                            // as the branch with a live agent does.
                            let _ = crate::parts::wire_side_providers(
                                &runtime.parts,
                                &runtime.config,
                                &crate::on_harness::signed_out_provider(),
                            );
                            // A tree can outlive the agent: an assemble that fails
                            // after `mount` leaves the old one in `harness_app`,
                            // and its `llm` row is still holding the provider it
                            // captured. Nothing will drive it again, but "nothing
                            // drives it" is not "the credentials are gone".
                            if let (Some(app), Some(slots)) = (
                                runtime.harness_app.as_mut(),
                                runtime.harness_providers.clone(),
                            ) {
                                let _ = crate::on_harness::deactivate_provider(
                                    app,
                                    slots.as_ref(),
                                )
                                .await;
                            }
                            if let Some(provider) = runtime.config.subagent_fast_provider.as_ref() {
                                provider.reset(Arc::new(|| None));
                            }
                            if let Some(provider) =
                                runtime.config.subagent_capable_provider.as_ref()
                            {
                                provider.reset(Arc::new(|| None));
                            }
                        }
                        generation = generation.wrapping_add(1);
                        event_generation.store(generation, Ordering::Release);
                        pending_steer_acknowledgements.clear();
                        agent_available = false;
                        provider_unavailable_reason = Some(reason);
                        controls.provider_unavailable_reason.store(
                            encode_provider_unavailable_reason(Some(reason)),
                            Ordering::Release,
                        );
                        observed_tokens = None;
                        snapshot_in_flight = false;
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::AwaitingProvider),
                            Ordering::Release,
                        );
                        if let Some(intervention) = pending_policy_intervention.take() {
                            let _ = runtime_event_tx.send(
                                CodingRuntimeEvent::PolicyInterventionCleared {
                                    intervention_id: intervention.id,
                                },
                            );
                        }
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::ProviderUnavailable {
                            reason,
                            forced: stop_report.forced,
                        });
                        let _ = done.send(Ok(RuntimeGeneration(generation)));
                    }
                    Some(CodingRuntimeControl::Reprepare {
                        generation: request_generation,
                        target,
                        done,
                    }) => {
                        if request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(mut runtime) = resources.take() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        // What the model is told once this lands: a reload, or
                        // what a reloaded config changed. A new session is not
                        // told anything — it has no conversation above it.
                        let reloaded_from = match &target {
                            ReprepareTarget::Reload { .. } => Some(None),
                            ReprepareTarget::ReloadConfig(_) => Some(Some(runtime.config.clone())),
                            _ => None,
                        };
                        // A reload with nothing to reconnect is a re-read, not
                        // a rebuild (`docs/adr/0022` §2): the skills on disk go
                        // into the registry the live tree already serves, and
                        // the catalog the model is told about is re-contributed
                        // under the same id. Reconnecting is what a rebuild is
                        // for, so a session with MCP servers still takes that
                        // route — and so does one mid-turn, which has a request
                        // in flight against the prompt this would change.
                        if matches!(
                            &target,
                            ReprepareTarget::Reload {
                                plugin_skill_dirs: None
                            }
                        ) && active_turn.is_none()
                            && !compactions.is_active()
                            && live_root_agent(&runtime).is_some()
                            && runtime.parts.mcp_statuses().await.is_empty()
                        {
                            if reload_skills_live(&runtime).is_ok() {
                                tell(&runtime, Some(crate::told::reloaded()));
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfiguring {
                                        operation: ReconfigureKind::Reprepare,
                                    },
                                );
                                let unchanged = session_changed(generation, &runtime);
                                resources = Some(runtime);
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfigured {
                                        operation: ReconfigureKind::Reprepare,
                                    },
                                );
                                let _ = done.send(Ok(unchanged));
                                continue;
                            }
                        }
                        let withdraws_mcp = matches!(
                            &target,
                            ReprepareTarget::Reload { .. } | ReprepareTarget::ReloadConfig(_)
                        );
                        let resolved = match resolve_reprepare_input(&runtime, target) {
                            Ok(input) => input,
                            Err(error) => {
                                resources = Some(runtime);
                                let _ = done.send(Err(error));
                                continue;
                            }
                        };
                        // A same-directory ChangeDirectory resolves to no input: the current
                        // runtime remains authoritative, with no candidate session, generation
                        // advance, or reconfiguration events.
                        let Some((input, prepared_lease, reprepare_cancel)) = resolved else {
                            let unchanged = session_changed(generation, &runtime);
                            resources = Some(runtime);
                            let _ = done.send(Ok(unchanged));
                            continue;
                        };
                        let operation = input.operation;
                        let reuses_current_session = runtime
                            .parts
                            .session
                            .as_ref()
                            .zip(match &input.prepare.session {
                                crate::SessionMode::Resume(id)
                                | crate::SessionMode::ExternalSnapshot { id, .. } => Some(id),
                                crate::SessionMode::Fresh | crate::SessionMode::Disabled => None,
                            })
                            .is_some_and(|(current, target)| current.id == *target);
                        let changes_session = matches!(
                            operation,
                            ReconfigureKind::FreshSession
                                | ReconfigureKind::ResumeSession
                                | ReconfigureKind::ChangeDirectory
                        );
                        if active_turn.is_some() && (reuses_current_session || changes_session) {
                            resources = Some(runtime);
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }
                        if withdraws_mcp {
                            // Config, trust, and auth are mutable security inputs.
                            // Remove the old scope before reading them so a failed
                            // replacement cannot leave revoked MCP authority mounted.
                            runtime.parts.withdraw_mcp_tools().await;
                        }
                        let previous_phase = runtime_status(
                            controls.state.load(Ordering::Acquire),
                        )
                        .phase;
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation,
                        });

                        // Preflight the complete capability graph while the current agent is
                        // still alive. A prepare failure must leave the old runtime untouched.
                        let reuse_lease = prepared_lease.or_else(|| {
                            matching_session_lease(&runtime.parts, &input.prepare.session)
                        });
                        let prepare_candidate = prepare_with_plugin_hook_source_reusing_lease(
                            &input.config,
                            input.prepare.clone(),
                            runtime.plugin_hooks.as_ref(),
                            reuse_lease,
                            true,
                        );
                        let candidate_parts = match reprepare_cancel.as_ref() {
                            Some(cancel) => {
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => Err(RuntimeError::Cancelled),
                                    result = tokio::time::timeout(
                                        std::time::Duration::from_secs(30),
                                        prepare_candidate,
                                    ) => match result {
                                        Ok(result) => result.map_err(runtime_prepare_error),
                                        Err(_) => Err(RuntimeError::ReconfigureFailed(
                                            "session resume preflight timed out after 30 seconds"
                                                .to_string(),
                                        )),
                                    },
                                }
                            }
                            None => prepare_candidate.await.map_err(runtime_prepare_error),
                        };
                        let mut candidate = match candidate_parts {
                            Ok(mut parts) => {
                                // The person's tool switches are theirs, not this
                                // tree's: a reprepare must not put back what they
                                // turned off (`CodingParts::tool_switches`).
                                parts.adopt_tool_switches(runtime.parts.tool_switches());
                                RuntimeResources {
                                config: input.config,
                                prepare: input.prepare,
                                provider_factory: runtime.provider_factory.clone(),
                                plugin_hooks: runtime.plugin_hooks.clone(),
                                parts,
                                // Filled by `assemble_runtime_resources` below,
                                // on whichever engine this runtime runs.
                                harness_providers: None,
                                harness_app: None,
                                wakeup_tx: runtime.wakeup_tx.clone(),
                                loop_active: Arc::clone(&runtime.loop_active),
                                // Preserve the injected VL hook across reprepare
                                // (/model swap, reconfigure).
                                image_preprocessor: runtime.image_preprocessor.clone(),
                                }
                            }
                            Err(error) => {
                                controls.state.store(
                                    runtime_phase_state(generation, previous_phase),
                                    Ordering::Release,
                                );
                                resources = Some(runtime);
                                let _ = done.send(Err(error));
                                continue;
                            }
                        };

                        if operation == ReconfigureKind::Reprepare {
                            candidate.parts.inherit_runtime_continuity(&runtime.parts);
                        } else {
                            candidate.parts.plan_mode.store(
                                runtime.parts.plan_mode.load(Ordering::Acquire),
                                Ordering::Release,
                            );
                            candidate.parts.bypass_mode.store(
                                runtime.parts.bypass_mode.load(Ordering::Acquire),
                                Ordering::Release,
                            );
                            candidate.parts.accept_edits.store(
                                runtime.parts.accept_edits.load(Ordering::Acquire),
                                Ordering::Release,
                            );
                        }

                        // Complete every fallible candidate build step before disturbing the
                        // current agent. A failed fresh/resume/cd transition must leave the
                        // previous runtime executable; rebuilding it as a rollback can fail for
                        // reasons (notably authentication) unrelated to the accepted operation.
                        let replacement = match assemble_runtime_resources(&mut candidate).await {
                            Ok(replacement) => replacement,
                            Err(candidate_error) => {
                                let cleanup_error =
                                    discard_uncommitted_session(operation, &candidate).err();
                                controls.state.store(
                                    runtime_phase_state(generation, previous_phase),
                                    Ordering::Release,
                                );
                                resources = Some(runtime);
                                let candidate_error = match cleanup_error {
                                    Some(cleanup_error) => {
                                        format!("{candidate_error}; {cleanup_error}")
                                    }
                                    None => candidate_error,
                                };
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                    candidate_error,
                                )));
                                continue;
                            }
                        };

                        // Persistence is the transition's irrevocable commit point. Publish
                        // only after the complete replacement has assembled, while the old
                        // agent is still executable if this final fallible write fails.
                        if let Err(publish_error) = candidate.parts.publish_staged_session() {
                            let cleanup_error =
                                discard_uncommitted_session(operation, &candidate).err();
                            controls.state.store(
                                runtime_phase_state(generation, previous_phase),
                                Ordering::Release,
                            );
                            resources = Some(runtime);
                            let publish_error = match cleanup_error {
                                Some(cleanup_error) => {
                                    format!("{publish_error}; {cleanup_error}")
                                }
                                None => publish_error.to_string(),
                            };
                            let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                publish_error,
                            )));
                            continue;
                        }

                        let controller_interrupted = cancel_controllers_and_finish_held(
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            Some(runtime.loop_active.as_ref()),
                            controls.state.as_ref(),
                            generation,
                            RuntimePhase::Reconfiguring,
                            &runtime_event_tx,
                            "runtime reconfigured",
                        );
                        fail_close_pending_requests(
                            &agent,
                            &mut pending_requests,
                            active_turn.is_some(),
                        );
                        let mut stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                            // Reprepare REBUILDS the runtime (`runtime = candidate` below), so
                            // the team_manager itself is replaced; terminate its in-flight runs
                            // cleanly here before the old manager is dropped.
                            Some(&runtime.parts.team_manager),
                            runtime.parts.snapshot_persistence_status(),
                        )
                        .await;
                        if controller_interrupted {
                            stop_report.reason = Some(StopReason::Cancelled);
                        }
                        finish_stopped_native_turn(
                            &stop_report,
                            Some(&runtime),
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &runtime_event_tx,
                        );
                        if let Some(error) = fail_close_after_stopped_persistence(
                            &stop_report,
                            Some(&runtime),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        ) {
                            persistence_failure = stop_report.persistence_failure.clone();
                            let _ = replacement.commands.send(AgentCommand::Shutdown);
                            let cleanup_error =
                                discard_uncommitted_session(operation, &candidate).err();
                            resources = Some(runtime);
                            let error = match cleanup_error {
                                Some(cleanup_error) => RuntimeError::ReconfigureFailed(format!(
                                    "{error}; candidate cleanup failed: {cleanup_error}"
                                )),
                                None => error,
                            };
                            let _ = done.send(Err(error));
                            continue;
                        }
                        preserve_sessionless_snapshot(&mut runtime, &stop_report);
                        runtime = candidate;
                        agent = Some(replacement);
                        generation = generation.wrapping_add(1);
                        event_generation.store(generation, Ordering::Release);
                        pending_steer_acknowledgements.clear();
                        runtime
                            .parts
                            .team_manager
                            .set_event_sender(team_event_tx.clone());
                        runtime.parts.team_manager.begin_generation(generation);
                        agent_available = true;
                        // The rebuild is what a reason stands for: whatever made
                        // the provider unavailable (NotConfigured included — an
                        // onboarding login reloads with a configuration that now
                        // names one) was addressed by building this one, so a
                        // readiness read after the reprepare must say so.
                        provider_unavailable_reason = None;
                        controls
                            .provider_unavailable_reason
                            .store(0, Ordering::Release);
                        observed_tokens = None;
                        snapshot_in_flight = false;
                        compaction_suspended = false;
                        match reloaded_from {
                            Some(None) => tell(&runtime, Some(crate::told::reloaded())),
                            Some(Some(before)) => {
                                tell(&runtime, crate::told::reconfigured(&before, &runtime.config))
                            }
                            None => {}
                        }
                        let changed = session_changed(generation, &runtime);
                        let cwd = runtime.config.working_dir.clone();
                        resources = Some(runtime);
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Ready),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx
                            .send(CodingRuntimeEvent::SessionChanged(changed.clone()));
                        if changes_session {
                            if let Some(intervention) = pending_policy_intervention.take() {
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::PolicyInterventionCleared {
                                        intervention_id: intervention.id,
                                    },
                                );
                            }
                        }
                        if operation == ReconfigureKind::ChangeDirectory {
                            let _ = runtime_event_tx
                                .send(CodingRuntimeEvent::WorkingDirectoryChanged(cwd));
                        }
                        let _ = runtime_event_tx
                            .send(CodingRuntimeEvent::Reconfigured { operation });
                        let _ = done.send(Ok(changed));
                    }
                    Some(CodingRuntimeControl::ApplyUndo {
                        generation: request_generation,
                        expected_revision,
                        code_rewound_to,
                        original,
                        truncated,
                        restored_prompt,
                        target_n,
                        prompts_before,
                        done,
                    }) => {
                        if request_generation != generation
                            || expected_revision != conversation_revision
                            || compaction_suspended
                            || compactions.is_active()
                            || active_turn.is_some()
                        {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(mut runtime) = resources.take() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }
                        // With the agent live, the undo is facts in its log and
                        // nothing is rebuilt (`docs/adr/0022` §2) — as long as
                        // the log can say the change; see `live_can_say`.
                        let live = live_root_agent(&runtime)
                            .filter(|live| live_can_say(live, &truncated.messages));
                        let undo_sidecars = match persist_runtime_undo(
                            &mut runtime,
                            Some(original.as_ref()),
                            &truncated,
                            live.as_ref(),
                        ) {
                            Ok(sidecars) => sidecars,
                            Err(error) => {
                                if error.is_snapshot_conflict() {
                                    resources = Some(runtime);
                                    let _ = done.send(Err(RuntimeError::Busy));
                                    continue;
                                }
                                let health_error = native_session_health_error(&runtime).await;
                                if error.requires_fail_close() || health_error.is_some() {
                                    let detail = health_error
                                        .as_deref()
                                        .map(|health| {
                                            format!(
                                                "; canonical native session health check failed: {health}"
                                            )
                                        })
                                        .unwrap_or_default();
                                    persistence_failure = Some(format!("{error}{detail}"));
                                    let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                                    agent = None;
                                    agent_available = false;
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Failed),
                                        Ordering::Release,
                                    );
                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                        AgentEvent::Error {
                                            message: format!(
                                                "undo persistence could not be proven safe; runtime stopped: {error}{detail}"
                                            ),
                                            http_status: None,
                                            code: None,
                                            retryable: None,
                                        },
                                    ));
                                }
                                resources = Some(runtime);
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                    error.to_string(),
                                )));
                                continue;
                            }
                        };
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation: ReconfigureKind::Undo,
                        });
                        if let Some(agent) = live.as_ref() {
                            let _ = undo_sidecars;
                            if let Some(turn) = code_rewound_to {
                                record_code_rewind(agent, turn);
                            } else {
                                // Rides with the next message rather than being
                                // committed now: the snapshot handed back below is
                                // the conversation as it now stands, and a note
                                // committed after it would make it stale at once.
                                agent.inject(
                                    crate::told::conversation_rewound_code_kept(),
                                    atomcode_harness::session::InjectionOrigin::Reminder,
                                );
                            }
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
                            pending_steer_acknowledgements.clear();
                            runtime.parts.team_manager.begin_generation(generation);
                            observed_tokens = None;
                            conversation_revision = conversation_revision.wrapping_add(1);
                            let snapshot = Arc::new(truncated);
                            resources = Some(runtime);
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Ready),
                                Ordering::Release,
                            );
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfigured {
                                operation: ReconfigureKind::Undo,
                            });
                            let _ = done.send(Ok(UndoResult {
                                generation: RuntimeGeneration(generation),
                                snapshot,
                                restored_prompt,
                                target_n,
                                prompts_before,
                            }));
                            continue;
                        }
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                            Some(&runtime.parts.team_manager),
                            runtime.parts.snapshot_persistence_status(),
                        )
                        .await;
                        if let Some(error) = fail_close_after_stopped_persistence(
                            &stop_report,
                            Some(&runtime),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        ) {
                            persistence_failure = stop_report.persistence_failure.clone();
                            resources = Some(runtime);
                            let _ = done.send(Err(error));
                            continue;
                        }
                        match assemble_runtime_resources(&mut runtime).await {
                            Ok(replacement) => {
                                agent = Some(replacement);
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                pending_steer_acknowledgements.clear();
                                runtime.parts.team_manager.begin_generation(generation);
                                agent_available = true;
                                observed_tokens = None;
                                snapshot_in_flight = false;
                                let snapshot = Arc::new(truncated);
                                // With the next message, as on the live branch.
                                if let Some(agent) = code_rewound_to
                                    .is_none()
                                    .then(|| live_root_agent(&runtime))
                                    .flatten()
                                {
                                    agent.inject(
                                        crate::told::conversation_rewound_code_kept(),
                                        atomcode_harness::session::InjectionOrigin::Reminder,
                                    );
                                }
                                resources = Some(runtime);
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Ready),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfigured {
                                        operation: ReconfigureKind::Undo,
                                    },
                                );
                                let _ = done.send(Ok(UndoResult {
                                    generation: RuntimeGeneration(generation),
                                    snapshot,
                                    restored_prompt,
                                    target_n,
                                    prompts_before,
                                }));
                            }
                            Err(candidate_error) => {
                                let restore_error = restore_runtime_undo(
                                    &mut runtime,
                                    &truncated,
                                    original.as_ref(),
                                    undo_sidecars,
                                )
                                .err();
                                if let Some(error) = restore_error.as_ref() {
                                    persistence_failure = Some(format!(
                                        "undo rollback persistence failed: {error}"
                                    ));
                                    agent = None;
                                    agent_available = false;
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Failed),
                                        Ordering::Release,
                                    );
                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                        AgentEvent::Error {
                                            message: format!(
                                                "undo rollback persistence failed; runtime stopped: {error}"
                                            ),
                                            http_status: None,
                                            code: None,
                                            retryable: None,
                                        },
                                    ));
                                } else {
                                    match assemble_runtime_resources(&mut runtime).await {
                                        Ok(rollback) => {
                                            agent = Some(rollback);
                                            agent_available = true;
                                            controls.state.store(
                                                runtime_phase_state(
                                                    generation,
                                                    RuntimePhase::Ready,
                                                ),
                                                Ordering::Release,
                                            );
                                        }
                                        Err(rollback_error) => {
                                            agent = None;
                                            agent_available = false;
                                            controls.state.store(
                                                runtime_phase_state(
                                                    generation,
                                                    RuntimePhase::Failed,
                                                ),
                                                Ordering::Release,
                                            );
                                            let _ = runtime_event_tx.send(
                                                CodingRuntimeEvent::Agent(AgentEvent::Error {
                                                    message: format!(
                                                        "undo rollback failed: {rollback_error}"
                                                    ),
                                                    http_status: None,
                                                    code: None,
                                                    retryable: None,
                                                }),
                                            );
                                        }
                                    }
                                }
                                resources = Some(runtime);
                                let detail = restore_error
                                    .map(|error| format!("; snapshot restore failed: {error}"))
                                    .unwrap_or_default();
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(format!(
                                    "{candidate_error}{detail}"
                                ))));
                            }
                        }
                    }
                    Some(CodingRuntimeControl::RestoreSnapshot {
                        generation: request_generation,
                        snapshot,
                        done,
                    }) => {
                        if request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(mut runtime) = resources.take() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation: ReconfigureKind::RestoreSession,
                        });
                        // Between turns, with the agent live, a restore is facts
                        // in its log like an undo, and nothing is rebuilt.
                        let live = (active_turn.is_none() && !compactions.is_active())
                            .then(|| live_root_agent(&runtime))
                            .flatten()
                            .filter(|live| live_can_say(live, &snapshot.messages));
                        if let Some(live) = live {
                            let original = current_runtime_snapshot(&runtime)
                                .or_else(|| runtime.parts.runtime_resume_snapshot());
                            match persist_runtime_undo(
                                &mut runtime,
                                original.as_ref(),
                                &snapshot,
                                Some(&live),
                            ) {
                                Ok(_) => {
                                    generation = generation.wrapping_add(1);
                                    event_generation.store(generation, Ordering::Release);
                                    pending_steer_acknowledgements.clear();
                                    runtime.parts.team_manager.begin_generation(generation);
                                    observed_tokens = None;
                                    conversation_revision = conversation_revision.wrapping_add(1);
                                    let changed = session_changed(generation, &runtime);
                                    resources = Some(runtime);
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Ready),
                                        Ordering::Release,
                                    );
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::SessionChanged(changed.clone()),
                                    );
                                    if let Some(intervention) = pending_policy_intervention.take()
                                    {
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::PolicyInterventionCleared {
                                                intervention_id: intervention.id,
                                            },
                                        );
                                    }
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::Reconfigured {
                                            operation: ReconfigureKind::RestoreSession,
                                        },
                                    );
                                    let _ = done.send(Ok(changed));
                                }
                                Err(error) => {
                                    if error.requires_fail_close() {
                                        persistence_failure = Some(error.to_string());
                                        let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                                        agent = None;
                                        agent_available = false;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Failed),
                                            Ordering::Release,
                                        );
                                    } else {
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Ready),
                                            Ordering::Release,
                                        );
                                    }
                                    resources = Some(runtime);
                                    let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                        error.to_string(),
                                    )));
                                }
                            }
                            continue;
                        }
                        fail_close_pending_requests(
                            &agent,
                            &mut pending_requests,
                            active_turn.is_some(),
                        );
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                            Some(&runtime.parts.team_manager),
                            runtime.parts.snapshot_persistence_status(),
                        )
                        .await;
                        finish_stopped_native_turn(
                            &stop_report,
                            Some(&runtime),
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &runtime_event_tx,
                        );
                        if let Some(error) = fail_close_after_stopped_persistence(
                            &stop_report,
                            Some(&runtime),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        ) {
                            persistence_failure = stop_report.persistence_failure.clone();
                            resources = Some(runtime);
                            let _ = done.send(Err(error));
                            continue;
                        }
                        preserve_sessionless_snapshot(&mut runtime, &stop_report);
                        let original = current_runtime_snapshot(&runtime)
                            .or_else(|| stop_report.snapshot.clone())
                            .or_else(|| runtime.parts.runtime_resume_snapshot());
                        let persisted = persist_runtime_undo(
                            &mut runtime,
                            original.as_ref(),
                            &snapshot,
                            None,
                        );
                        let candidate = match persisted.as_ref() {
                            Ok(_) => assemble_runtime_resources(&mut runtime)
                                .await
                                .map_err(NativePersistenceError::certain),
                            Err(error) => Err(error.clone()),
                        };
                        match candidate {
                            Ok(replacement) => {
                                agent = Some(replacement);
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                pending_steer_acknowledgements.clear();
                                runtime.parts.team_manager.begin_generation(generation);
                                agent_available = true;
                                observed_tokens = None;
                                snapshot_in_flight = false;
                                let changed = session_changed(generation, &runtime);
                                resources = Some(runtime);
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Ready),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::SessionChanged(changed.clone()),
                                );
                                if let Some(intervention) = pending_policy_intervention.take() {
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::PolicyInterventionCleared {
                                            intervention_id: intervention.id,
                                        },
                                    );
                                }
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfigured {
                                        operation: ReconfigureKind::RestoreSession,
                                    },
                                );
                                let _ = done.send(Ok(changed));
                            }
                            Err(candidate_error) => {
                                let persistence_succeeded = persisted.is_ok();
                                let restore_error = if persistence_succeeded {
                                    original.as_ref().and_then(|original| {
                                        restore_runtime_undo(
                                            &mut runtime,
                                            &snapshot,
                                            original,
                                            persisted.ok().flatten(),
                                        )
                                        .err()
                                    })
                                } else {
                                    None
                                };
                                let fail_close_reason = persistence_fail_close_reason(
                                    &candidate_error,
                                    restore_error.as_ref(),
                                );
                                if let Some(reason) = fail_close_reason {
                                    persistence_failure = Some(reason.clone());
                                    agent = None;
                                    agent_available = false;
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Failed),
                                        Ordering::Release,
                                    );
                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                        AgentEvent::Error {
                                            message: format!(
                                                "conversation restore persistence failed; runtime stopped: {reason}"
                                            ),
                                            http_status: None,
                                            code: None,
                                            retryable: None,
                                        },
                                    ));
                                } else {
                                    match assemble_runtime_resources(&mut runtime).await {
                                        Ok(rollback) => {
                                            agent = Some(rollback);
                                            agent_available = true;
                                            controls.state.store(
                                                runtime_phase_state(
                                                    generation,
                                                    RuntimePhase::Ready,
                                                ),
                                                Ordering::Release,
                                            );
                                        }
                                        Err(rollback_error) => {
                                            agent = None;
                                            agent_available = false;
                                            controls.state.store(
                                                runtime_phase_state(
                                                    generation,
                                                    RuntimePhase::Failed,
                                                ),
                                                Ordering::Release,
                                            );
                                            let _ = runtime_event_tx.send(
                                                CodingRuntimeEvent::Agent(AgentEvent::Error {
                                                    message: format!(
                                                        "conversation restore rollback failed: {rollback_error}"
                                                    ),
                                                    http_status: None,
                                                    code: None,
                                                    retryable: None,
                                                }),
                                            );
                                        }
                                    }
                                }
                                resources = Some(runtime);
                                let detail = restore_error
                                    .map(|error| format!("; snapshot restore failed: {error}"))
                                    .unwrap_or_default();
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(format!(
                                    "{candidate_error}{detail}"
                                ))));
                            }
                        }
                    }
                    Some(CodingRuntimeControl::StartGoal { generation: request_generation, condition, images, done, recovery_tx }) => {
                        if !native_protocol || request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        if resources.is_none() {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        }
                        // Starting a goal now means opening its first round, so
                        // an agent that cannot take one means the goal cannot
                        // start. Registering it anyway would put the badge on
                        // screen over a session that is never going to move.
                        if !agent_available {
                            let _ = done.send(Err(provider_unavailable_reason
                                .map(RuntimeError::ProviderUnavailable)
                                .unwrap_or(RuntimeError::Unavailable)));
                            continue;
                        }
                        if active_turn.is_some() && held_turn.is_none() {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        cancel_controllers_and_finish_held(
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            resources.as_ref().map(|runtime| runtime.loop_active.as_ref()),
                            controls.state.as_ref(),
                            generation,
                            RuntimePhase::Ready,
                            &runtime_event_tx,
                            "superseded by /goal",
                        );
                        if let Some(runtime) = resources.as_ref() {
                            next_controller_id = next_controller_id.wrapping_add(1);
                            let controller_id = next_controller_id;
                            // Start immediately on the configured default so goal start never
                            // blocks the owner loop on a network round-trip. The round budget
                            // scales per plan (Pro 1000 → 300, Lite 800 → 240) from the account's
                            // live request quota, resolved OFF the loop and applied via a self-sent
                            // AdjustGoalRounds; env override wins and any miss keeps the default.
                            let next = GoalState::new(
                                controller_id,
                                condition.clone(),
                                runtime.config.goal_max_rounds,
                                runtime.config.goal_max_duration_secs,
                            );
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(next.progress()));
                            goal = Some(next);
                            let _ = done.send(Ok(()));
                            // The first round is this one. Every later round
                            // comes from a turn's END — the evaluator reads the
                            // round that just finished and writes the next
                            // prompt — so a goal that only registered itself
                            // would sit there with nothing to end, until the
                            // person typed something of their own, and THAT
                            // would become round one's prompt. Which is not
                            // what they asked for.
                            //
                            // Self-sent rather than opened here: `Submit` is
                            // where a turn begins, with the execution policy,
                            // the receipt and the turn accounting that belong
                            // to it. A second way in would be a second answer
                            // to "what is a turn".
                            let (started, _) = oneshot::channel();
                            let _ = recovery_tx.send(CodingRuntimeControl::Submit {
                                generation: request_generation,
                                input: UserInput { text: condition, images },
                                done: started,
                            });
                            // Only spawn the quota fetch when it could actually change the cap:
                            // an env override short-circuits to the default, and with no live
                            // rate-limit source there is nothing to derive from.
                            if crate::config::goal_max_rounds_env().is_none() {
                                if let Some(source) = runtime.parts.rate_limit_source().cloned() {
                                    let default = runtime.config.goal_max_rounds;
                                    let tx = recovery_tx.clone();
                                    tokio::spawn(async move {
                                        let max_rounds = resolve_goal_round_cap(Some(&source), default).await;
                                        if max_rounds != default {
                                            let _ = tx.send(CodingRuntimeControl::AdjustGoalRounds {
                                                generation: request_generation,
                                                controller_id,
                                                max_rounds,
                                            });
                                        }
                                    });
                                }
                            }
                        }
                    }
                    Some(CodingRuntimeControl::AdjustGoalRounds { generation: request_generation, controller_id, max_rounds }) => {
                        // Apply the live per-plan round budget resolved off the loop. Ignore
                        // it if a newer generation took over, the goal is gone, a different
                        // controller now owns it, or it already stopped.
                        if request_generation == generation {
                            if let Some(state) = goal.as_mut() {
                                if state.id == controller_id && state.active {
                                    state.set_round_cap(max_rounds);
                                    let _ = runtime_event_tx
                                        .send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                }
                            }
                        }
                    }
                    Some(CodingRuntimeControl::StopGoal { generation: request_generation, done }) => {
                        if !native_protocol || request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        }
                        if let Some(mut current) = goal.take() {
                            current.cancel.cancel();
                            current.finish(GoalTerminal::Cancelled, "cleared by user");
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(current.progress()));
                        }
                        if let Some((turn_id, _, snapshot, stats)) =
                            held_turn.take_if(|_| !kernel_turn_open)
                        {
                            active_turn = None;
                            cancel_pending = false;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                                turn_id, reason: StopReason::Cancelled, snapshot, stats,
                            }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        } else if held_turn.is_some() {
                        // A turn the agent opened under the hold is not what
                        // this stops: the person asked for the controller to
                        // stop, not for their own message to. The hold goes, so
                        // the turn now running is the one this owner accounts
                        // for, and its own end reports it.
                            held_turn = None;
                        } else if active_turn.is_some() {
                            if request_cancel_snapshot(&agent) {
                                cancel_pending = true;
                                snapshot_in_flight = true;
                            } else {
                                agent_available = false;
                                controls.state.store(runtime_phase_state(generation, RuntimePhase::Failed), Ordering::Release);
                                let _ = done.send(Err(RuntimeError::DeliveryFailed));
                                continue;
                            }
                        }
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::StartLoop { generation: request_generation, prompt, every, done, recovery_tx }) => {
                        if !native_protocol || request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        if resources.is_none() {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        }
                        // Same as `/goal`: the first round is opened here, so
                        // no agent to open it means no loop.
                        if !agent_available {
                            let _ = done.send(Err(provider_unavailable_reason
                                .map(RuntimeError::ProviderUnavailable)
                                .unwrap_or(RuntimeError::Unavailable)));
                            continue;
                        }
                        if active_turn.is_some() && held_turn.is_none() {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        cancel_controllers_and_finish_held(
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            resources.as_ref().map(|runtime| runtime.loop_active.as_ref()),
                            controls.state.as_ref(),
                            generation,
                            RuntimePhase::Ready,
                            &runtime_event_tx,
                            "superseded by /loop",
                        );
                        while loop_fire_rx.try_recv().is_ok() {}
                        while wakeup_rx.try_recv().is_ok() {}
                        if let Some(runtime) = resources.as_ref() {
                            next_controller_id = next_controller_id.wrapping_add(1);
                            let next = LoopState::new(next_controller_id, prompt.clone(), runtime.config.loop_max_rounds).every(every);
                            runtime.loop_active.store(true, Ordering::Release);
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(next.progress()));
                            loop_state = Some(next);
                            let _ = done.send(Ok(()));
                            // The first pass, for the same reason as `/goal`'s:
                            // the next one is scheduled by the END of this one,
                            // whether the model asks (`schedule_wakeup`) or the
                            // person set a cadence. With no first pass there is
                            // nothing to schedule from — "every five minutes"
                            // would start five minutes late at best, and never
                            // at all in the model-paced form.
                            let (started, _) = oneshot::channel();
                            let _ = recovery_tx.send(CodingRuntimeControl::Submit {
                                generation: request_generation,
                                input: UserInput::from(prompt),
                                done: started,
                            });
                        }
                    }
                    Some(CodingRuntimeControl::StopLoop { generation: request_generation, done }) => {
                        if !native_protocol || request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        }
                        if let Some(mut current) = loop_state.take() {
                            current.cancel.cancel();
                            current.active = false;
                            current.last_reason = Some("cleared by user".into());
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(current.progress()));
                        }
                        if let Some(runtime) = resources.as_ref() { runtime.loop_active.store(false, Ordering::Release); }
                        pending_wakeup = None;
                        if let Some((turn_id, _, snapshot, stats)) =
                            held_turn.take_if(|_| !kernel_turn_open)
                        {
                            active_turn = None;
                            cancel_pending = false;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                                turn_id, reason: StopReason::Cancelled, snapshot, stats,
                            }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        } else if held_turn.is_some() {
                        // A turn the agent opened under the hold is not what
                        // this stops: the person asked for the controller to
                        // stop, not for their own message to. The hold goes, so
                        // the turn now running is the one this owner accounts
                        // for, and its own end reports it.
                            held_turn = None;
                        } else if active_turn.is_some() {
                            if request_cancel_snapshot(&agent) {
                                cancel_pending = true;
                                snapshot_in_flight = true;
                            } else {
                                agent_available = false;
                                controls.state.store(runtime_phase_state(generation, RuntimePhase::Failed), Ordering::Release);
                                let _ = done.send(Err(RuntimeError::DeliveryFailed));
                                continue;
                            }
                        }
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::Shutdown { generation: request_generation }) => {
                        if let Some(task) = next_prompt_task.take() {
                            task.abort();
                        }
                        if request_generation == generation {
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::ShuttingDown),
                                Ordering::Release,
                            );
                        }
                        if native_protocol {
                            fail_close_pending_requests(
                                &agent,
                                &mut pending_requests,
                                active_turn.is_some(),
                            );
                        }
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeShutdown,
                            resources.as_ref().map(|runtime| &runtime.parts.team_manager),
                            resources
                                .as_ref()
                                .and_then(|runtime| runtime.parts.snapshot_persistence_status()),
                        )
                        .await;
                        forced_shutdown = stop_report.forced;
                        if native_protocol {
                            finish_stopped_native_turn(
                                &stop_report,
                                resources.as_ref(),
                                &mut active_turn,
                                &mut terminal_reason,
                                &mut turn_stats,
                                &mut conversation_revision,
                                &mut snapshot_waiters,
                                &runtime_event_tx,
                            );
                        }
                        let _ = fail_close_after_stopped_persistence(
                            &stop_report,
                            resources.as_ref(),
                            &mut goal,
                            &mut loop_state,
                            &mut pending_wakeup,
                            &mut held_turn,
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut conversation_revision,
                            &mut snapshot_waiters,
                            &mut agent_available,
                            controls.state.as_ref(),
                            generation,
                            &runtime_event_tx,
                        );
                        interrupt_queued_controls(
                            &mut controls,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeShutdown,
                        );
                        shutdown_was_handled = true;
                        exit_reason = RuntimeExitReason::ShutdownRequested;
                        break;
                    }
                    None => controls_open = false,
                },
                command = kernel_command_rx.recv() => match command {
                    Some(command) => {
                        let _ = send_agent_command(&agent, command);
                    }
                    None => break,
                },
                event = receive_agent_event(&mut agent) => match event {
                    Some(event) => {
                        if let AgentEvent::Compacted {
                            committed: true,
                            snapshot: Some(snapshot),
                            ..
                        } = &event
                        {
                            if let Some(runtime) = resources.as_mut() {
                                if runtime.parts.session.is_none() {
                                    runtime.parts.set_runtime_resume(snapshot.clone());
                                }
                            }
                        }
                        if matches!(
                            &event,
                            AgentEvent::Compacted {
                                committed: true,
                                ..
                            }
                        ) {
                            conversation_revision = conversation_revision.wrapping_add(1);
                        }
                        let uncertain_compaction = matches!(
                            &event,
                            AgentEvent::CompactionFailed { .. }
                        )
                        .then(|| {
                            resources.as_ref().and_then(|runtime| {
                                runtime.parts.take_snapshot_persistence_uncertain()
                            })
                        })
                        .flatten();
                        if matches!(
                            &event,
                            AgentEvent::Compacted { .. } | AgentEvent::CompactionFailed { .. }
                        ) {
                            if let Some(warning) = resources.as_ref().and_then(|runtime| {
                                runtime.parts.take_cost_persistence_warning()
                            }) {
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::ControllerWarning(warning));
                            }
                        }
                        let event = handle_compaction_event(
                            event,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                        );
                        if let Some(error) = uncertain_compaction {
                            persistence_failure = Some(error.clone());
                            let message = format!(
                                "compaction persistence became uncertain; runtime stopped: {error}"
                            );
                            pending_requests.clear();
                            pending_wakeup = None;
                            terminal_reason = None;
                            held_turn = None;
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.finish(
                                    GoalTerminal::Failed,
                                    "ended: compaction persistence became uncertain",
                                );
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            if let Some(mut state) = loop_state.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some(
                                    "ended: compaction persistence became uncertain".into(),
                                );
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::LoopChanged(state.progress()));
                            }
                            if let Some(runtime) = resources.as_ref() {
                                runtime.loop_active.store(false, Ordering::Release);
                            }
                            for waiter in snapshot_waiters.drain(..) {
                                let _ = waiter.send(Err(RuntimeError::SnapshotUnavailable(
                                    message.clone(),
                                )));
                            }
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                AgentEvent::Error {
                                    message: message.clone(),
                                    http_status: None,
                                    code: None,
                                    retryable: None,
                                },
                            ));
                            if let Some(turn_id) = active_turn.take() {
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::TurnFinished(
                                        TurnCompletion::SnapshotUnavailable {
                                            turn_id,
                                            reason: StopReason::ProviderError,
                                            error: RuntimeSnapshotError { message },
                                            stats: std::mem::take(&mut turn_stats),
                                        },
                                    ),
                                );
                            } else {
                                turn_stats = RuntimeTurnStats::default();
                            }
                            snapshot_in_flight = false;
                            let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                            agent = None;
                            agent_available = false;
                            controls.state.store(
                                runtime_phase_state(generation, RuntimePhase::Failed),
                                Ordering::Release,
                            );
                            continue;
                        }
                        match event {
                        Some(event) if native_protocol => match event {
                            AgentEvent::PolicyIntervention { intervention } => {
                                pending_policy_intervention = Some(intervention.clone());
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                    AgentEvent::PolicyIntervention { intervention },
                                ));
                            }
                            AgentEvent::Usage(meta) => {
                                observed_tokens = Some(meta.used_tokens as usize);
                                turn_stats.record_usage(&meta);
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                    AgentEvent::Usage(meta),
                                ));
                            }
                            AgentEvent::Request { id, kind, payload } => {
                                // Auto is a runtime-owned execution mode, so approval bypass
                                // belongs at this common request boundary rather than in each
                                // driver. Do not publish an approval that has already been
                                // answered: otherwise WebUI/TUI/ACP can briefly render a stale
                                // prompt. Non-approval requests (notably request_user_input)
                                // must still round-trip through the driver.
                                let bypass_approval = kind == APPROVAL_KIND
                                    && resources.as_ref().is_some_and(|runtime| {
                                        runtime.parts.bypass_mode.load(Ordering::Acquire)
                                    });
                                if bypass_approval {
                                    let value = serde_json::to_value(ApprovalResponse::allow())
                                        .unwrap_or(serde_json::Value::Null);
                                    if send_agent_command(
                                        &agent,
                                        AgentCommand::Respond { id, value },
                                    ) {
                                        continue;
                                    }
                                    // Preserve the request if delivery failed. Normal owner
                                    // teardown will fail it closed; never claim success for a
                                    // response that did not reach the kernel.
                                }
                                pending_requests.insert(id, kind.clone());
                                controls.state.store(
                                    runtime_phase_state(
                                        generation,
                                        RuntimePhase::WaitingApproval,
                                    ),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Request(
                                    RuntimeRequest {
                                        id,
                                        kind,
                                        payload,
                                        snapshot: resources
                                            .as_ref()
                                            .and_then(current_runtime_snapshot)
                                            .map(Arc::new),
                                    },
                                ));
                            }
                            AgentEvent::TurnComplete { reason, .. } => {
                                kernel_turn_open = false;
                                // The tree carries the real cause; this protocol's
                                // drivers match on the folded set.
                                let reason = reason.folded_for_runtime_drivers();
                                pending_steer_acknowledgements.clear();
                                let persistence_status = resources.as_ref().and_then(|runtime| {
                                    runtime.parts.snapshot_persistence_status()
                                });
                                emit_terminal_persistence_warnings(
                                    persistence_status.as_ref(),
                                    &runtime_event_tx,
                                );
                                turn_stats.duration = turn_started_at
                                    .take()
                                    .map(|started| started.elapsed())
                                    .unwrap_or_default();
                                if let Some(error) = resources.as_ref().and_then(|runtime| {
                                    runtime.parts.take_snapshot_persistence_uncertain()
                                }) {
                                    persistence_failure = Some(error.clone());
                                    let turn_id = active_turn.take().unwrap_or_default();
                                    terminal_reason = None;
                                    pending_requests.clear();
                                    pending_wakeup = None;
                                    if let Some(mut state) = goal.take() {
                                        state.cancel.cancel();
                                        state.finish(
                                            GoalTerminal::Failed,
                                            "ended: session persistence became uncertain",
                                        );
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::GoalChanged(state.progress()),
                                        );
                                    }
                                    if let Some(mut state) = loop_state.take() {
                                        state.cancel.cancel();
                                        state.active = false;
                                        state.last_reason = Some(
                                            "ended: session persistence became uncertain".into(),
                                        );
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::LoopChanged(state.progress()),
                                        );
                                    }
                                    if let Some(runtime) = resources.as_ref() {
                                        runtime.loop_active.store(false, Ordering::Release);
                                    }
                                    let message = format!(
                                        "session persistence became uncertain; runtime stopped: {error}"
                                    );
                                    for waiter in snapshot_waiters.drain(..) {
                                        let _ = waiter.send(Err(RuntimeError::SnapshotUnavailable(
                                            message.clone(),
                                        )));
                                    }
                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                        AgentEvent::Error {
                                            message: message.clone(),
                                            http_status: None,
                                            code: None,
                                            retryable: None,
                                        },
                                    ));
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::TurnFinished(
                                            TurnCompletion::SnapshotUnavailable {
                                                turn_id,
                                                reason: StopReason::ProviderError,
                                                error: RuntimeSnapshotError { message },
                                                stats: std::mem::take(&mut turn_stats),
                                            },
                                        ),
                                    );
                                    snapshot_in_flight = false;
                                    let _ = send_agent_command(&agent, AgentCommand::Shutdown);
                                    agent = None;
                                    agent_available = false;
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Failed),
                                        Ordering::Release,
                                    );
                                    continue;
                                }
                                terminal_reason = Some(reason);
                                if !snapshot_in_flight {
                                    if send_agent_command(&agent, AgentCommand::Snapshot) {
                                        snapshot_in_flight = true;
                                    } else {
                                        let turn_id = active_turn.take().unwrap_or_default();
                                        terminal_reason = None;
                                        pending_requests.clear();
                                        pending_wakeup = None;
                                        if let Some(mut state) = goal.take() {
                                            state.cancel.cancel();
                                            state.finish(
                                                GoalTerminal::Failed,
                                                "ended: kernel snapshot command delivery failed",
                                            );
                                            let _ = runtime_event_tx.send(
                                                CodingRuntimeEvent::GoalChanged(state.progress()),
                                            );
                                        }
                                        if let Some(mut state) = loop_state.take() {
                                            state.cancel.cancel();
                                            state.active = false;
                                            state.last_reason = Some(
                                                "ended: kernel snapshot command delivery failed"
                                                    .into(),
                                            );
                                            let _ = runtime_event_tx.send(
                                                CodingRuntimeEvent::LoopChanged(state.progress()),
                                            );
                                        }
                                        if let Some(runtime) = resources.as_ref() {
                                            runtime
                                                .loop_active
                                                .store(false, Ordering::Release);
                                        }
                                        let error = RuntimeSnapshotError {
                                            message: "kernel snapshot command delivery failed".into(),
                                        };
                                        let unavailable = RuntimeError::SnapshotUnavailable(
                                            error.message.clone(),
                                        );
                                        for waiter in snapshot_waiters.drain(..) {
                                            let _ = waiter.send(Err(unavailable.clone()));
                                        }
                                        let stats = std::mem::take(&mut turn_stats);
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::TurnFinished(
                                                TurnCompletion::SnapshotUnavailable {
                                                    turn_id,
                                                    reason: StopReason::ProviderError,
                                                    error,
                                                    stats,
                                                },
                                            ),
                                        );
                                        agent_available = false;
                                        controls.state.store(
                                            runtime_phase_state(
                                                generation,
                                                RuntimePhase::Failed,
                                            ),
                                            Ordering::Release,
                                        );
                                    }
                                }
                            }
                            AgentEvent::Snapshot { snapshot } => {
                                snapshot_in_flight = false;
                                if terminal_reason.is_some() || cancel_pending {
                                    conversation_revision = conversation_revision.wrapping_add(1);
                                }
                                if let Some(runtime) = resources.as_mut() {
                                    if runtime.parts.session.is_none() {
                                        runtime.parts.set_runtime_resume(snapshot.clone());
                                    }
                                }
                                let snapshot = Arc::new(snapshot);
                                let undo_snapshot = resources
                                    .as_ref()
                                    .and_then(current_runtime_snapshot)
                                    .map(Arc::new)
                                    .unwrap_or_else(|| snapshot.clone());
                                for waiter in snapshot_waiters.drain(..) {
                                    let _ = waiter.send(Ok(RuntimeSnapshotReceipt {
                                        snapshot: snapshot.clone(),
                                        undo_snapshot: undo_snapshot.clone(),
                                        revision: conversation_revision,
                                    }));
                                }
                                if let Some(reason) = terminal_reason
                                    .take()
                                    .or_else(|| cancel_pending.then_some(StopReason::Cancelled))
                                {
                                    cancel_pending = false;
                                    pending_requests.clear();
                                    let stats = std::mem::take(&mut turn_stats);
                                    let turn_id = active_turn.unwrap_or_default();
                                    let mut completion_reason = reason;
                                    if let Some(state) = goal.as_mut().filter(|state| state.active) {
                                        if let Some(meta) = stats.last_usage.as_ref() {
                                            state.tokens_used = state.tokens_used.saturating_add(
                                                (meta.tokens.prompt + meta.tokens.completion) as u64,
                                            );
                                        }
                                        // Keep one bounded runtime-owned recap. It survives
                                        // replacement of the conversation and is attached once
                                        // to a real recovery submit; it is deliberately not copied
                                        // into every synthetic continuation.
                                        state.update_progress_recap(summarize_for_goal(
                                            &snapshot.messages,
                                            state.last_reason.as_deref(),
                                        ));
                                        let stop_reason = state.cap_reached();
                                        let evaluate = matches!(
                                            reason,
                                            StopReason::Stopped
                                                | StopReason::MaxContinuations
                                                | StopReason::MaxRounds
                                        );
                                        let recoverable = matches!(
                                            reason,
                                            StopReason::Timeout | StopReason::ProviderError
                                        );
                                        let mut keep_goal_registered = false;
                                        if let Some(why) = stop_reason {
                                            // A cap fired (round budget, or the optional time cap).
                                            // This is "ran out of budget", NOT "the evaluator judged
                                            // the work unfinished" — the note names the budget and
                                            // how to continue, and never claims "goal not met".
                                            let note = goal_cap_stop_note(why, state.max_rounds);
                                            state.pause_at_cap(note.clone());
                                            keep_goal_registered = true;
                                            completion_reason = match why {
                                                "round limit" => StopReason::MaxRounds,
                                                "time limit" => StopReason::Timeout,
                                                _ => StopReason::ProviderError,
                                            };
                                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                            let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(format!("goal {note}")));
                                        } else if evaluate {
                                            state.unproductive = 0;
                                            let condition = state.condition.clone();
                                            let controller_id = state.id;
                                            let cancel = state.cancel.clone();
                                            let summary = summarize_for_goal(
                                                &snapshot.messages,
                                                state.last_reason.as_deref(),
                                            );
                                            let provider = resources.as_ref().and_then(|runtime| {
                                                let session_id = runtime.parts.session.as_ref().map(|binding| binding.id.as_str());
                                                build_goal_evaluator_provider(
                                                    &runtime.provider_factory,
                                                    &runtime.config,
                                                    session_id,
                                                )
                                                .ok()
                                            });
                                            held_turn = Some((turn_id, reason, snapshot.clone(), stats));
                                            let tx = goal_eval_tx.clone();
                                            if let Some(provider) = provider {
                                                tokio::spawn(async move {
                                                    let inner = tokio::spawn(async move {
                                                        evaluate_goal(generation, controller_id, provider, condition, summary, cancel).await
                                                    });
                                                    let outcome = match inner.await {
                                                        Ok(outcome) => outcome,
                                                        Err(_) => EvalOutcome {
                                                            generation,
                                                            controller_id,
                                                            result: GoalResult::Error(
                                                                "evaluator task failed".into(),
                                                            ),
                                                            usage: None,
                                                        },
                                                    };
                                                    let _ = tx.send(outcome);
                                                });
                                                continue;
                                            }
                                            let _ = tx.send(EvalOutcome {
                                                generation,
                                                controller_id,
                                                result: GoalResult::Error("could not build evaluator provider".into()),
                                                usage: None,
                                            });
                                            continue;
                                        } else if recoverable {
                                            state.unproductive = state.unproductive.saturating_add(1);
                                            if state.unproductive < MAX_UNPRODUCTIVE {
                                                state.round = state.round.saturating_add(1);
                                                state.last_reason = Some(format!("round ended: {reason:?}"));
                                                let text = goal_continuation_message(
                                                    state.last_reason.as_deref().unwrap_or("round failed"),
                                                    &state.condition,
                                                );
                                                held_turn = None;
                                                if send_agent_command(
                                                    &agent,
                                                    AgentCommand::SendSyntheticMessage { text },
                                                ) {
                                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                                    continue;
                                                }
                                                agent_available = false;
                                                state.cancel.cancel();
                                                state.finish(
                                                    GoalTerminal::Failed,
                                                    "continuation dispatch failed",
                                                );
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(
                                                    "goal stopped: continuation dispatch failed".into(),
                                                ));
                                                goal = None;
                                                active_turn = None;
                                                let _ = runtime_event_tx.send(
                                                    CodingRuntimeEvent::TurnFinished(
                                                        TurnCompletion::Completed {
                                                            turn_id,
                                                            reason: StopReason::ProviderError,
                                                            snapshot,
                                                            stats,
                                                        },
                                                    ),
                                                );
                                                controls.state.store(
                                                    runtime_phase_state(
                                                        generation,
                                                        RuntimePhase::Failed,
                                                    ),
                                                    Ordering::Release,
                                                );
                                                continue;
                                            } else {
                                                // The stop reason does not distinguish a full context
                                                // from network/provider failures. Preserve the goal and
                                                // describe both recovery paths instead of falsely
                                                // prescribing `/compact` for every provider outage.
                                                state.pause_for_recovery(
                                                    "paused after repeated provider/timeout failures; check provider/network status, or run /compact if the context is full, then send a message to continue",
                                                );
                                                keep_goal_registered = true;
                                                held_turn = None;
                                                completion_reason = StopReason::ProviderError;
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning("goal paused after repeated provider/timeout failures — check provider/network status, or run /compact if context is full, then continue".into()));
                                            }
                                        } else {
                                            state.finish(
                                                GoalTerminal::Failed,
                                                format!("ended: {reason:?}"),
                                            );
                                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                        }
                                        if !keep_goal_registered {
                                            goal = None;
                                        }
                                    } else if let Some(state) = loop_state.as_mut().filter(|state| state.active) {
                                        if reason == StopReason::Stopped {
                                            // The person's cadence, or failing
                                            // that whatever the model asked for
                                            // — `LoopState::next_round` owns
                                            // that choice so there is one
                                            // answer to "when is the next one".
                                            if let Some(wakeup) = state.next_round(pending_wakeup.take()) {
                                                let cancel = state.cancel.clone();
                                                let controller_id = state.id;
                                                let tx = loop_fire_tx.clone();
                                                let delay = std::time::Duration::from_secs(wakeup.delay_seconds as u64);
                                                tokio::spawn(async move {
                                                    tokio::select! {
                                                        _ = tokio::time::sleep(delay) => { let _ = tx.send((generation, controller_id, wakeup)); }
                                                        _ = cancel.cancelled() => {}
                                                    }
                                                });
                                                held_turn = Some((turn_id, reason, snapshot, stats));
                                                continue;
                                            }
                                            state.active = false;
                                            state.last_reason = Some("completed".into());
                                        } else {
                                            pending_wakeup = None;
                                            state.active = false;
                                            state.last_reason = Some(format!("ended: {reason:?}"));
                                        }
                                        state.cancel.cancel();
                                        let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                                        if let Some(runtime) = resources.as_ref() { runtime.loop_active.store(false, Ordering::Release); }
                                        loop_state = None;
                                    }
                                    active_turn = None;
                                    if completion_reason == StopReason::Stopped
                                        && resources.as_ref().is_some_and(|runtime| {
                                            runtime.config.next_prompt_suggestions
                                        })
                                    {
                                        if let Some(task) = next_prompt_task.take() {
                                            task.abort();
                                        }
                                        let provider = resources.as_ref().and_then(|runtime| {
                                            let session_id = runtime
                                                .parts
                                                .session
                                                .as_ref()
                                                .map(|binding| binding.id.as_str());
                                            runtime
                                                .provider_factory
                                                .build(&runtime.config, session_id)
                                                .ok()
                                        });
                                        if let Some(provider) = provider {
                                            let tx = next_prompt_tx.clone();
                                            let messages = snapshot.messages.clone();
                                            let suggestion_generation = generation;
                                            let suggestion_revision = conversation_revision;
                                            let suggestion_session_id = resources
                                                .as_ref()
                                                .and_then(|runtime| runtime.parts.session.as_ref())
                                                .map(|binding| binding.id.clone());
                                            next_prompt_task = Some(tokio::spawn(async move {
                                                if let Some(text) = crate::next_prompt_suggestion::generate_next_prompt_suggestion(
                                                    provider,
                                                    &messages,
                                                )
                                                .await
                                                {
                                                    let _ = tx.send(NextPromptSuggestionOutcome {
                                                        generation: suggestion_generation,
                                                        revision: suggestion_revision,
                                                        session_id: suggestion_session_id,
                                                        turn_id,
                                                        text,
                                                    });
                                                }
                                            }));
                                        }
                                    }
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::TurnFinished(
                                            TurnCompletion::Completed {
                                                turn_id,
                                                reason: completion_reason,
                                                snapshot,
                                                stats,
                                            },
                                        ),
                                    );
                                    controls.state.store(
                                        runtime_phase_state(generation, RuntimePhase::Ready),
                                        Ordering::Release,
                                    );
                                }
                            }
                            AgentEvent::TurnStarted { .. } => {
                                if let Some(intervention) = pending_policy_intervention.take() {
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::PolicyInterventionCleared {
                                            intervention_id: intervention.id,
                                        },
                                    );
                                }
                                turn_started_at = Some(std::time::Instant::now());
                                kernel_turn_open = true;
                                // A turn nobody submitted: a catalog command
                                // (`/init`, `/worklog`, a skill like `/setup`)
                                // put its prompt in the agent's inbox and the
                                // agent woke on it. It is still this owner's
                                // turn to account for — without an
                                // `active_turn`, a cancel reads it as idle,
                                // answers `Ok` and never reaches the kernel.
                                if active_turn.is_none() {
                                    next_turn_id = next_turn_id.wrapping_add(1);
                                    active_turn = Some(next_turn_id);
                                    turn_stats = RuntimeTurnStats::default();
                                }
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::InTurn),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                    AgentEvent::TurnStarted { turn: None },
                                ));
                            }
                            event @ AgentEvent::ToolStarted { .. } => {
                                turn_stats.tool_call_count =
                                    turn_stats.tool_call_count.saturating_add(1);
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(event));
                            }
                            AgentEvent::Steered { count, inputs, .. } => {
                                let acknowledged = acknowledge_steered_inputs(
                                    &mut pending_steer_acknowledgements,
                                    generation,
                                    &inputs,
                                );
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                    AgentEvent::Steered { turn: None, count, inputs },
                                ));
                                if !acknowledged.is_empty() {
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::SteerAcknowledged {
                                            inputs: acknowledged,
                                        },
                                    );
                                }
                            }
                            event => {
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(event));
                            }
                        },
                        Some(event @ AgentEvent::Usage(_)) => {
                            if let AgentEvent::Usage(meta) = &event {
                                observed_tokens = Some(meta.used_tokens as usize);
                            }
                            if kernel_event_tx.send(event).is_err() {
                                break;
                            }
                        }
                        Some(event) => {
                            if kernel_event_tx.send(event).is_err() {
                                break;
                            }
                        }
                            None => {}
                        }
                    },
                    None => {
                        if native_protocol {
                            let message = "kernel event stream closed before snapshot terminal";
                            let unavailable = RuntimeError::SnapshotUnavailable(message.into());
                            for waiter in snapshot_waiters.drain(..) {
                                let _ = waiter.send(Err(unavailable.clone()));
                            }
                            let _ = pending_wakeup.take();
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.finish(
                                    GoalTerminal::Failed,
                                    "ended: kernel event stream closed",
                                );
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            if let Some(mut state) = loop_state.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("ended: kernel event stream closed".into());
                                let _ = runtime_event_tx
                                    .send(CodingRuntimeEvent::LoopChanged(state.progress()));
                            }
                            if let Some(runtime) = resources.as_ref() {
                                runtime.loop_active.store(false, Ordering::Release);
                            }
                            if let Some(turn_id) = active_turn.take() {
                                let reason = terminal_reason
                                    .take()
                                    .unwrap_or(StopReason::ProviderError);
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::TurnFinished(
                                        TurnCompletion::SnapshotUnavailable {
                                            turn_id,
                                            reason,
                                            error: RuntimeSnapshotError {
                                                message: message.into(),
                                            },
                                            stats: std::mem::take(&mut turn_stats),
                                        },
                                    ),
                                );
                            }
                        }
                        break;
                    },
                },
            }
        }
        if let Some(task) = next_prompt_task.take() {
            task.abort();
        }
        controls.state.store(
            runtime_phase_state(generation, RuntimePhase::Stopped),
            Ordering::Release,
        );
        for waiter in snapshot_waiters.drain(..) {
            let _ = waiter.send(Err(RuntimeError::Unavailable));
        }
        interrupt_queued_controls(
            &mut controls,
            &runtime_event_tx,
            CompactionInterruption::RuntimeShutdown,
        );
        if !shutdown_was_handled {
            let stop_report = stop_current_agent(
                &mut agent,
                &mut compactions,
                &mut observed_tokens,
                &runtime_event_tx,
                CompactionInterruption::RuntimeShutdown,
                resources
                    .as_ref()
                    .map(|runtime| &runtime.parts.team_manager),
                resources
                    .as_ref()
                    .and_then(|runtime| runtime.parts.snapshot_persistence_status()),
            )
            .await;
            forced_shutdown = stop_report.forced;
            let _ = fail_close_after_stopped_persistence(
                &stop_report,
                resources.as_ref(),
                &mut goal,
                &mut loop_state,
                &mut pending_wakeup,
                &mut held_turn,
                &mut active_turn,
                &mut terminal_reason,
                &mut turn_stats,
                &mut conversation_revision,
                &mut snapshot_waiters,
                &mut agent_available,
                controls.state.as_ref(),
                generation,
                &runtime_event_tx,
            );
            controls.state.store(
                runtime_phase_state(generation, RuntimePhase::Stopped),
                Ordering::Release,
            );
        }
        // Release the active-session lease before publishing the shutdown terminal,
        // so a caller may safely start a replacement immediately after `shutdown`.
        drop(resources);
        controls.terminal_tx.send_replace(Some(RuntimeExit {
            reason: exit_reason,
            forced: forced_shutdown,
        }));
    });

    KernelRuntimeAdapter {
        commands: kernel_command_tx,
        events: kernel_event_rx,
        owner_tx,
        owner_task,
    }
}

fn emit_compaction_interrupted(
    runtime_event_tx: &RuntimeEventEmitter,
    trigger: CompactTrigger,
    reason: CompactionInterruption,
) {
    let _ = runtime_event_tx.send(CodingRuntimeEvent::CompactionFinished {
        completion: CompactionCompletion::Interrupted { trigger, reason },
    });
}

fn interrupt_queued_controls(
    controls: &mut CodingRuntimeControlReceiver,
    runtime_event_tx: &RuntimeEventEmitter,
    reason: CompactionInterruption,
) {
    // Closing first linearizes shutdown against a sender that read the previous
    // available generation but has not reached `send` yet: later sends fail, while
    // everything already accepted remains drainable here.
    controls.rx.close();
    while let Ok(control) = controls.rx.try_recv() {
        reject_runtime_control(control, runtime_event_tx, reason);
    }
}

fn reject_runtime_control(
    control: CodingRuntimeControl,
    runtime_event_tx: &RuntimeEventEmitter,
    reason: CompactionInterruption,
) {
    match control {
        CodingRuntimeControl::Compact { focus, .. } => {
            emit_compaction_interrupted(runtime_event_tx, CompactTrigger::Manual { focus }, reason)
        }
        CodingRuntimeControl::Shutdown { .. } => {}
        // Fire-and-forget self-send with no waiter: nothing to fail-close.
        CodingRuntimeControl::AdjustGoalRounds { .. } => {}
        // A question, not a change: a stopping runtime has nothing pending, and
        // dropping the sender says so to a caller that is asking anyway.
        CodingRuntimeControl::PendingPolicyIntervention { done } => {
            let _ = done.send(None);
        }
        CodingRuntimeControl::Submit { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        // A stopping runtime has no catalog to describe or switch. Fail-closed
        // like every other awaited control.
        CodingRuntimeControl::ToolCatalog { done, .. }
        | CodingRuntimeControl::SwitchTool { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::Respond { done, .. }
        | CodingRuntimeControl::ResolvePolicyIntervention { done, .. }
        | CodingRuntimeControl::Cancel { done, .. }
        | CodingRuntimeControl::PauseGoal { done, .. }
        | CodingRuntimeControl::SetMode { done, .. }
        | CodingRuntimeControl::WaitMcpReady { done, .. }
        | CodingRuntimeControl::QueueLocalContext { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::Snapshot { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::ContextStats { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        // A stopping runtime cannot say which mode it is in either — the flags
        // belong to a tree that is being torn down.
        CodingRuntimeControl::Mode { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::McpStatus { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::McpTools { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::ApproveMcpTool { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::McpRows { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::McpDetail { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::McpAct { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::WithdrawMcpTools { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::ReassembleProvider { done, .. }
        | CodingRuntimeControl::DeactivateProvider { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::Reprepare { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::ApplyUndo { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::RewindCatalog { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::UndoTarget { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::WorkspaceChanges { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::Autonomy { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::Usage { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::BeginRewind { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::FinishRewind { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::RestoreSnapshot { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::StartGoal { done, .. }
        | CodingRuntimeControl::StopGoal { done, .. }
        | CodingRuntimeControl::StartLoop { done, .. }
        | CodingRuntimeControl::StopLoop { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
    }
}

/// Apply normal turn vision preprocessing to images attached while answering
/// `request_user_input`; tool responses otherwise bypass the submit boundary.
async fn preprocess_request_response(
    kind: &str,
    mut value: serde_json::Value,
    resources: Option<&RuntimeResources>,
    runtime_event_tx: &RuntimeEventEmitter,
) -> serde_json::Value {
    if kind != REQUEST_USER_INPUT_KIND {
        return value;
    }
    let Some(runtime) = resources else {
        return value;
    };
    // Native multimodal models must receive the original image bytes.
    if runtime.config.supports_vision {
        return value;
    }
    let Some(preprocessor) = runtime.image_preprocessor.clone() else {
        return value;
    };
    let session_id = runtime
        .parts
        .session
        .as_ref()
        .map(|binding| binding.id.clone());

    if let Some(responses) = value
        .get_mut("responses")
        .and_then(|item| item.as_array_mut())
    {
        let pending = std::mem::take(responses);
        *responses = futures::future::join_all(pending.into_iter().map(|response| {
            preprocess_one_user_input_response(
                response,
                preprocessor.as_ref(),
                session_id.clone(),
                runtime_event_tx,
            )
        }))
        .await;
    } else {
        value = preprocess_one_user_input_response(
            value,
            preprocessor.as_ref(),
            session_id,
            runtime_event_tx,
        )
        .await;
    }
    value
}

async fn preprocess_one_user_input_response(
    value: serde_json::Value,
    preprocessor: &dyn ImagePreprocessor,
    session_id: Option<String>,
    runtime_event_tx: &RuntimeEventEmitter,
) -> serde_json::Value {
    let Ok(mut response) = serde_json::from_value::<UserInputResponse>(value.clone()) else {
        return value;
    };
    if response.images.is_empty() {
        return value;
    }

    // Choice-mode answers keep their selected labels in `selected`; using them
    // as the VL caption would duplicate those labels in the formatted tool
    // result. Only free-form text belongs in the recognition prompt.
    let prompt = response.text.clone().unwrap_or_default();
    let (processed, notice) = preprocessor
        .preprocess(
            prompt,
            std::mem::take(&mut response.images),
            false,
            session_id,
        )
        .await;
    response.text = (!processed.text.is_empty()).then_some(processed.text);
    response.images = processed.images;
    match notice {
        Some(VisionNotice::Recognised {
            vl_model,
            char_count,
        }) => {
            let _ = runtime_event_tx.send(CodingRuntimeEvent::VisionPreprocessSuccess {
                vl_model,
                char_count,
            });
        }
        Some(VisionNotice::Failed { reason }) => {
            let _ = runtime_event_tx.send(CodingRuntimeEvent::VisionPreprocessFailed { reason });
        }
        None => {}
    }
    serde_json::to_value(response).unwrap_or(value)
}

fn fail_close_pending_requests(
    agent: &Option<AgentHandle>,
    pending_requests: &mut BTreeMap<RequestId, String>,
    cancel_turn: bool,
) {
    if let Some(agent) = agent.as_ref() {
        for id in pending_requests.keys().copied() {
            let _ = agent.commands.send(AgentCommand::Respond {
                id,
                value: serde_json::Value::Null,
            });
        }
        if cancel_turn {
            let _ = agent.commands.send(AgentCommand::Cancel);
        }
    }
    pending_requests.clear();
}

fn send_agent_command(agent: &Option<AgentHandle>, command: AgentCommand) -> bool {
    agent
        .as_ref()
        .is_some_and(|agent| agent.commands.send(command).is_ok())
}

fn request_cancel_snapshot(agent: &Option<AgentHandle>) -> bool {
    send_agent_command(agent, AgentCommand::Cancel)
        && send_agent_command(agent, AgentCommand::Snapshot)
}

#[allow(clippy::too_many_arguments)]
fn replay_pending_resume_prompt(
    agent: &Option<AgentHandle>,
    resources: Option<&mut RuntimeResources>,
    runtime_state: &AtomicU64,
    generation: u64,
    next_turn_id: &mut u64,
    active_turn: &mut Option<u64>,
    turn_stats: &mut RuntimeTurnStats,
    agent_available: &mut bool,
) {
    let Some(runtime) = resources else {
        return;
    };
    let Some(binding) = runtime.parts.session.as_mut() else {
        return;
    };
    let Some(prompt) = binding.pending_resume_prompt.as_ref() else {
        return;
    };
    *next_turn_id = next_turn_id.wrapping_add(1);
    *active_turn = Some(*next_turn_id);
    *turn_stats = RuntimeTurnStats::default();
    runtime_state.store(
        runtime_phase_state(generation, RuntimePhase::InTurn),
        Ordering::Release,
    );
    if send_agent_command(
        agent,
        AgentCommand::SendMessage {
            text: prompt.text.clone(),
            images: prompt.images.clone(),
        },
    ) {
        binding.pending_resume_prompt = None;
    } else {
        *agent_available = false;
        *active_turn = None;
        runtime_state.store(
            runtime_phase_state(generation, RuntimePhase::Failed),
            Ordering::Release,
        );
    }
}

async fn receive_agent_event(agent: &mut Option<AgentHandle>) -> Option<AgentEvent> {
    match agent.as_mut() {
        Some(agent) => agent.events.recv().await,
        None => std::future::pending().await,
    }
}

fn resolve_reprepare_input(
    runtime: &RuntimeResources,
    target: ReprepareTarget,
) -> Result<
    Option<(
        ReprepareInput,
        Option<SessionLease>,
        Option<tokio_util::sync::CancellationToken>,
    )>,
    RuntimeError,
> {
    match target {
        ReprepareTarget::Reload { plugin_skill_dirs } => {
            let mut prepare = runtime.prepare.clone();
            if let Some(plugin_skill_dirs) = plugin_skill_dirs {
                prepare.plugin_skill_dirs = plugin_skill_dirs;
            }
            prepare.session = match runtime.parts.session.as_ref() {
                Some(binding) => crate::SessionMode::Resume(binding.id.clone()),
                None => crate::SessionMode::Disabled,
            };
            Ok(Some((
                ReprepareInput {
                    config: runtime.config.clone(),
                    prepare,
                    operation: ReconfigureKind::Reprepare,
                },
                None,
                None,
            )))
        }
        ReprepareTarget::ReloadConfig(config) => {
            let mut prepare = runtime.prepare.clone();
            prepare.session = match runtime.parts.session.as_ref() {
                Some(binding) => crate::SessionMode::Resume(binding.id.clone()),
                None => crate::SessionMode::Disabled,
            };
            Ok(Some((
                ReprepareInput {
                    config,
                    prepare,
                    operation: ReconfigureKind::Reprepare,
                },
                None,
                None,
            )))
        }
        ReprepareTarget::Fresh => {
            let mut prepare = runtime.prepare.clone();
            prepare.session = crate::SessionMode::Fresh;
            Ok(Some((
                ReprepareInput {
                    config: runtime.config.clone(),
                    prepare,
                    operation: ReconfigureKind::FreshSession,
                },
                None,
                None,
            )))
        }
        ReprepareTarget::Resume(id) => {
            let mut prepare = runtime.prepare.clone();
            prepare.session = crate::SessionMode::Resume(id);
            Ok(Some((
                ReprepareInput {
                    config: runtime.config.clone(),
                    prepare,
                    operation: ReconfigureKind::ResumeSession,
                },
                None,
                None,
            )))
        }
        ReprepareTarget::ResumeWithLease {
            id,
            working_dir,
            lease,
            cancel,
        } => {
            if lease.id() != id {
                return Err(RuntimeError::ReconfigureFailed(format!(
                    "prepared session lease is for {:?}, not {:?}",
                    lease.id(),
                    id
                )));
            }
            if !working_dir.is_absolute() {
                return Err(RuntimeError::InvalidWorkingDirectory(format!(
                    "session working directory is not absolute: {}",
                    working_dir.display()
                )));
            }
            if !working_dir.is_dir() {
                return Err(RuntimeError::InvalidWorkingDirectory(format!(
                    "session working directory is not a directory: {}",
                    working_dir.display()
                )));
            }
            let mut config = runtime.config.clone();
            // Do not canonicalize here. The persisted project bucket is keyed by
            // this exact path spelling (`/var` and `/private/var` differ on macOS),
            // and the transferred lease below is the authority that validates it.
            config.working_dir = working_dir;
            let mut prepare = runtime.prepare.clone();
            prepare.session = crate::SessionMode::Resume(id);
            Ok(Some((
                ReprepareInput {
                    config,
                    prepare,
                    operation: ReconfigureKind::ResumeSession,
                },
                Some(lease),
                cancel,
            )))
        }
        ReprepareTarget::ChangeDirectory(directory) => {
            let target = if directory.is_absolute() {
                directory
            } else {
                runtime.config.working_dir.join(directory)
            };
            let canonical =
                atomcode_capabilities::pathnorm::canonicalize(&target).map_err(|e| {
                    RuntimeError::InvalidWorkingDirectory(format!(
                        "cannot change directory to {}: {e}",
                        target.display()
                    ))
                })?;
            if !canonical.is_dir() {
                return Err(RuntimeError::InvalidWorkingDirectory(format!(
                    "working directory is not a directory: {}",
                    canonical.display()
                )));
            }
            let current =
                atomcode_capabilities::pathnorm::canonicalize(&runtime.config.working_dir)
                    .unwrap_or_else(|_| runtime.config.working_dir.clone());
            if atomcode_capabilities::pathnorm::path_case_key(&canonical)
                == atomcode_capabilities::pathnorm::path_case_key(&current)
            {
                return Ok(None);
            }
            let mut config = runtime.config.clone();
            config.working_dir = canonical;
            let mut prepare = runtime.prepare.clone();
            prepare.session = crate::SessionMode::Fresh;
            Ok(Some((
                ReprepareInput {
                    config,
                    prepare,
                    operation: ReconfigureKind::ChangeDirectory,
                },
                None,
                None,
            )))
        }
    }
}

fn matching_session_lease(
    parts: &crate::CodingParts,
    target: &crate::SessionMode,
) -> Option<SessionLease> {
    let target_id = match target {
        crate::SessionMode::Resume(id) | crate::SessionMode::ExternalSnapshot { id, .. } => id,
        crate::SessionMode::Fresh | crate::SessionMode::Disabled => return None,
    };
    parts
        .session
        .as_ref()
        .filter(|binding| binding.id == *target_id)
        .map(|binding| binding.lease.clone())
}

fn discard_uncommitted_session(
    operation: ReconfigureKind,
    candidate: &RuntimeResources,
) -> Result<(), String> {
    if !matches!(
        operation,
        ReconfigureKind::FreshSession | ReconfigureKind::ChangeDirectory
    ) {
        return Ok(());
    }
    let Some(binding) = candidate.parts.session.as_ref() else {
        return Ok(());
    };
    binding.manager.delete(&binding.lease).map_err(|error| {
        format!(
            "failed to discard uncommitted session {}: {error}",
            binding.id
        )
    })
}

fn session_in_use_id(error: &io::Error) -> Option<String> {
    match error
        .get_ref()
        .and_then(|source| source.downcast_ref::<SessionStoreError>())
    {
        Some(SessionStoreError::SessionInUse { id, .. }) => Some(id.clone()),
        _ => None,
    }
}

fn runtime_start_prepare_error(error: io::Error) -> RuntimeStartError {
    match session_in_use_id(&error) {
        Some(id) => RuntimeStartError::SessionInUse { id },
        None => RuntimeStartError::Prepare(error),
    }
}

fn runtime_prepare_error(error: io::Error) -> RuntimeError {
    match session_in_use_id(&error) {
        Some(id) => RuntimeError::SessionInUse { id },
        None => RuntimeError::ReconfigureFailed(error.to_string()),
    }
}

fn build_goal_evaluator_provider(
    factory: &Arc<dyn CodingProviderFactory>,
    host: &CodingAgentConfig,
    session_id: Option<&str>,
) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
    if let Some(registry) = host.subagent_config.as_deref() {
        if let Some(key) = registry.evaluator_provider.as_deref() {
            // `evaluator_provider` is a model-selection id (a legacy provider name
            // still resolves via projection, §14.3). Resolve through the single
            // boundary so it works for both schemas.
            if let Ok(resolved) = registry.resolve_model(Some(key)) {
                let mut evaluator = host.clone();
                evaluator.provider_name = key.to_owned();
                evaluator.model = resolved.model.clone();
                evaluator.provider_type = resolved.provider_type.clone();
                evaluator.context_window = resolved.context_window as u32;
                evaluator.chat_options.max_tokens = resolved.max_tokens.map(|value| value as u32);
                evaluator.thinking_type = resolved.thinking_type.clone();
                evaluator.thinking_keep = resolved.thinking_keep.clone();
                evaluator.reasoning_history = resolved.reasoning_history.clone();
                evaluator.thinking_enabled = resolved.thinking_enabled;
                evaluator.user_agent = resolved.user_agent.clone();
                evaluator.skip_tls_verify = resolved.skip_tls_verify;
                evaluator.subagent_fast_provider = None;
                evaluator.subagent_capable_provider = None;
                evaluator.subagent_model_providers = None;
                evaluator.subagent_config = None;
                // An evaluator is an independent provider boundary. Never let a
                // missing target credential/endpoint inherit the host provider's
                // values: that could send the host API key to another base URL.
                evaluator.api_key = resolved.api_key.clone().unwrap_or_default();
                evaluator.base_url = resolved.base_url.clone().unwrap_or_default();
                if let Ok(provider) = factory.build(&evaluator, session_id) {
                    return Ok(provider);
                }
            }
        }
    }

    factory.build(host, session_id)
}

/// What a tree needs from the runtime's own state to continue this session.
///
/// The same two sources the chain's `assemble` reads: a session-bound runtime
/// reloads the canonical native aggregate (absent only for a fresh session not
/// yet published), and a sessionless one continues from the snapshot the
/// runtime kept in memory.
fn harness_host_state(
    parts: &crate::CodingParts,
    config: &CodingAgentConfig,
    prepare: &PrepareOptions,
) -> Result<crate::on_harness::HostState, std::io::Error> {
    // The system prompt's context block as the session started with it.
    let (session, stored_prompt) = match &parts.session {
        Some(binding) => {
            // A fresh session that has not been published yet has nothing on
            // disk, and that is the ONE reason its log may be missing here. Any
            // other absence is half a session, and continuing on half a session
            // hands the model a conversation the store cannot explain — the
            // person's history, silently gone.
            let (resume, context) = match binding.staged_header() {
                Some(header) => (false, header.context.clone()),
                None => (
                    true,
                    binding
                        .manager
                        .read_event_header(&binding.id)
                        .map_err(std::io::Error::from)?
                        .context,
                ),
            };
            (
                crate::host_rows::SessionSeed {
                    id: Some(binding.id.clone()),
                    snapshot: None,
                    stored: Some(crate::session_store::StoredSession {
                        store: binding.manager.clone(),
                        lease: binding.lease.clone(),
                        status: parts.snapshot_persistence_status(),
                    }),
                    resume,
                },
                context,
            )
        }
        None => {
            let snapshot = parts.runtime_resume_snapshot();
            let prompt = snapshot
                .as_ref()
                .and_then(atomcode_capabilities::session::events::stored_prompt);
            (
                crate::host_rows::SessionSeed {
                    id: None,
                    snapshot,
                    stored: None,
                    resume: false,
                },
                prompt,
            )
        }
    };
    let session_id = parts.session.as_ref().map(|binding| binding.id.as_str());

    // Everything the chain's `prepare` + `assemble` hang on the kernel agent that
    // the rows do not already carry, as the same objects.
    let hooks = crate::host_rows::HostHooks::new();
    // The current-date tail: a per-turn `<system-reminder>` carrying today's date, appended
    // AFTER the cached prefix. It is the SOLE date source — the persona no longer bakes a
    // wall-clock date into the system prompt, because a date at the FRONT of the request
    // re-prefills the whole cached prefix once per day (the `project_system_prompt_date`
    // cache-poison bug). Unconditional: every session needs the model to know the date.
    hooks.insert(
        "status-reminder",
        Arc::new(atomcode_capabilities::session::StatusReminderHook::new()),
    );
    if let Some(snapshot) = parts.snapshot_hook() {
        hooks.insert("native-snapshot", snapshot);
    }
    let mcp = parts.mcp_publication();
    let middleware = crate::host_rows::HostMiddleware::new();
    if let Some(telemetry) = &config.telemetry {
        // One per mounted tree, shared by the two adapters: the hook is the only
        // one the kernel tells where it is, and the tool middleware reads it
        // from here (`telemetry::Position`).
        let position = Arc::new(crate::telemetry::Position::default());
        hooks.insert(
            "telemetry",
            Arc::new(crate::telemetry::TelemetryHook::new(
                telemetry.clone(),
                config.provider_type.as_str(),
                &config.base_url,
                &config.model,
                session_id,
                Arc::clone(&position),
            )),
        );
        middleware.insert(
            "tool-telemetry",
            Arc::new(crate::telemetry::ToolTelemetryMiddleware::new(
                telemetry.clone(),
                config.provider_type.as_str(),
                &config.base_url,
                &config.model,
                session_id,
                position,
            )),
        );
    }
    if parts.todo_enabled() {
        hooks.insert(
            "todo-eager",
            Arc::new(
                crate::todo::TodoEagerHook::new(
                    &config.model,
                    &config.provider_type,
                    config.todo.eager,
                )
                .with_working_dir(config.working_dir.clone()),
            ),
        );
        // Everything the model is told about its list: the list and where it
        // stands on EVERY request, one note when it has gone quiet for a few
        // steps, the one-shot nudge when the model stops with items still open,
        // and the `<id>.todos.json` sidecar that lets a compacted session still
        // show its plan (issue #1503 — the transcript's `todowrite` calls are
        // gone by then, and vscode's fallback derives from exactly those).
        //
        // The harness's `todo-reminder` row says the quiet-list note too, so
        // `CODING_ROWS` keeps it off: one voice, and none of it in the log.
        hooks.insert(
            "todo",
            Arc::new(crate::todo::TodoHook::new(config.working_dir.clone())),
        );
    }
    // The person's `[permissions]` rules decide among the gates, and WHERE they
    // decide is the whole contract: after every hard boundary, before the
    // convenience gates and the approval prompt. The `permissions` row states
    // that position (`CODING_ROWS`), so this only fills it in — no `prepend`,
    // which would hoist the gate ahead of the boundaries, and
    // `insert_mounted_by_row` so the middleware table does not ALSO append an
    // innermost copy. Criteria: `a_permission_allow_rule_cannot_unlock_the_credential_boundary`
    // and `a_permission_allow_rule_still_skips_the_prompt_it_covers` fail in
    // opposite directions if this moves either way.
    let permission_rows = if config.permission_rules.is_empty() {
        atomcode_plexus::Layer::new()
    } else {
        middleware.insert_mounted_by_row(
            "permission-rules",
            Arc::new(atomcode_capabilities::tools::PermissionRuleGate::new(
                config.permission_rules.clone(),
                parts.shared_cwd.clone(),
            )),
        );
        atomcode_plexus::Layer::new()
            .swap("permissions", "kernel-middleware")
            .patch(
                "permissions",
                KernelMiddlewarePatch {
                    middleware: "permission-rules",
                    prepend: true,
                },
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?
    };
    #[cfg(feature = "atomgit")]
    middleware.insert(
        "git-push-label",
        Arc::new(atomcode_capabilities::tools::GitPushLabelMiddleware::new(
            config.working_dir.clone(),
        )),
    );
    Ok(crate::on_harness::HostState {
        // None: this runtime mounts the product's own rows and nothing else.
        // The field is for a host outside this workspace that brings its own.
        plugins: Vec::new(),
        session_context: Some(crate::on_harness::HostContext {
            hook: Arc::new(atomcode_capabilities::session::SessionContextHook::new(
                &config.working_dir,
            )),
            stored: stored_prompt,
        }),
        session,
        hooks: Some(hooks),
        middleware: Some(middleware),
        cc_hooks: parts.cc_external_hooks.clone(),
        tools: parts
            .extra_tools()
            .into_iter()
            .chain(parts.host_only_tools())
            .collect(),
        skills: parts.skill_registry(),
        runtime_commands: parts.runtime_commands.clone(),
        tool_switches: Some(parts.tool_switches()),
        tool_catalog_slot: parts.tool_catalog_slot(),
        mcp,
        // Both halves or neither: the events only exist when this assembly has
        // MCP, and `prepare` only started the fan-out when there was a sink.
        mcp_telemetry: parts.mcp_connect_meter().zip(config.telemetry.clone()).map(
            |(events, telemetry)| crate::host_rows::McpConnectMeter {
                events,
                meter: crate::telemetry::McpTelemetry::new(telemetry, config.working_dir.clone()),
            },
        ),
        rate_limit_source: parts.rate_limit_source().cloned(),
        front_end: prepare.front_end.clone(),
        delegated_llm: parts.delegated_provider(),
        team_events: parts.subagent_knobs().map(|_| {
            let manager = parts.team_manager.clone();
            Arc::new(move |event| manager.publish_external(event)) as crate::team_progress::TeamSink
        }),
        compaction_checkpoint: parts.snapshot_hook(),
        summary_provider: Some(parts.side_provider_slot()),
        model: Some(config.model.clone()),
        // `from_config` is the one constructor that carries the parsed file along;
        // a runtime built from a hand-made config was configured by no file.
        config_file: config
            .subagent_config
            .is_some()
            .then(atomcode_config::Config::default_path),
        // The UI language, for the rows that write words of their own (the
        // `/worklog` and `/init` templates). The persona does not read it: what
        // the model answers and commits in follows the conversation.
        language: config.preferred_language,
        web_search_api_key: config.web_search_api_key.clone(),
        rows: harness_option_rows(parts, config, prepare)
            .map_err(std::io::Error::other)?
            .then(permission_rows),
        datalog: config.datalog.enabled.then(|| config.datalog.clone()),
        modes: Some(crate::on_harness::HostModes {
            modes: atomcode_harness::seams::Modes {
                plan: Arc::clone(&parts.plan_mode),
                accept_edits: Arc::clone(&parts.accept_edits),
            },
            plan_mcp_grants: Arc::clone(&parts.mcp_plan_grants),
            approval_grants: parts.approval.store(),
        }),
    })
}

/// A mounted product tree: the handle a driver speaks to, the tree behind it,
/// and the table its provider lives in.
///
/// The tree must outlive the handle. Dropping the `App` unloads every row, and
/// the next command would reach a conversation whose services are gone.
pub struct Mounted {
    pub handle: AgentHandle,
    pub app: atomcode_plexus::App,
    pub providers: Arc<crate::on_harness::ProviderSlots>,
}

/// Mount the product as a tree, continuing `parts`' session.
///
/// The one place a tree is built from prepared parts — the runtime does it at
/// start and on every rebuild (undo, restore, reprepare, a provider coming
/// back), so the session, the hooks and the rows cannot differ between the first
/// agent and the next. Public because that assembly IS the product: a test or an
/// embedder that wants what ships asks for it here rather than rebuilding a
/// second version of it.
pub async fn mount(
    parts: &crate::CodingParts,
    config: &CodingAgentConfig,
    prepare: &PrepareOptions,
    provider: Arc<dyn atomcode_kernel::provider::LlmProvider>,
) -> Result<Mounted, String> {
    // Presence follows the same rule the overlay uses: a person who can answer
    // means asking, nobody means fencing.
    let presence = if config.is_attended() {
        crate::on_harness::Presence::Attended
    } else {
        crate::on_harness::Presence::Headless
    };
    // What the person wrote in `config.toml`, as a layer of its own.
    let extra = vec![crate::on_harness::config_rows(config)?];
    // The model catalog, when this host has one. Both halves come from what
    // `install_subagent_tiers` already put on the config, so the tree and the
    // chain resolve a selection through the same resolver — including its reset
    // on `/model`.
    let models = config
        .subagent_config
        .clone()
        .zip(config.subagent_model_providers.clone())
        .map(|(model_config, providers)| crate::on_harness::HostModels {
            config: model_config,
            providers,
            current: config.provider_name.clone(),
        });
    // The capability graph's own sub-agents — the reviewer, `task`, `team` — run on
    // this model, billed to the session and metered the way the chain's
    // `assemble` wires them.
    let _ = crate::parts::wire_side_providers(parts, config, &provider);
    let host = harness_host_state(parts, config, prepare).map_err(|error| error.to_string())?;
    // Turns on this tree are billed to the model it was built for. The chain
    // stamps the same attribution inside `assemble`.
    if let Some(snapshot) = parts.snapshot_hook() {
        snapshot.set_model_attribution(&config.provider_name, &config.model);
    }
    let (handle, app, providers) = crate::on_harness::mount_hosted(
        &config.working_dir,
        presence,
        provider,
        models,
        host,
        &extra,
    )
    .await?;
    Ok(Mounted {
        handle,
        app,
        providers,
    })
}

/// Build the agent for `config`, replacing whatever tree the runtime held.
///
/// Replacing is the point: a rebuilt agent that left the previous tree in
/// `harness_app` would have a later `/model` patch a tree whose driver loop had
/// already exited, and a later `/logout` leave the credentials in the agent that
/// is actually running.
async fn build_agent(
    runtime: &mut RuntimeResources,
    config: &CodingAgentConfig,
    provider: Arc<dyn atomcode_kernel::provider::LlmProvider>,
) -> Result<AgentHandle, String> {
    let mounted = mount(&runtime.parts, config, &runtime.prepare, provider).await?;
    runtime.harness_app = Some(mounted.app);
    runtime.harness_providers = Some(mounted.providers);
    Ok(mounted.handle)
}

/// Row edits for what this runtime's options switched off, and for the
/// directory rows resolve against.
///
/// Read off what prepare DECIDED (a review provider exists or not, the todo
/// switch after its environment override) rather than re-deriving it from the
/// options, so the tree and the capability graph cannot disagree about whether a
/// capability is on.
#[derive(serde::Serialize)]
struct KernelMiddlewarePatch<'a> {
    middleware: &'a str,
    prepend: bool,
}

#[derive(serde::Serialize)]
struct MemoryPatch<'a> {
    project_root: &'a std::path::Path,
    inject: bool,
}

#[derive(serde::Serialize)]
struct OutputArtifactPatch {
    dir: std::path::PathBuf,
    threshold_bytes: usize,
}

#[derive(serde::Serialize)]
struct SubagentRowPatch {
    max_rounds: u32,
}

#[derive(serde::Serialize)]
struct TeamRowPatch<'a> {
    project_root: &'a std::path::Path,
    max_members: usize,
    max_rounds: u32,
}

#[derive(serde::Serialize)]
struct AgentLoopOptionsPatch<'a> {
    working_dir: &'a std::path::Path,
    undo_cancelled: bool,
    stream_idle_ms: u128,
    /// Carried, not left to the row's default: this patch is the last word on
    /// `agent-loop` in this host, so omitting it would *be* the fuse value — the
    /// serde default in `harness/plugins/agent_loop.rs`. See
    /// [`crate::on_harness::RUNAWAY_FUSE_ROUNDS`].
    max_rounds: u32,
}

fn harness_option_rows(
    parts: &crate::CodingParts,
    config: &CodingAgentConfig,
    prepare: &PrepareOptions,
) -> Result<atomcode_plexus::Layer, String> {
    let wd = config.working_dir.as_path();
    let mut rows = atomcode_plexus::Layer::new()
        .when(!prepare.tools, |layer| layer.disable("memory"))
        .when(!prepare.tools || !prepare.web, |layer| {
            layer.disable("tool-web")
        })
        // The runtime mounts its own `code_review` and `recall` (see
        // `CodingParts::host_only_tools`) whenever prepare built them; the rows'
        // versions are different contracts under the same names. Delegation is
        // the tree's own rows, off when the driver turned it off.
        .disable("tool-code-review")
        .when(parts.subagent_knobs().is_none(), |layer| {
            layer
                .disable("subagent-in-process")
                .disable("team-in-process")
        })
        .disable("recall")
        .when(!parts.todo_enabled(), |layer| layer.disable("tool-todo"))
        // The runtime's own `request_user_input` asks the person, when it is on;
        // the tree's `ask_user` is a narrower contract for the same capability,
        // and two question tools is one too many either way.
        .disable("tool-ask");

    // Rows whose directory defaults to the process's cwd, pointed at this
    // session's working directory instead. A `[[patch]]` replaces a row's whole
    // config, so each carries every field the row is given elsewhere.
    //
    // The chain's `memory` switch is the injection alone; the `memory` tool is
    // one of the core tools and stays either way.
    if prepare.tools {
        rows = rows
            .patch(
                "memory",
                MemoryPatch {
                    project_root: wd,
                    inject: prepare.memory,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    // `[tools.output] threshold_bytes`: where an oversized tool result is cut.
    // Patched only when set, so an unconfigured tree mounts the row as written.
    if let Some(threshold_bytes) = config.tool_output_threshold_bytes {
        rows = rows
            .patch(
                "tool-output-artifact",
                OutputArtifactPatch {
                    dir: crate::on_harness::artifacts_dir(wd),
                    threshold_bytes,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    // `[subagent]`: how long a delegated agent may run, and how many members a
    // team may hold; roles come from this project and the person's home.
    if let Some((max_concurrent, max_rounds)) = parts.subagent_knobs() {
        rows = rows
            .patch("subagent-in-process", SubagentRowPatch { max_rounds })
            .map_err(|e| e.to_string())?
            .patch(
                "team-in-process",
                TeamRowPatch {
                    project_root: wd,
                    max_members: max_concurrent,
                    max_rounds,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    // Ctrl-C semantics: by default a cancelled turn is undone — its prompt and
    // partial work leave what the model sees next, as the chain rolls them back.
    rows.patch(
        "agent-loop",
        AgentLoopOptionsPatch {
            working_dir: wd,
            undo_cancelled: !config.keep_interrupted_context,
            stream_idle_ms: config.stream_timeout.as_millis(),
            max_rounds: crate::on_harness::RUNAWAY_FUSE_ROUNDS,
        },
    )
    .map_err(|e| e.to_string())
}

async fn assemble_runtime_resources(runtime: &mut RuntimeResources) -> Result<AgentHandle, String> {
    runtime
        .parts
        .register_extra_tool(Arc::new(ScheduleWakeupTool::new(
            runtime.wakeup_tx.clone(),
            Arc::clone(&runtime.loop_active),
        )));
    let session_id = runtime
        .parts
        .session
        .as_ref()
        .map(|binding| binding.id.as_str());
    let provider = runtime
        .provider_factory
        .build(&runtime.config, session_id)
        .map_err(|error| error.to_string())?;
    let config = runtime.config.clone();
    build_agent(runtime, &config, provider).await
}

fn preserve_sessionless_snapshot(runtime: &mut RuntimeResources, report: &StopReport) {
    if runtime.parts.session.is_none() {
        if let Some(snapshot) = report.snapshot.clone() {
            runtime.parts.set_runtime_resume(snapshot);
        }
    }
}

fn session_changed(generation: u64, runtime: &RuntimeResources) -> SessionChanged {
    SessionChanged {
        generation: RuntimeGeneration(generation),
        session_id: runtime
            .parts
            .session
            .as_ref()
            .map(|binding| binding.id.clone()),
        working_dir: runtime.config.working_dir.clone(),
    }
}

struct NativeUndoSidecars {
    message_count: u32,
    turn_count: u32,
    turn_stats: Vec<TurnStat>,
    archived_turn_stats: Vec<TurnStat>,
    removed_presentation: Vec<(usize, PresentationEntry)>,
    /// Where the session's log stood before the change was appended: what a
    /// rollback cuts it back to.
    events_mark: Option<u64>,
}

#[derive(Clone, Debug)]
struct NativePersistenceError {
    message: String,
    uncertain_commit: bool,
    snapshot_conflict: bool,
}

impl NativePersistenceError {
    fn certain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain_commit: false,
            snapshot_conflict: false,
        }
    }

    fn snapshot_conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain_commit: false,
            snapshot_conflict: true,
        }
    }

    fn uncertain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain_commit: true,
            snapshot_conflict: false,
        }
    }

    fn requires_fail_close(&self) -> bool {
        self.uncertain_commit
    }

    fn is_snapshot_conflict(&self) -> bool {
        self.snapshot_conflict
    }
}

impl From<SessionStoreError> for NativePersistenceError {
    fn from(error: SessionStoreError) -> Self {
        let uncertain_commit = error.is_uncertain_commit();
        Self {
            message: error.to_string(),
            uncertain_commit,
            snapshot_conflict: false,
        }
    }
}

impl std::fmt::Display for NativePersistenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn persistence_fail_close_reason(
    candidate_error: &NativePersistenceError,
    restore_error: Option<&NativePersistenceError>,
) -> Option<String> {
    if candidate_error.requires_fail_close() {
        Some(candidate_error.to_string())
    } else {
        restore_error.map(|error| format!("snapshot rollback persistence failed: {error}"))
    }
}

/// Record that the workspace went back to before `turn` (`docs/adr/0024` §17).
///
/// The conversation's own `Rewound` says what the model no longer sees; this one
/// says what the working tree no longer holds, and the projection leaves the
/// conversation alone.
fn record_code_rewind(live: &atomcode_harness::agent::Agent, turn: u64) {
    use atomcode_harness::session::SessionEvent;
    let log = live.session();
    let Some(to) = log.events().iter().find_map(|logged| {
        matches!(logged.event, SessionEvent::TurnStart { turn: t } if t == turn)
            .then_some(logged.seq)
    }) else {
        return;
    };
    atomcode_harness::session::commit(
        live.ctx(),
        &log,
        SessionEvent::Rewound {
            turn: log.current_turn(),
            to,
            scope: atomcode_harness::session::RewindScope::Code,
        },
    );
}

/// Read the skills on disk again, into the registry the live tree is already
/// serving, and re-render the catalog the model is told about
/// (`docs/adr/0022` §2).
///
/// This is a reload *without* a rebuild: every holder of the registry — the
/// `use_skill` and `list_skills` tools, the `skills` seam, the slash menu — has
/// an `Arc` to the one this replaces the contents of, and the prompt fragment is
/// re-contributed under the same id, which replaces it. A remount would have
/// taken the whole tree with it, MCP connections and all.
///
/// `Err` when the tree has no skills row: there is nothing to re-read into, and
/// the caller falls back to the rebuild rather than reporting a reload that
/// reached nothing.
fn reload_skills_live(runtime: &RuntimeResources) -> Result<usize, ()> {
    let app = runtime.harness_app.as_ref().ok_or(())?;
    let ctx = app.context();
    let registry = ctx
        .service::<atomcode_harness::seams::SkillsSvc>()
        .ok_or(())?;
    // The directories prepare decided on, the same way it decided them: a
    // driver that named its own is not second-guessed here.
    let dirs = match runtime.prepare.skill_dirs.clone() {
        Some(dirs) => dirs,
        None => {
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            atomcode_capabilities::skills::runtime_skill_dirs(&home, &runtime.config.working_dir)
        }
    };
    registry.reload_dirs(&dirs, &runtime.prepare.plugin_skill_dirs);
    // The catalog is ranked against the project's own instruction files, as at
    // mount — a reload that dropped the ranking would quietly reorder the
    // prompt prefix.
    let instructions =
        atomcode_capabilities::session::SessionContextHook::new(&runtime.config.working_dir)
            .instruction_text();
    if let Some(prompts) = ctx.service::<atomcode_harness::seams::SystemPromptSvc>() {
        let (id, rank) = crate::on_harness::SKILLS_FRAGMENT;
        match registry.render_catalog_prioritizing(&instructions) {
            Some(catalog) if !catalog.trim().is_empty() => prompts.contribute(id, rank, catalog),
            // Every skill is gone: so is what said they were there.
            _ => prompts.remove(id),
        }
    }
    Ok(registry.len())
}

#[async_trait::async_trait]
impl RuntimeCommands for CodingRuntimeHandle {
    async fn start_goal(&self, condition: String) -> Result<(), String> {
        CodingRuntimeHandle::start_goal(self, condition)
            .await
            .map_err(|error| error.to_string())
    }
    async fn stop_goal(&self) -> Result<(), String> {
        CodingRuntimeHandle::stop_goal(self)
            .await
            .map_err(|error| error.to_string())
    }
    async fn pause_goal(&self) -> Result<(), String> {
        CodingRuntimeHandle::pause_goal(self)
            .await
            .map_err(|error| error.to_string())
    }
    async fn start_loop(&self, prompt: String, every: Option<u32>) -> Result<(), String> {
        CodingRuntimeHandle::start_loop(self, prompt, every)
            .await
            .map_err(|error| error.to_string())
    }
    async fn stop_loop(&self) -> Result<(), String> {
        CodingRuntimeHandle::stop_loop(self)
            .await
            .map_err(|error| error.to_string())
    }
    async fn queue_local_context(&self, text: String) -> Result<(), String> {
        CodingRuntimeHandle::queue_local_context(self, LocalContextInput { content: text })
            .await
            .map_err(|error| error.to_string())
    }
    async fn pending_policy(&self) -> Option<PolicyIntervention> {
        self.pending_policy_intervention().await
    }
    async fn resolve_policy(&self, id: u64, action: PolicyRecoveryAction) -> Result<(), String> {
        self.resolve_policy_intervention(id, action)
            .await
            .map_err(|error| error.to_string())
    }
    async fn change_directory(&self, directory: std::path::PathBuf) -> Result<(), String> {
        CodingRuntimeHandle::change_directory(self, directory)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Whether the change from `live`'s log to `target` is one the log can *say* —
/// an undo or a compaction — rather than a conversation reseeded from outside
/// it.
///
/// A reseed appends the target's own messages as facts, and an unanswered
/// prompt among them is a prompt the live agent answers: committing one would
/// make a *restore* send something to the model. So a candidate the log cannot
/// say goes the other way, through a rebuilt runtime, which is what a
/// conversation that did not come out of this log always needed.
fn live_can_say(live: &atomcode_harness::agent::Agent, target: &[Message]) -> bool {
    use atomcode_harness::session::SessionEvent;
    atomcode_capabilities::session::events::events_to_become(&live.session().events(), target)
        .iter()
        .all(|event| {
            matches!(
                event,
                SessionEvent::Rewound { .. } | SessionEvent::Compacted { .. }
            )
        })
}

/// The conversation's own agent, live in the mounted tree — what a change to
/// the same session is committed into rather than rebuilt around
/// (`docs/adr/0022` §2).
fn live_root_agent(runtime: &RuntimeResources) -> Option<Arc<atomcode_harness::agent::Agent>> {
    runtime
        .harness_app
        .as_ref()?
        .context()
        .service::<atomcode_harness::seams::AgentsSvc>()?
        .list()
        .into_iter()
        .find(|agent| agent.parent().is_none())
}

/// Tell the conversation's agent what the person just did (see [`crate::told`]).
///
/// `note` puts it where it happened — into the log at once when idle, ahead of
/// the next turn when one is running — as a logged fact, so a resumed session
/// still says it. `None` is a change that says nothing.
fn tell(runtime: &RuntimeResources, said: Option<String>) {
    let Some(said) = said else {
        return;
    };
    if let Some(agent) = live_root_agent(runtime) {
        agent.note(said, atomcode_harness::session::InjectionOrigin::Reminder);
    }
}

/// The execution mode, decoded from the three flags `SetMode` writes — not a
/// fourth field that would have to be kept in step with them. Plan wins when two
/// are set, which cannot happen through `SetMode` (it writes all three) but is
/// the safe reading of a tree where it did.
fn current_mode(parts: &crate::CodingParts) -> RuntimeMode {
    if parts.plan_mode.load(Ordering::Acquire) {
        RuntimeMode::Plan
    } else if parts.bypass_mode.load(Ordering::Acquire) {
        RuntimeMode::Auto
    } else if parts.accept_edits.load(Ordering::Acquire) {
        RuntimeMode::AcceptEdits
    } else {
        RuntimeMode::Build
    }
}

/// Commit `events` into `live`'s log, in order. The session store appends each
/// as it is committed; one it could not keep is reported as an uncertain commit.
fn commit_into_live_log(
    runtime: &RuntimeResources,
    live: &atomcode_harness::agent::Agent,
    events: impl IntoIterator<Item = atomcode_harness::session::SessionEvent>,
) -> Result<(), NativePersistenceError> {
    let log = live.session();
    for event in events {
        atomcode_harness::session::commit(live.ctx(), &log, event);
    }
    match runtime.parts.take_snapshot_persistence_uncertain() {
        Some(message) => Err(NativePersistenceError::uncertain(message)),
        None => Ok(()),
    }
}

/// Take the stored conversation to `snapshot`, with the facts that make its
/// projection that (`docs/adr/0024` §17). With a `live` agent they are committed
/// into its log — every subscriber hears them and nothing is rebuilt; without
/// one they are appended to the store for a rebuilt tree to replay.
fn persist_runtime_undo(
    runtime: &mut RuntimeResources,
    expected_snapshot: Option<&SessionSnapshot>,
    snapshot: &SessionSnapshot,
    live: Option<&Arc<atomcode_harness::agent::Agent>>,
) -> Result<Option<NativeUndoSidecars>, NativePersistenceError> {
    let Some(binding) = runtime.parts.session.as_ref() else {
        runtime.parts.set_runtime_resume(snapshot.clone());
        if let Some(live) = live {
            let change = atomcode_capabilities::session::events::events_to_become(
                &live.session().events(),
                &snapshot.messages,
            );
            commit_into_live_log(runtime, live, change)?;
        }
        return Ok(None);
    };
    let mut live_change: Vec<atomcode_harness::session::SessionEvent> = Vec::new();
    let message_count = u32::try_from(snapshot.messages.len()).map_err(|_| {
        NativePersistenceError::certain("snapshot message count exceeds native metadata")
    })?;
    let mut snapshot_conflict = false;
    let events = binding.manager.is_event_session(&binding.id);
    let sidecars = binding
        .manager
        .commit_native_runtime_mutation(
            &binding.lease,
            snapshot,
            |current_snapshot, meta, presentation| {
                if expected_snapshot.is_some_and(|expected| {
                    !atomcode_capabilities::session::events::same_conversation(
                        &current_snapshot.messages,
                        &expected.messages,
                    )
                }) {
                    snapshot_conflict = true;
                    return Err(SessionStoreError::Corrupt {
                        kind: "session mutation conflict",
                        message: "canonical snapshot changed before undo commit".into(),
                    });
                }
                let mut sidecars = NativeUndoSidecars {
                    message_count: meta.message_count,
                    turn_count: meta.turn_count,
                    turn_stats: meta.turn_stats.clone(),
                    archived_turn_stats: Vec::new(),
                    removed_presentation: Vec::new(),
                    events_mark: None,
                };
                // A log session's change is planned first, so the turns it
                // leaves standing decide which statistics go; it is appended
                // last, so a failure before that leaves nothing to undo.
                let plan = events
                    .then(|| {
                        binding.manager.plan_conversation_change(
                            &binding.id,
                            &snapshot.messages,
                            u64::try_from(atomcode_capabilities::session::now_ms()).unwrap_or(0),
                        )
                    })
                    .transpose()?;
                let visible = plan.as_ref().map(|plan| plan.visible_turns());
                sidecars.archived_turn_stats =
                    meta.archive_turn_stats_where(|stat| match &visible {
                        Some(visible) => {
                            stat.position_valid
                                && stat.turn_id != 0
                                && !visible.contains(&stat.turn_id)
                        }
                        None => stat.position_valid && stat.after_message > snapshot.messages.len(),
                    });
                let surviving_turn_ids: BTreeSet<_> = meta
                    .turn_stats
                    .iter()
                    .filter_map(|stat| {
                        (stat.position_valid && stat.turn_id != 0).then_some(stat.turn_id)
                    })
                    .collect();
                let original_entries = std::mem::take(&mut presentation.entries);
                for (index, entry) in original_entries.into_iter().enumerate() {
                    let keep = match entry.anchor {
                        DisplayAnchor::AtStart => true,
                        DisplayAnchor::AfterTurn { turn_id } => {
                            surviving_turn_ids.contains(&turn_id)
                        }
                    };
                    if keep {
                        presentation.entries.push(entry);
                    } else {
                        sidecars.removed_presentation.push((index, entry));
                    }
                }
                meta.message_count = message_count;
                meta.turn_count = u32::try_from(meta.turn_stats.len()).map_err(|_| {
                    SessionStoreError::Corrupt {
                        kind: "session mutation",
                        message: "turn count exceeds native metadata".into(),
                    }
                })?;
                meta.updated_at = atomcode_capabilities::session::now_ms();
                if let Some(plan) = plan {
                    if live.is_some() {
                        live_change = plan
                            .change
                            .iter()
                            .map(|logged| logged.event.clone())
                            .collect();
                    } else {
                        binding
                            .manager
                            .append_events(&binding.lease, &plan.change)?;
                    }
                    sidecars.events_mark = Some(plan.mark);
                }
                Ok(sidecars)
            },
        )
        .map_err(|error| {
            if snapshot_conflict {
                NativePersistenceError::snapshot_conflict(error.to_string())
            } else {
                NativePersistenceError::from(error)
            }
        })?;
    if let Some(live) = live {
        commit_into_live_log(runtime, live, live_change)?;
    }
    Ok(Some(sidecars))
}

fn restore_runtime_undo(
    runtime: &mut RuntimeResources,
    expected_current_snapshot: &SessionSnapshot,
    snapshot: &SessionSnapshot,
    sidecars: Option<NativeUndoSidecars>,
) -> Result<(), NativePersistenceError> {
    let Some(sidecars) = sidecars else {
        return persist_runtime_snapshot(runtime, snapshot);
    };
    let binding = runtime.parts.session.as_ref().ok_or_else(|| {
        NativePersistenceError::certain("native undo rollback lost its session binding")
    })?;
    let NativeUndoSidecars {
        message_count,
        turn_count,
        turn_stats,
        archived_turn_stats,
        removed_presentation,
        events_mark,
    } = sidecars;
    let mut snapshot_conflict = false;
    binding
        .manager
        .commit_native_runtime_mutation(
            &binding.lease,
            snapshot,
            |current_snapshot, meta, presentation| {
                if !atomcode_capabilities::session::events::same_conversation(
                    &current_snapshot.messages,
                    &expected_current_snapshot.messages,
                ) {
                    snapshot_conflict = true;
                    return Err(SessionStoreError::Corrupt {
                        kind: "session mutation conflict",
                        message: "canonical snapshot changed before undo rollback".into(),
                    });
                }
                meta.message_count = message_count;
                meta.turn_count = turn_count;
                meta.remove_archived_turn_usage(&archived_turn_stats);
                meta.turn_stats = turn_stats;
                for (original_index, entry) in removed_presentation {
                    presentation
                        .entries
                        .insert(original_index.min(presentation.entries.len()), entry);
                }
                meta.updated_at = atomcode_capabilities::session::now_ms();
                // Nothing has read the change since it was appended: no agent
                // ran on it, so it is cut back rather than answered with more.
                if let Some(mark) = events_mark {
                    binding.manager.truncate_events(&binding.lease, mark)?;
                }
                Ok(())
            },
        )
        .map_err(|error| {
            if snapshot_conflict {
                NativePersistenceError::snapshot_conflict(error.to_string())
            } else {
                NativePersistenceError::from(error)
            }
        })
}

fn persist_runtime_snapshot(
    runtime: &mut RuntimeResources,
    snapshot: &SessionSnapshot,
) -> Result<(), NativePersistenceError> {
    if let Some(binding) = runtime.parts.session.as_ref() {
        let events = binding.manager.is_event_session(&binding.id);
        binding
            .manager
            .commit_native_runtime_mutation(
                &binding.lease,
                snapshot,
                |_current_snapshot, _meta, _presentation| {
                    if events {
                        binding.manager.append_conversation_change(
                            &binding.lease,
                            &snapshot.messages,
                            u64::try_from(atomcode_capabilities::session::now_ms()).unwrap_or(0),
                        )?;
                    }
                    Ok(())
                },
            )
            .map_err(NativePersistenceError::from)
    } else {
        runtime.parts.set_runtime_resume(snapshot.clone());
        Ok(())
    }
}

fn current_runtime_snapshot(runtime: &RuntimeResources) -> Option<SessionSnapshot> {
    let binding = runtime.parts.session.as_ref()?;
    binding.manager.load_snapshot(&binding.id).ok()
}

async fn native_session_health_error(runtime: &RuntimeResources) -> Option<String> {
    // Extract cheap owned handles (Arc + String) and END the borrow before awaiting.
    let (manager, id) = {
        let binding = runtime.parts.session.as_ref()?;
        (binding.manager.clone(), binding.id.clone())
    };
    // `load_native_session` takes an OS meta-lock and can `thread::sleep`-poll for up to
    // 10s under cross-process contention (session/manager `acquire_file_lock_until`). Run
    // it on the BLOCKING pool so it never stalls this tokio worker — and every other task
    // scheduled on it — during the fail-close error path.
    match tokio::task::spawn_blocking(move || {
        manager
            .load_native_session(&id)
            .err()
            .map(|error| error.to_string())
    })
    .await
    {
        Ok(health) => health,
        // The probe panicked on the blocking pool — treat as a health failure so the
        // caller fail-closes rather than proceeding past an unverified session.
        Err(_join) => Some("native session health probe panicked".to_string()),
    }
}

struct RuntimeUndoPlan {
    truncated: Vec<Message>,
    restored_prompt: String,
    target_n: usize,
    prompts_before: usize,
}

pub fn undo_snapshot_to_prompt(
    snapshot: &SessionSnapshot,
    nth: Option<usize>,
) -> Result<SnapshotUndoResult, RuntimeError> {
    let plan = compute_runtime_undo(&snapshot.messages, nth)?;
    let mut truncated = snapshot.clone();
    truncated.messages = plan.truncated;
    Ok(SnapshotUndoResult {
        snapshot: truncated,
        restored_prompt: plan.restored_prompt,
        target_n: plan.target_n,
        prompts_before: plan.prompts_before,
    })
}

/// Going back to before `turn`, worked out on the session LOG.
///
/// A rewind point names a turn, and the log's `TurnStart { turn }` is where it
/// began. The conversation before it is what the log projects to once a
/// `Rewound { to: <that seq> }` is appended — the very fact the change is then
/// committed as (`events_to_become` finds it), so what is planned here and what
/// lands are one computation.
///
/// Nothing here counts prompts in the conversation as it now stands. That was
/// the bug: a point was looked up by its prompt ordinal in the current
/// projection, and a compaction folds turns out of the projection — not out of
/// the log (`SessionEvent::Compacted`) — so every point before a fold asked for
/// an ordinal the folded conversation no longer had (`UndoOutOfRange`). A
/// `Rewound` to before a fold takes the fold back with it, and a fold before the
/// point stays, so the target is the conversation as it really stood then.
///
/// A turn a rewind already took back is not a place to go back to: a `Rewound`
/// to it would also leave out whatever the earlier rewind covered before it.
///
/// What comes back to the input box is the first thing the person said in that
/// turn, and nothing when they said nothing: a turn the harness opened — a team
/// member's report waking the lead, a `/goal` round — has a `TurnStart` and a
/// point in the ledger like any other, but no person's words of its own (or
/// only words that came in mid-turn). Its place in the log is all a rewind
/// needs; refusing it for want of a prompt is what made 35 of the 710 points on
/// one machine unreachable.
pub fn undo_to_turn_in_log(
    events: &[atomcode_harness::session::LoggedEvent],
    turn: u64,
) -> Result<SnapshotUndoResult, RuntimeError> {
    use atomcode_harness::session::{LoggedEvent, RewindScope as LogScope, SessionEvent};
    let unavailable = || RuntimeError::RewindPointUnavailable { turn_id: turn };
    if atomcode_harness::session::rewound_turns(events).contains(&turn) {
        return Err(unavailable());
    }
    let to = events
        .iter()
        .find_map(|logged| {
            matches!(logged.event, SessionEvent::TurnStart { turn: t } if t == turn)
                .then_some(logged.seq)
        })
        .ok_or_else(unavailable)?;
    let restored_prompt = events
        .iter()
        .find_map(|logged| match &logged.event {
            SessionEvent::UserMessage { turn: t, text, .. } if *t == turn => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let mut rewound = events.to_vec();
    rewound.push(LoggedEvent {
        seq: events.iter().map(|e| e.seq).max().unwrap_or(0) + 1,
        at: 0,
        event: SessionEvent::Rewound {
            turn: events.iter().map(|e| e.event.turn()).max().unwrap_or(0),
            to,
            scope: LogScope::Conversation,
        },
    });
    let prompts = |snapshot: &SessionSnapshot| {
        snapshot
            .messages
            .iter()
            .filter(|message| {
                message.role == atomcode_kernel::message::Role::User && !message.synthetic
            })
            .count()
    };
    let snapshot = atomcode_capabilities::session::events::snapshot_of(&rewound);
    Ok(SnapshotUndoResult {
        target_n: prompts(&snapshot) + 1,
        prompts_before: prompts(&atomcode_capabilities::session::events::snapshot_of(events)),
        restored_prompt,
        snapshot,
    })
}

/// The session's log, as the undo snapshot is projected from it
/// (`current_runtime_snapshot`), so what a rewind plans and the conversation it
/// replaces are read off one history. A session not written down has only the
/// live agent's.
fn session_events(
    runtime: &RuntimeResources,
) -> Result<Vec<atomcode_harness::session::LoggedEvent>, RuntimeError> {
    match runtime.parts.session.as_ref() {
        Some(binding) if binding.manager.is_event_session(&binding.id) => {
            binding.manager.load_events(&binding.id).map_err(|error| {
                RuntimeError::ReconfigureFailed(format!("could not read the session log: {error}"))
            })
        }
        _ => live_root_agent(runtime)
            .map(|live| live.session().events())
            .ok_or(RuntimeError::Unavailable),
    }
}

/// The points a person can still go back to: the ledger's, less those whose
/// turn a rewind or an undo has already taken back.
///
/// The ledger is pruned by a rewind (`SnapshotHook::begin_rewind`) but not by an
/// undo, so after `/undo` it still listed the turns just undone — and picking
/// one was refused (`RewindPointUnavailable`). Read against the log instead:
/// what the log says was taken back is not on offer, whichever gesture took it.
///
/// Unless it carries a workspace checkpoint: `/undo` takes the conversation
/// back and leaves the files, so restoring the code to before that turn is
/// still a thing a person can ask for — and it still works, since a code-only
/// rewind does not look for the turn in the conversation.
///
/// A log that cannot be read leaves the ledger as it is — the catalog is a menu,
/// and the rewind itself checks again.
fn reachable_points(runtime: &RuntimeResources, points: Vec<RewindPoint>) -> Vec<RewindPoint> {
    let Ok(events) = session_events(runtime) else {
        return points;
    };
    let gone = atomcode_harness::session::rewound_turns(&events);
    points
        .into_iter()
        .filter(|point| point.before_tree.is_some() || !gone.contains(&point.turn_id))
        .collect()
}

fn compute_runtime_undo(
    messages: &[Message],
    nth: Option<usize>,
) -> Result<RuntimeUndoPlan, RuntimeError> {
    let prompt_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == atomcode_kernel::message::Role::User && !message.synthetic
        })
        .map(|(index, _)| index)
        .collect();
    let available = prompt_indices.len();
    let target = nth.unwrap_or(available);
    let Some(index) = target
        .checked_sub(1)
        .and_then(|index| prompt_indices.get(index))
        .copied()
    else {
        return Err(RuntimeError::UndoOutOfRange {
            requested: target,
            available,
        });
    };
    Ok(RuntimeUndoPlan {
        truncated: messages[..index].to_vec(),
        restored_prompt: messages[index].text.clone(),
        target_n: target,
        prompts_before: available,
    })
}

fn handle_compaction_event(
    event: AgentEvent,
    compactions: &mut CompactionTracker,
    observed_tokens: &mut Option<usize>,
    runtime_event_tx: &RuntimeEventEmitter,
) -> Option<AgentEvent> {
    match event {
        AgentEvent::CompactionStarted { trigger } => {
            compactions.started(&trigger);
            let _ = runtime_event_tx.send(CodingRuntimeEvent::CompactionStarted { trigger });
            None
        }
        AgentEvent::Compacted {
            trigger,
            epoch,
            removed,
            bytes_before,
            bytes_after,
            committed,
            snapshot,
        } => {
            compactions.finished(&trigger);
            let mut outcome = CompactionOutcome::from_kernel(
                trigger,
                epoch,
                removed,
                bytes_before,
                bytes_after,
                committed,
                *observed_tokens,
            );
            outcome.committed_snapshot = snapshot.map(Arc::new);
            if committed {
                *observed_tokens = Some(outcome.estimated_tokens_after);
            }
            let _ = runtime_event_tx.send(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome),
            });
            None
        }
        AgentEvent::CompactionFailed { trigger, error } => {
            compactions.finished(&trigger);
            let _ = runtime_event_tx.send(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Failed { trigger, error },
            });
            None
        }
        event => Some(event),
    }
}

#[derive(Default)]
struct StopReport {
    forced: bool,
    reason: Option<StopReason>,
    snapshot: Option<SessionSnapshot>,
    snapshot_after_turn_terminal: bool,
    conversation_changed: bool,
    persistence_failure: Option<String>,
}

impl StopReport {
    fn has_verified_turn_terminal(&self) -> bool {
        self.reason.is_some() && self.snapshot_after_turn_terminal
    }
}

fn record_stopped_conversation_event(report: &mut StopReport, event: &AgentEvent) {
    match event {
        AgentEvent::TurnComplete { .. } => report.conversation_changed = true,
        AgentEvent::Compacted {
            committed: true,
            snapshot,
            ..
        } => {
            report.conversation_changed = true;
            if let Some(snapshot) = snapshot {
                report.snapshot = Some(snapshot.clone());
            }
        }
        _ => {}
    }
}

/// End whatever the agent is doing and read its conversation back, WITHOUT
/// taking the handle.
///
/// [`stop_current_agent`] is the chain's shape: it `take()`s the handle, sends
/// `Shutdown` and lets the agent die, because on that engine a provider change
/// means rebuilding the agent anyway. On the harness the agent is the thing
/// worth keeping — the provider lives behind a seam and can be swapped under it
/// — so this cancels the turn instead of ending the agent, and asks for the
/// snapshot the caller still needs.
async fn quiesce_current_agent(
    agent: &mut AgentHandle,
    compactions: &mut CompactionTracker,
    observed_tokens: &mut Option<usize>,
    runtime_event_tx: &RuntimeEventEmitter,
    reason: CompactionInterruption,
    team_manager: Option<&crate::team::TeamRunManager>,
    persistence_status: Option<SnapshotPersistenceStatus>,
) -> StopReport {
    // Detached team members are background work started under the credentials
    // being revoked; a logout must end them for the same reason it ends the
    // provider.
    if let Some(manager) = team_manager {
        manager.stop_all().await;
    }
    let mut report = StopReport::default();
    let _ = agent.commands.send(AgentCommand::Cancel);
    let _ = agent.commands.send(AgentCommand::Snapshot);
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = agent.events.recv() => match event {
                Some(event) => {
                    record_stopped_conversation_event(&mut report, &event);
                    match handle_compaction_event(
                        event,
                        compactions,
                        observed_tokens,
                        runtime_event_tx,
                    ) {
                        Some(AgentEvent::Usage(meta)) => {
                            *observed_tokens = Some(meta.used_tokens as usize);
                        }
                        Some(AgentEvent::TurnComplete { reason, .. }) => {
                            report.reason = Some(reason.folded_for_runtime_drivers());
                        }
                        Some(AgentEvent::Snapshot { snapshot }) => {
                            report.snapshot = Some(snapshot);
                            report.snapshot_after_turn_terminal = report.reason.is_some();
                            // The snapshot is the last thing asked for, so it is
                            // also the signal that the agent is quiet again.
                            break;
                        }
                        _ => {}
                    }
                }
                // The agent ended on its own; nothing more will arrive.
                None => break,
            },
            () = &mut timeout => {
                // Not fatal and not `forced`: the agent is still alive and the
                // caller keeps its handle. The snapshot is simply missing, which
                // `preserve_sessionless_snapshot` already treats as "nothing to
                // preserve".
                break;
            }
        }
    }
    compactions.interrupt_all(reason, runtime_event_tx);
    emit_terminal_persistence_warnings(persistence_status.as_ref(), runtime_event_tx);
    report.persistence_failure = persistence_status.and_then(|s| s.take_uncertain_commit());
    report
}

async fn stop_current_agent(
    agent: &mut Option<AgentHandle>,
    compactions: &mut CompactionTracker,
    observed_tokens: &mut Option<usize>,
    runtime_event_tx: &RuntimeEventEmitter,
    reason: CompactionInterruption,
    team_manager: Option<&crate::team::TeamRunManager>,
    persistence_status: Option<SnapshotPersistenceStatus>,
) -> StopReport {
    // Detached Team members outlive the tool call that spawned them. On a genuine
    // teardown (shutdown, provider deactivate, undo/restore) callers pass the
    // manager so we terminate them and no background editor is orphaned. A mere
    // provider RECONFIGURE that KEEPS the session (`/model`, `/config` reload)
    // passes `None`: that async work is independent of which provider the main
    // agent uses and must survive the reconfigure.
    if let Some(manager) = team_manager {
        manager.stop_all().await;
    }
    let Some(mut agent) = agent.take() else {
        compactions.interrupt_all(reason, runtime_event_tx);
        emit_terminal_persistence_warnings(persistence_status.as_ref(), runtime_event_tx);
        return StopReport {
            persistence_failure: persistence_status
                .and_then(|status| status.take_uncertain_commit()),
            ..StopReport::default()
        };
    };
    let _ = agent.commands.send(AgentCommand::Shutdown);
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(timeout);
    let mut events_open = true;
    let mut report = StopReport::default();
    loop {
        tokio::select! {
            result = &mut agent.task => {
                let _ = result;
                break;
            }
            event = agent.events.recv(), if events_open => match event {
                Some(event) => {
                    record_stopped_conversation_event(&mut report, &event);
                    match handle_compaction_event(
                        event,
                        compactions,
                        observed_tokens,
                        runtime_event_tx,
                    ) {
                        Some(AgentEvent::Usage(meta)) => {
                            *observed_tokens = Some(meta.used_tokens as usize);
                        }
                        Some(AgentEvent::TurnComplete { reason, .. }) => {
                            report.reason = Some(reason.folded_for_runtime_drivers());
                        }
                        Some(AgentEvent::Snapshot { snapshot }) => {
                            report.snapshot = Some(snapshot);
                            report.snapshot_after_turn_terminal = report.reason.is_some();
                        }
                        _ => {}
                    }
                }
                None => events_open = false,
            },
            () = &mut timeout => {
                agent.task.abort();
                let _ = (&mut agent.task).await;
                report.forced = true;
                break;
            }
        }
    }

    while let Ok(event) = agent.events.try_recv() {
        record_stopped_conversation_event(&mut report, &event);
        match handle_compaction_event(event, compactions, observed_tokens, runtime_event_tx) {
            Some(AgentEvent::Usage(meta)) => {
                *observed_tokens = Some(meta.used_tokens as usize);
            }
            Some(AgentEvent::TurnComplete { reason, .. }) => {
                report.reason = Some(reason.folded_for_runtime_drivers())
            }
            Some(AgentEvent::Snapshot { snapshot }) => {
                report.snapshot = Some(snapshot);
                report.snapshot_after_turn_terminal = report.reason.is_some();
            }
            _ => {}
        }
    }
    compactions.interrupt_all(reason, runtime_event_tx);
    emit_terminal_persistence_warnings(persistence_status.as_ref(), runtime_event_tx);
    report.persistence_failure =
        persistence_status.and_then(|status| status.take_uncertain_commit());
    report
}

fn emit_terminal_persistence_warnings(
    persistence_status: Option<&SnapshotPersistenceStatus>,
    runtime_event_tx: &RuntimeEventEmitter,
) {
    let Some(status) = persistence_status else {
        return;
    };
    if let Some(warning) = status.take_auxiliary_warning() {
        let _ = runtime_event_tx.send(CodingRuntimeEvent::PersistenceWarning(warning));
    }
    if let Some(warning) = status.take_cost_warning() {
        let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(warning));
    }
}

/// End a turn the runtime stopped itself, and end it EXACTLY once.
///
/// Every path that quiesces the agent has to reach this, including the ones that
/// then fail: `quiesce_current_agent` consumes the agent's own `TurnComplete`
/// into the `StopReport`, so after it runs nothing else will ever finish the
/// turn. A path that skips this leaves `active_turn` set, and the next `Submit`
/// becomes a steer into a turn that is gone.
///
/// **No criterion covers the failing paths, and that is a known gap.** The only
/// way to fail the logout patch is for a row to refuse to remount, and nothing a
/// test can reach makes that happen — the layer is a fixed string and the one
/// row it touches is `llm`. So these branches are held by construction and by
/// this comment rather than by a red test; if a way to inject a patch failure
/// ever appears, the criterion to write is "a failed logout still ends the turn
/// it cancelled".
fn finish_stopped_native_turn(
    report: &StopReport,
    resources: Option<&RuntimeResources>,
    active_turn: &mut Option<u64>,
    terminal_reason: &mut Option<StopReason>,
    turn_stats: &mut RuntimeTurnStats,
    conversation_revision: &mut u64,
    snapshot_waiters: &mut Vec<RuntimeSnapshotWaiter>,
    runtime_event_tx: &RuntimeEventEmitter,
) {
    if report.conversation_changed || active_turn.is_some() {
        *conversation_revision = (*conversation_revision).wrapping_add(1);
    }
    if let Some(error) = report.persistence_failure.as_ref() {
        let message = format!(
            "session persistence became uncertain while stopping the current agent: {error}"
        );
        for waiter in snapshot_waiters.drain(..) {
            let _ = waiter.send(Err(RuntimeError::SnapshotUnavailable(message.clone())));
        }
        if let Some(turn_id) = active_turn.take() {
            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::SnapshotUnavailable {
                    turn_id,
                    reason: StopReason::ProviderError,
                    error: RuntimeSnapshotError { message },
                    stats: std::mem::take(turn_stats),
                },
            ));
        }
        *terminal_reason = None;
        return;
    }
    let snapshot = report.snapshot.clone().or_else(|| {
        let binding = resources?.parts.session.as_ref()?;
        binding.manager.load_snapshot(&binding.id).ok()
    });
    if let Some(snapshot) = snapshot {
        let snapshot = Arc::new(snapshot);
        let undo_snapshot = resources
            .and_then(current_runtime_snapshot)
            .map(Arc::new)
            .unwrap_or_else(|| snapshot.clone());
        for waiter in snapshot_waiters.drain(..) {
            let _ = waiter.send(Ok(RuntimeSnapshotReceipt {
                snapshot: snapshot.clone(),
                undo_snapshot: undo_snapshot.clone(),
                revision: *conversation_revision,
            }));
        }
        if let Some(turn_id) = active_turn.take() {
            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::Completed {
                    turn_id,
                    reason: report
                        .reason
                        .or_else(|| terminal_reason.take())
                        .unwrap_or(StopReason::Cancelled),
                    snapshot,
                    stats: std::mem::take(turn_stats),
                },
            ));
        }
    } else {
        let message = "runtime stopped before a canonical snapshot was available".to_string();
        for waiter in snapshot_waiters.drain(..) {
            let _ = waiter.send(Err(RuntimeError::SnapshotUnavailable(message.clone())));
        }
        if let Some(turn_id) = active_turn.take() {
            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::SnapshotUnavailable {
                    turn_id,
                    reason: report
                        .reason
                        .or_else(|| terminal_reason.take())
                        .unwrap_or(StopReason::Cancelled),
                    error: RuntimeSnapshotError { message },
                    stats: std::mem::take(turn_stats),
                },
            ));
        }
    }
    *terminal_reason = None;
}

#[allow(clippy::too_many_arguments)]
fn fail_close_after_stopped_persistence(
    report: &StopReport,
    resources: Option<&RuntimeResources>,
    goal: &mut Option<GoalState>,
    loop_state: &mut Option<LoopState>,
    pending_wakeup: &mut Option<WakeupRequest>,
    held_turn: &mut Option<(u64, StopReason, Arc<SessionSnapshot>, RuntimeTurnStats)>,
    active_turn: &mut Option<u64>,
    terminal_reason: &mut Option<StopReason>,
    turn_stats: &mut RuntimeTurnStats,
    conversation_revision: &mut u64,
    snapshot_waiters: &mut Vec<RuntimeSnapshotWaiter>,
    agent_available: &mut bool,
    state: &AtomicU64,
    generation: u64,
    runtime_event_tx: &RuntimeEventEmitter,
) -> Option<RuntimeError> {
    let error = report.persistence_failure.as_ref()?;
    finish_stopped_native_turn(
        report,
        resources,
        active_turn,
        terminal_reason,
        turn_stats,
        conversation_revision,
        snapshot_waiters,
        runtime_event_tx,
    );
    if let Some(mut current) = goal.take() {
        current.cancel.cancel();
        current.finish(
            GoalTerminal::Failed,
            "ended: session persistence became uncertain",
        );
        let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(current.progress()));
    }
    if let Some(mut current) = loop_state.take() {
        current.cancel.cancel();
        current.active = false;
        current.last_reason = Some("ended: session persistence became uncertain".into());
        let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(current.progress()));
    }
    if let Some(runtime) = resources {
        runtime.loop_active.store(false, Ordering::Release);
    }
    *pending_wakeup = None;
    *held_turn = None;
    *active_turn = None;
    *terminal_reason = None;
    *agent_available = false;
    state.store(
        runtime_phase_state(generation, RuntimePhase::Failed),
        Ordering::Release,
    );
    let message = format!(
        "session persistence became uncertain while stopping the current agent; runtime stopped: {error}"
    );
    let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(AgentEvent::Error {
        message: message.clone(),
        http_status: None,
        code: None,
        retryable: None,
    }));
    Some(RuntimeError::ReconfigureFailed(message))
}

#[allow(clippy::too_many_arguments)]
fn fail_close_after_forced_provider_stop(
    message: &str,
    resources: Option<&RuntimeResources>,
    goal: &mut Option<GoalState>,
    loop_state: &mut Option<LoopState>,
    pending_wakeup: &mut Option<WakeupRequest>,
    held_turn: &mut Option<(u64, StopReason, Arc<SessionSnapshot>, RuntimeTurnStats)>,
    active_turn: &mut Option<u64>,
    terminal_reason: &mut Option<StopReason>,
    turn_stats: &mut RuntimeTurnStats,
    conversation_revision: &mut u64,
    snapshot_waiters: &mut Vec<RuntimeSnapshotWaiter>,
    agent_available: &mut bool,
    state: &AtomicU64,
    generation: u64,
    runtime_event_tx: &RuntimeEventEmitter,
) {
    *conversation_revision =
        (*conversation_revision).wrapping_add(u64::from(active_turn.is_some()));
    let unavailable = RuntimeError::SnapshotUnavailable(message.to_string());
    for waiter in snapshot_waiters.drain(..) {
        let _ = waiter.send(Err(unavailable.clone()));
    }
    if let Some(turn_id) = active_turn.take() {
        let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
            TurnCompletion::SnapshotUnavailable {
                turn_id,
                reason: StopReason::ProviderError,
                error: RuntimeSnapshotError {
                    message: message.to_string(),
                },
                stats: std::mem::take(turn_stats),
            },
        ));
    }
    if let Some(mut current) = goal.take() {
        current.cancel.cancel();
        current.finish(
            GoalTerminal::Failed,
            "ended: active agent did not stop safely",
        );
        let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(current.progress()));
    }
    if let Some(mut current) = loop_state.take() {
        current.cancel.cancel();
        current.active = false;
        current.last_reason = Some("ended: active agent did not stop safely".into());
        let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(current.progress()));
    }
    if let Some(runtime) = resources {
        runtime.loop_active.store(false, Ordering::Release);
    }
    *pending_wakeup = None;
    *held_turn = None;
    *terminal_reason = None;
    *agent_available = false;
    state.store(
        runtime_phase_state(generation, RuntimePhase::Failed),
        Ordering::Release,
    );
    let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(AgentEvent::Error {
        message: message.to_string(),
        http_status: None,
        code: None,
        retryable: None,
    }));
}

#[allow(clippy::too_many_arguments)]
fn cancel_controllers_and_finish_held(
    goal: &mut Option<GoalState>,
    loop_state: &mut Option<LoopState>,
    pending_wakeup: &mut Option<WakeupRequest>,
    held_turn: &mut Option<(u64, StopReason, Arc<SessionSnapshot>, RuntimeTurnStats)>,
    active_turn: &mut Option<u64>,
    terminal_reason: &mut Option<StopReason>,
    loop_active: Option<&std::sync::atomic::AtomicBool>,
    state: &AtomicU64,
    generation: u64,
    phase_after_held: RuntimePhase,
    runtime_event_tx: &RuntimeEventEmitter,
    detail: &str,
) -> bool {
    let had_controller = goal.is_some() || loop_state.is_some();
    if let Some(mut current) = goal.take() {
        current.cancel.cancel();
        current.finish(GoalTerminal::Failed, detail);
        let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(current.progress()));
    }
    if let Some(mut current) = loop_state.take() {
        current.cancel.cancel();
        current.active = false;
        current.last_reason = Some(detail.into());
        let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(current.progress()));
    }
    if let Some(loop_active) = loop_active {
        loop_active.store(false, Ordering::Release);
    }
    *pending_wakeup = None;
    if let Some((turn_id, _, snapshot, stats)) = held_turn.take() {
        *active_turn = None;
        *terminal_reason = None;
        let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
            TurnCompletion::Completed {
                turn_id,
                reason: StopReason::Cancelled,
                snapshot,
                stats,
            },
        ));
        state.store(
            runtime_phase_state(generation, phase_after_held),
            Ordering::Release,
        );
    }
    had_controller
}

#[doc(hidden)]
pub fn noop_agent_handle() -> AgentHandle {
    let (commands, mut command_rx) = mpsc::unbounded_channel();
    let (event_tx, events) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                AgentCommand::Compact { focus } => {
                    let _ = event_tx.send(AgentEvent::Compacted {
                        trigger: CompactTrigger::Manual { focus },
                        epoch: 0,
                        removed: 0,
                        bytes_before: 0,
                        bytes_after: 0,
                        committed: false,
                        snapshot: None,
                    });
                }
                AgentCommand::Shutdown => break,
                _ => {}
            }
        }
    });
    AgentHandle {
        commands,
        events,
        task,
    }
}

/// The runtime owner has stopped and can no longer accept controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeUnavailable;

impl fmt::Display for RuntimeUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("coding runtime is unavailable")
    }
}

impl Error for RuntimeUnavailable {}

/// Resolve the `/goal` round cap at goal start. An explicit `ATOMCODE_GOAL_MAX_ROUNDS`
/// wins and skips the network (its value is already baked into `config_default`).
/// Otherwise size the budget from the account's live request quota — a share of the
/// tightest rolling window's `call_limit` — fetched best-effort through the host
/// source with a short timeout. Any miss (no source, fetch error/timeout, no usable
/// window, non-CodingPlan user) falls back to `config_default` so `/goal` never blocks.
async fn resolve_goal_round_cap(
    rate_limit_source: Option<&Arc<dyn crate::rate_limit::RateLimitWindowSource>>,
    config_default: u32,
) -> u32 {
    if crate::config::goal_max_rounds_env().is_some() {
        return config_default;
    }
    let call_limit = match rate_limit_source {
        Some(source) => {
            match tokio::time::timeout(std::time::Duration::from_secs(3), source.fetch_windows())
                .await
            {
                Ok(Ok(windows)) => crate::rate_limit::binding_window_call_limit(&windows),
                _ => None,
            }
        }
        None => None,
    };
    match call_limit {
        Some(limit) => crate::config::derive_goal_max_rounds(None, Some(limit)),
        None => config_default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_image(data: &str) -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: data.into(),
        }
    }

    #[test]
    fn steered_acknowledgement_restores_preprocessed_original_input() {
        let original = UserInput {
            text: "before\n[Image #1]\nafter".into(),
            images: vec![test_image("raw-image")],
        };
        let forwarded = UserInput {
            text: "before\n[image description]\nafter".into(),
            images: Vec::new(),
        };
        let mut pending = VecDeque::from([PendingSteerAcknowledgement {
            generation: 7,
            original: original.clone(),
            forwarded: forwarded.clone(),
        }]);

        let acknowledged = acknowledge_steered_inputs(
            &mut pending,
            7,
            &[atomcode_kernel::event::SteeredInput {
                text: forwarded.text,
                images: forwarded.images,
            }],
        );

        assert_eq!(acknowledged, vec![original]);
        assert!(pending.is_empty());
    }

    /// A rewind point is a turn, and the turn is found in the LOG: a point
    /// before a compaction lands before it with the folded turns back, two
    /// prompts that read the same are two turns, and what is planned is exactly
    /// the one `Rewound` the change is committed as.
    #[test]
    fn a_rewind_to_a_turn_is_planned_on_the_log() {
        use atomcode_harness::session::{LoggedEvent, RewindScope as LogScope, SessionEvent};

        let fact = |seq: u64, event: SessionEvent| LoggedEvent { seq, at: 0, event };
        let said = |turn: u64, text: &str| SessionEvent::UserMessage {
            turn,
            text: text.into(),
            images: Vec::new(),
        };
        let reply = |turn: u64, text: &str| SessionEvent::AssistantMessage {
            turn,
            round: 1,
            text: text.into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            reasoning_blocks: Vec::new(),
            meta: None,
        };
        let mut events = vec![
            fact(1, SessionEvent::TurnStart { turn: 1 }),
            fact(2, said(1, "first")),
            fact(3, reply(1, "ok")),
            fact(4, SessionEvent::TurnStart { turn: 2 }),
            fact(5, said(2, "继续")),
            fact(6, reply(2, "again")),
            fact(
                7,
                SessionEvent::Compacted {
                    turn: 2,
                    through: 6,
                    summary: "SUMMARY".into(),
                    from: 0,
                },
            ),
            fact(8, SessionEvent::TurnStart { turn: 3 }),
            fact(9, said(3, "继续")),
            fact(10, reply(3, "once more")),
        ];
        let texts = |undo: &SnapshotUndoResult| -> Vec<String> {
            undo.snapshot
                .messages
                .iter()
                .map(|m| m.text.clone())
                .collect()
        };
        let lands_as = |events: &[LoggedEvent], undo: &SnapshotUndoResult| {
            atomcode_capabilities::session::events::events_to_become(
                events,
                &undo.snapshot.messages,
            )
        };

        // Before the fold: the folded turn is back in the clear, no summary.
        let before_the_fold = undo_to_turn_in_log(&events, 2).unwrap();
        assert_eq!(texts(&before_the_fold), vec!["first", "ok"]);
        assert_eq!(before_the_fold.restored_prompt, "继续");
        assert_eq!(
            lands_as(&events, &before_the_fold),
            vec![SessionEvent::Rewound {
                turn: 3,
                to: 4,
                scope: LogScope::Conversation,
            }],
            "one fact, to that turn's start"
        );

        // After the fold: the conversation then was the summary.
        let after_the_fold = undo_to_turn_in_log(&events, 3).unwrap();
        assert_eq!(texts(&after_the_fold), vec!["SUMMARY"]);
        assert_eq!(after_the_fold.restored_prompt, "继续");
        assert_eq!(
            lands_as(&events, &after_the_fold),
            vec![SessionEvent::Rewound {
                turn: 3,
                to: 8,
                scope: LogScope::Conversation,
            }]
        );

        // A turn the log never held, and one a rewind already took back.
        assert!(matches!(
            undo_to_turn_in_log(&events, 9),
            Err(RuntimeError::RewindPointUnavailable { turn_id: 9 })
        ));
        events.push(fact(
            11,
            SessionEvent::Rewound {
                turn: 3,
                to: 8,
                scope: LogScope::Conversation,
            },
        ));
        assert!(matches!(
            undo_to_turn_in_log(&events, 3),
            Err(RuntimeError::RewindPointUnavailable { turn_id: 3 })
        ));
        // …while the turns before it still are.
        assert_eq!(
            texts(&undo_to_turn_in_log(&events, 2).unwrap()),
            vec!["first", "ok"]
        );

        // A turn the harness opened — a member's report woke the lead — has a
        // start and no words of the person's: still a place to go back to, with
        // nothing to hand back.
        let woken = vec![
            fact(1, SessionEvent::TurnStart { turn: 1 }),
            fact(2, said(1, "first")),
            fact(3, reply(1, "ok")),
            fact(4, SessionEvent::TurnStart { turn: 2 }),
            fact(5, reply(2, "the member reported")),
        ];
        let before_the_report = undo_to_turn_in_log(&woken, 2).expect("a harness-opened turn");
        assert_eq!(texts(&before_the_report), vec!["first", "ok"]);
        assert_eq!(before_the_report.restored_prompt, "");
        assert_eq!(
            lands_as(&woken, &before_the_report),
            vec![SessionEvent::Rewound {
                turn: 2,
                to: 4,
                scope: LogScope::Conversation,
            }]
        );
    }

    #[test]
    fn steered_acknowledgement_does_not_consume_an_unrelated_client_input() {
        let original = UserInput::from("local");
        let mut pending = VecDeque::from([PendingSteerAcknowledgement {
            generation: 4,
            original: original.clone(),
            forwarded: UserInput::from("local-forwarded"),
        }]);
        let remote = atomcode_kernel::event::SteeredInput {
            text: "remote".into(),
            images: Vec::new(),
        };

        assert_eq!(
            acknowledge_steered_inputs(&mut pending, 4, &[remote]),
            Vec::<UserInput>::new()
        );
        assert_eq!(pending.len(), 1);

        let local = acknowledge_steered_inputs(
            &mut pending,
            4,
            &[atomcode_kernel::event::SteeredInput {
                text: "local-forwarded".into(),
                images: Vec::new(),
            }],
        );
        assert_eq!(local[0].text, original.text);
        assert!(pending.is_empty());
    }

    #[test]
    fn steered_acknowledgement_drops_stale_generation_state() {
        let mut pending = VecDeque::from([PendingSteerAcknowledgement {
            generation: 2,
            original: UserInput::from("old"),
            forwarded: UserInput::from("same"),
        }]);
        let current = atomcode_kernel::event::SteeredInput {
            text: "same".into(),
            images: Vec::new(),
        };

        assert_eq!(
            acknowledge_steered_inputs(&mut pending, 3, &[current]),
            Vec::<UserInput>::new()
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn context_bearing_next_turn_is_not_registered_as_a_steer_acknowledgement() {
        let original = UserInput::from("continue");
        let command = AgentCommand::SendMessageWithContext {
            text: "continue".into(),
            images: Vec::new(),
            context: "recovery".into(),
        };

        assert_eq!(
            forwarded_steer_for_acknowledgement(Some(&original), &command),
            None
        );
    }

    #[test]
    fn plain_in_turn_message_is_registered_as_a_steer_acknowledgement() {
        let original = UserInput::from("original");
        let command = AgentCommand::SendMessage {
            text: "processed".into(),
            images: Vec::new(),
        };

        assert_eq!(
            forwarded_steer_for_acknowledgement(Some(&original), &command),
            Some(UserInput::from("processed"))
        );
    }

    #[test]
    fn runtime_turn_stats_sum_usage_across_rounds() {
        let mut stats = RuntimeTurnStats::default();
        for tokens in [
            atomcode_kernel::stream::TokenUsage {
                prompt: 100,
                completion: 10,
                cached: 80,
            },
            atomcode_kernel::stream::TokenUsage {
                prompt: 150,
                completion: 20,
                cached: 120,
            },
        ] {
            stats.record_usage(&MessageMeta {
                tokens,
                ..Default::default()
            });
        }

        assert_eq!(stats.turn_count, 2);
        assert_eq!(stats.prompt_tokens, 250);
        assert_eq!(stats.completion_tokens, 30);
        assert_eq!(stats.cached_tokens, 200);
        assert_eq!(stats.last_usage.unwrap().tokens.prompt, 150);
    }

    fn team_event(
        generation: u64,
        run: &str,
        seq: u64,
        payload: atomcode_capabilities::team::TeamEventPayload,
    ) -> crate::team::GenerationTeamEvent {
        crate::team::GenerationTeamEvent {
            generation,
            event: atomcode_capabilities::team::TeamEvent::new(
                atomcode_capabilities::team::TeamRunId::new(run),
                seq,
                payload,
            ),
        }
    }

    #[test]
    fn team_event_projection_filters_generation_and_deduplicates_sequences() {
        use atomcode_capabilities::team::TeamEventPayload;

        let mut sequences = BTreeMap::new();
        let first = team_event(3, "run-a", 1, TeamEventPayload::RunStarted { total: 2 });
        assert!(project_team_event(3, &mut sequences, first.clone()).is_some());
        assert!(project_team_event(3, &mut sequences, first).is_none());
        assert!(project_team_event(
            3,
            &mut sequences,
            team_event(3, "run-a", 0, TeamEventPayload::RunStarted { total: 2 })
        )
        .is_none());
        assert!(project_team_event(
            3,
            &mut sequences,
            team_event(2, "old", 99, TeamEventPayload::RunStarted { total: 1 })
        )
        .is_none());
        let terminal = project_team_event(
            3,
            &mut sequences,
            team_event(
                3,
                "run-a",
                2,
                TeamEventPayload::RunFinished {
                    total: 2,
                    completed: 2,
                    failed: 0,
                },
            ),
        )
        .expect("ordered terminal must be delivered");
        assert!(matches!(
            terminal.payload,
            TeamEventPayload::RunFinished { .. }
        ));
    }

    #[derive(Debug)]
    struct FakeQuotaSource {
        result: Result<Vec<crate::rate_limit::RateLimitWindow>, String>,
    }

    #[async_trait::async_trait]
    impl crate::rate_limit::RateLimitWindowSource for FakeQuotaSource {
        fn applies_to(&self, _base_url: &str) -> bool {
            true
        }
        async fn fetch_windows(&self) -> Result<Vec<crate::rate_limit::RateLimitWindow>, String> {
            self.result.clone()
        }
    }

    fn quota_window(call_limit: i64) -> crate::rate_limit::RateLimitWindow {
        crate::rate_limit::RateLimitWindow {
            window_size_seconds: 18_000,
            quota_exhausted: false,
            reset_at_display: "18:09".into(),
            seconds_until_reset: 7200,
            reset_label: "5h".into(),
            call_limit,
            calls_used: 0,
            usage_percent: 0.0,
        }
    }

    // NOTE: assumes ATOMCODE_GOAL_MAX_ROUNDS is unset (same assumption as
    // `config::tests::round_caps_have_generous_defaults`); an env override would
    // short-circuit to the config default.
    #[tokio::test]
    async fn goal_round_cap_derives_from_live_plan_quota() {
        // Pro window (call_limit 1000) → 30% = 300, overriding the passed default.
        let pro: Arc<dyn crate::rate_limit::RateLimitWindowSource> = Arc::new(FakeQuotaSource {
            result: Ok(vec![quota_window(1000)]),
        });
        assert_eq!(resolve_goal_round_cap(Some(&pro), 777).await, 300);
        // Lite window (800) → 240.
        let lite: Arc<dyn crate::rate_limit::RateLimitWindowSource> = Arc::new(FakeQuotaSource {
            result: Ok(vec![quota_window(800)]),
        });
        assert_eq!(resolve_goal_round_cap(Some(&lite), 777).await, 240);
    }

    #[tokio::test]
    async fn goal_round_cap_falls_back_when_quota_unavailable() {
        // No source, a fetch error, and empty windows all fall back to the config
        // default instead of blocking /goal or inventing a number.
        assert_eq!(resolve_goal_round_cap(None, 777).await, 777);
        let err: Arc<dyn crate::rate_limit::RateLimitWindowSource> = Arc::new(FakeQuotaSource {
            result: Err("status_v2 unavailable".into()),
        });
        assert_eq!(resolve_goal_round_cap(Some(&err), 777).await, 777);
        let empty: Arc<dyn crate::rate_limit::RateLimitWindowSource> =
            Arc::new(FakeQuotaSource { result: Ok(vec![]) });
        assert_eq!(resolve_goal_round_cap(Some(&empty), 777).await, 777);
    }

    #[test]
    fn terminal_persistence_warnings_are_emitted_once() {
        let status = SnapshotPersistenceStatus::default();
        status.report_auxiliary_warning("transcript write failed");
        status.report_cost_warning("cost write failed");
        let (raw, mut events) = mpsc::unbounded_channel();
        let event_tx = RuntimeEventEmitter {
            raw,
            tagged: None,
            generation: Arc::new(AtomicU64::new(0)),
        };

        emit_terminal_persistence_warnings(Some(&status), &event_tx);
        emit_terminal_persistence_warnings(Some(&status), &event_tx);

        assert!(matches!(
            events.try_recv(),
            Ok(CodingRuntimeEvent::PersistenceWarning(message))
                if message == "transcript write failed"
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(CodingRuntimeEvent::ControllerWarning(message)) if message == "cost write failed"
        ));
        assert!(events.try_recv().is_err());
    }

    struct TestProviderFactory {
        fail: bool,
    }

    struct MutatingProviderFactory {
        path: std::path::PathBuf,
        fail_second_build: bool,
        builds: std::sync::atomic::AtomicUsize,
    }

    struct MutatingProvider {
        path: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MutatingProvider {
        fn model_name(&self) -> &str {
            "mutating-test-provider"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[atomcode_kernel::tool::ToolDef],
            _options: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            std::fs::write(&self.path, "generated by the agent\n").unwrap();
            use atomcode_kernel::stream::StreamEvent;
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("answer".into()),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }

    impl CodingProviderFactory for MutatingProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            let build = self.builds.fetch_add(1, Ordering::AcqRel);
            if self.fail_second_build && build == 1 {
                return Err(crate::ProviderBuildError::Adapter(
                    "candidate provider failed".into(),
                ));
            }
            Ok(Arc::new(MutatingProvider {
                path: self.path.clone(),
            }))
        }
    }

    struct UsageProvider {
        model: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for UsageProvider {
        fn model_name(&self) -> &str {
            &self.model
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[atomcode_kernel::tool::ToolDef],
            _options: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            use atomcode_kernel::stream::{StreamEvent, TokenUsage};
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("answer".into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 100,
                    completion: 10,
                    cached: 0,
                }),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }

    #[derive(Default)]
    struct UsageProviderFactory {
        fail_model: std::sync::Mutex<Option<String>>,
    }

    impl CodingProviderFactory for UsageProviderFactory {
        fn build(
            &self,
            config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if self.fail_model.lock().unwrap().as_deref() == Some(config.model.as_str()) {
                return Err(crate::ProviderBuildError::Adapter(
                    "expected usage-provider reload failure".into(),
                ));
            }
            Ok(Arc::new(UsageProvider {
                model: config.model.clone(),
            }))
        }
    }

    struct RecoverableAuthFactory {
        fail: std::sync::atomic::AtomicBool,
    }

    struct SourceBuildGatewayFactory;

    struct FailAfterFirstBuildFactory {
        builds: std::sync::atomic::AtomicUsize,
    }

    struct FailSecondBuildFactory {
        builds: std::sync::atomic::AtomicUsize,
    }

    struct BlockAndFailSecondBuildFactory {
        builds: std::sync::atomic::AtomicUsize,
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
    }

    /// The second build removes the session's log, then fails: the rebuild
    /// after a change fails, and so does taking the change back.
    struct DeleteLogAndFailSecondBuildFactory {
        builds: std::sync::atomic::AtomicUsize,
        log_path: std::path::PathBuf,
    }

    impl CodingProviderFactory for RecoverableAuthFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if self.fail.load(Ordering::Acquire) {
                Err(crate::ProviderBuildError::Authentication(
                    "login required".into(),
                ))
            } else {
                Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                    vec![],
                )))
            }
        }
    }

    impl CodingProviderFactory for SourceBuildGatewayFactory {
        fn build(
            &self,
            config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if config.base_url.contains("llm-api.atomgit.com") {
                Err(crate::ProviderBuildError::SourceBuildGatewayUnsupported {
                    base_url: config.base_url.clone(),
                })
            } else {
                Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                    vec![],
                )))
            }
        }
    }

    impl CodingProviderFactory for FailAfterFirstBuildFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if self.builds.fetch_add(1, Ordering::AcqRel) == 0 {
                Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                    vec![
                        atomcode_kernel::stream::StreamEvent::TextDelta("answer".into()),
                        atomcode_kernel::stream::StreamEvent::Done { truncated: false },
                    ],
                ])))
            } else {
                Err(crate::ProviderBuildError::Adapter(
                    "candidate provider failed".into(),
                ))
            }
        }
    }

    impl CodingProviderFactory for FailSecondBuildFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if self.builds.fetch_add(1, Ordering::AcqRel) == 1 {
                Err(crate::ProviderBuildError::Adapter(
                    "candidate provider failed".into(),
                ))
            } else {
                Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                    vec![
                        atomcode_kernel::stream::StreamEvent::TextDelta("answer".into()),
                        atomcode_kernel::stream::StreamEvent::Done { truncated: false },
                    ],
                ])))
            }
        }
    }

    impl CodingProviderFactory for BlockAndFailSecondBuildFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if self.builds.fetch_add(1, Ordering::AcqRel) == 0 {
                return Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                    vec![
                        atomcode_kernel::stream::StreamEvent::TextDelta("answer".into()),
                        atomcode_kernel::stream::StreamEvent::Done { truncated: false },
                    ],
                ])));
            }
            self.entered.wait();
            self.release.wait();
            Err(crate::ProviderBuildError::Adapter(
                "blocked candidate failed".into(),
            ))
        }
    }

    impl CodingProviderFactory for DeleteLogAndFailSecondBuildFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if self.builds.fetch_add(1, Ordering::AcqRel) == 0 {
                return Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                    vec![],
                )));
            }
            std::fs::remove_file(&self.log_path).map_err(|error| {
                crate::ProviderBuildError::Adapter(format!(
                    "could not arrange rollback persistence failure: {error}"
                ))
            })?;
            Err(crate::ProviderBuildError::Adapter(
                "candidate provider failed after the log was removed".into(),
            ))
        }
    }

    impl CodingProviderFactory for TestProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            if self.fail {
                Err(crate::ProviderBuildError::Adapter(
                    "expected failure".into(),
                ))
            } else {
                Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                    vec![
                        atomcode_kernel::stream::StreamEvent::TextDelta("answer".into()),
                        atomcode_kernel::stream::StreamEvent::Done { truncated: false },
                    ],
                ])))
            }
        }
    }

    struct PendingProvider;

    #[async_trait::async_trait]
    impl LlmProvider for PendingProvider {
        fn model_name(&self) -> &str {
            "pending"
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[atomcode_kernel::tool::ToolDef],
            _options: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    struct PendingProviderFactory;

    impl CodingProviderFactory for PendingProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(PendingProvider))
        }
    }

    #[derive(Default)]
    struct TierRecordingFactory {
        models: std::sync::Mutex<Vec<String>>,
        provider_inputs: std::sync::Mutex<Vec<(String, String, String, Option<String>)>>,
        host_fast_cell: std::sync::Mutex<Option<Arc<crate::TierProvider>>>,
        fail_model: std::sync::Mutex<Option<String>>,
    }

    #[derive(Default)]
    struct CountingProviderFactory {
        builds: std::sync::atomic::AtomicUsize,
    }

    #[derive(Default)]
    struct GoalNotMetProviderFactory {
        builds: std::sync::atomic::AtomicUsize,
    }

    struct GoalMetProviderFactory;

    impl CodingProviderFactory for CountingProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                Vec::new(),
            )))
        }
    }

    impl CodingProviderFactory for GoalNotMetProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                vec![
                    atomcode_kernel::stream::StreamEvent::TextDelta(
                        "Verdict: no needs more work".into(),
                    ),
                    atomcode_kernel::stream::StreamEvent::Done { truncated: false },
                ],
            ])))
        }
    }

    /// A provider whose stream yields NO text — simulating an evaluator that
    /// returns an empty response (issue #17). The goal must end as `Stopped`
    /// (not `Failed`) and must NOT emit a `ControllerWarning` spam line.
    struct GoalInconclusiveProviderFactory;

    impl CodingProviderFactory for GoalInconclusiveProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                vec![atomcode_kernel::stream::StreamEvent::Done { truncated: false }],
            ])))
        }
    }

    impl CodingProviderFactory for GoalMetProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                vec![
                    atomcode_kernel::stream::StreamEvent::TextDelta("Verdict: yes goal met".into()),
                    atomcode_kernel::stream::StreamEvent::Done { truncated: false },
                ],
            ])))
        }
    }

    /// A provider that answers the goal EVALUATOR with `Verdict: yes` (so a goal reaches
    /// Satisfied) but the follow-up CLASSIFIER with a configured `Class:` line —
    /// distinguished by the system prompt. Lets a test drive to Satisfied and then
    /// exercise a specific classifier verdict on the next submit.
    impl CodingProviderFactory for TierRecordingFactory {
        fn build(
            &self,
            config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            self.models.lock().unwrap().push(config.model.clone());
            self.provider_inputs.lock().unwrap().push((
                config.provider_name.clone(),
                config.base_url.clone(),
                config.api_key.clone(),
                _session_id.map(str::to_owned),
            ));
            if let Some(cell) = config.subagent_fast_provider.clone() {
                *self.host_fast_cell.lock().unwrap() = Some(cell);
            }
            if self.fail_model.lock().unwrap().as_deref() == Some(config.model.as_str()) {
                return Err(crate::ProviderBuildError::Adapter(
                    "expected tier reload failure".into(),
                ));
            }
            if config.model == "host" {
                let _ = config
                    .subagent_fast_provider
                    .as_ref()
                    .and_then(|cell| cell.get());
                let _ = config
                    .subagent_capable_provider
                    .as_ref()
                    .and_then(|cell| cell.get());
            }
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                Vec::new(),
            )))
        }
    }

    fn tier_provider(model: &str, rank: i64) -> atomcode_config::config::provider::ProviderConfig {
        atomcode_config::config::provider::ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("key".into()),
            model: model.into(),
            base_url: Some("https://example.test/v1".into()),
            system_prompt: None,
            supports_vision: None,
            user_agent: None,
            context_window: 64_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            reasoning_effort_levels: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: Some(rank),
            retry_max_attempts: None,
        }
    }

    #[test]
    fn goal_evaluator_uses_configured_provider() {
        let mut registry = atomcode_config::config::Config::default();
        registry.evaluator_provider = Some("judge".into());
        registry
            .providers
            .insert("judge".into(), tier_provider("judge-model", 0));

        let factory = Arc::new(TierRecordingFactory::default());
        let mut host = native_start(false).agent;
        host.model = "host-model".into();
        host.subagent_config = Some(Arc::new(registry));

        build_goal_evaluator_provider(
            &(factory.clone() as Arc<dyn CodingProviderFactory>),
            &host,
            Some("session-1"),
        )
        .unwrap();

        assert_eq!(factory.models.lock().unwrap().as_slice(), ["judge-model"]);
    }

    #[test]
    fn goal_evaluator_falls_back_to_host_when_configured_provider_fails() {
        let mut registry = atomcode_config::config::Config::default();
        registry.evaluator_provider = Some("judge".into());
        registry
            .providers
            .insert("judge".into(), tier_provider("judge-model", 0));

        let factory = Arc::new(TierRecordingFactory::default());
        *factory.fail_model.lock().unwrap() = Some("judge-model".into());
        let mut host = native_start(false).agent;
        host.model = "host-model".into();
        host.subagent_config = Some(Arc::new(registry));

        build_goal_evaluator_provider(
            &(factory.clone() as Arc<dyn CodingProviderFactory>),
            &host,
            Some("session-1"),
        )
        .unwrap();

        assert_eq!(
            factory.models.lock().unwrap().as_slice(),
            ["judge-model", "host-model"]
        );
    }

    #[test]
    fn goal_evaluator_never_inherits_host_endpoint_or_credentials() {
        let mut registry = atomcode_config::config::Config::default();
        registry.evaluator_provider = Some("judge".into());
        let mut judge = tier_provider("judge-model", 0);
        judge.base_url = Some("https://judge.example/v1".into());
        judge.api_key = None;
        registry.providers.insert("judge".into(), judge);

        let factory = Arc::new(TierRecordingFactory::default());
        let mut host = native_start(false).agent;
        host.model = "host-model".into();
        host.base_url = "https://host.example/v1".into();
        host.api_key = "host-secret".into();
        host.subagent_config = Some(Arc::new(registry));

        build_goal_evaluator_provider(
            &(factory.clone() as Arc<dyn CodingProviderFactory>),
            &host,
            Some("session-1"),
        )
        .unwrap();

        assert_eq!(
            factory.provider_inputs.lock().unwrap().as_slice(),
            [(
                "judge".into(),
                "https://judge.example/v1".into(),
                String::new(),
                Some("session-1".into())
            )]
        );
    }

    #[tokio::test]
    async fn runtime_start_installs_configured_subagent_tiers() {
        let mut routing = atomcode_config::config::Config::default();
        routing
            .providers
            .insert("fast".into(), tier_provider("fast-model", 0));
        routing
            .providers
            .insert("host".into(), tier_provider("host", 1));
        routing
            .providers
            .insert("capable".into(), tier_provider("capable-model", 2));

        let factory = Arc::new(TierRecordingFactory::default());
        let mut start = native_start(false);
        start.agent.model = "host".into();
        start.agent.subagent_config = Some(Arc::new(routing));
        start.provider_factory = factory.clone();

        let runtime = CodingRuntime::start(start).await.unwrap();
        let models = factory.models.lock().unwrap().clone();
        assert_eq!(models, vec!["host", "fast-model", "capable-model"]);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_provider_reload_does_not_mutate_live_subagent_tiers() {
        let mut routing = atomcode_config::config::Config::default();
        routing
            .providers
            .insert("fast".into(), tier_provider("fast-model", 0));
        routing
            .providers
            .insert("host".into(), tier_provider("host", 1));
        routing
            .providers
            .insert("capable".into(), tier_provider("capable-model", 2));
        let routing = Arc::new(routing);

        let factory = Arc::new(TierRecordingFactory::default());
        let mut start = native_start(false);
        start.agent.model = "host".into();
        start.agent.subagent_config = Some(routing.clone());
        start.provider_factory = factory.clone();
        let runtime = CodingRuntime::start(start).await.unwrap();
        let fast_cell = factory
            .host_fast_cell
            .lock()
            .unwrap()
            .clone()
            .expect("runtime host config must expose fast tier cell");
        assert!(fast_cell.get().is_some());

        *factory.fail_model.lock().unwrap() = Some("fast-model".into());
        let mut next = CodingAgentConfig::new("key", "https://example.test/v1", "fast-model", ".");
        next.subagent_config = Some(routing);
        assert!(runtime.handle.reassemble_provider(next).await.is_err());

        assert!(
            fast_cell.get().is_some(),
            "failed reload must leave the live runtime's tier cache and routing intact"
        );
        runtime.handle.shutdown().await.unwrap();
    }

    fn native_start(fail_provider: bool) -> CodingRuntimeStart {
        CodingRuntimeStart {
            agent: CodingAgentConfig::new("key", "https://example.test/v1", "test", "."),
            prepare: PrepareOptions {
                request_user_input: true,
                session: crate::SessionMode::Disabled,
                tools: true,
                skill_dirs: Some(Vec::new()),
                plugin_skill_dirs: Vec::new(),
                mcp: false,
                extra_mcp_servers: Vec::new(),
                external_subagents: Vec::new(),
                memory: false,
                web: false,
                review: false,
                subagents: crate::SubagentPolicy::Disabled,
                rate_limit_source: None,
                front_end: None,
            },
            provider_factory: Arc::new(TestProviderFactory {
                fail: fail_provider,
            }),
            plugin_hooks: Arc::new(crate::StaticPluginHookSource::default()),
            image_preprocessor: None,
        }
    }

    async fn wait_for_turn_finished(runtime: &mut CodingRuntime) {
        loop {
            if matches!(
                runtime.events.recv().await.unwrap().event,
                CodingRuntimeEvent::TurnFinished(_)
            ) {
                break;
            }
        }
    }

    fn persist_native_session(
        manager: &atomcode_capabilities::session::SessionManager,
        id: &str,
        working_dir: &std::path::Path,
        snapshot: &SessionSnapshot,
    ) {
        let lease = manager.acquire_lease(id).unwrap();
        let mut meta = SessionMeta::new(id, working_dir.to_string_lossy(), 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = u32::try_from(snapshot.messages.len()).unwrap();
        manager
            .commit_native_import(
                &lease,
                Some(snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
    }

    async fn next_native_event(runtime: &mut CodingRuntime) -> CodingRuntimeEvent {
        tokio::time::timeout(std::time::Duration::from_secs(2), runtime.events.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event stream closed")
            .event
    }

    #[tokio::test]
    async fn goal_and_loop_are_mutually_exclusive_runtime_controllers() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();

        runtime.handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            next_native_event(&mut runtime).await,
            CodingRuntimeEvent::GoalChanged(GoalProgress { active: true, condition, .. })
                if condition == "tests pass"
        ));

        // Starting either one now also opens its first round, so the slot is
        // not free until that round is over — the same rule as any other turn,
        // and the reason this ends the round rather than asking twice in a row.
        runtime.handle.cancel().await.ok();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match runtime.handle.start_loop("watch CI", None).await {
                    Ok(()) => break,
                    Err(_) => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .expect("the loop never took the slot from the goal");

        let (mut goal_gone, mut loop_running) = (false, false);
        while !(goal_gone && loop_running) {
            match next_native_event(&mut runtime).await {
                CodingRuntimeEvent::GoalChanged(progress) if !progress.active => goal_gone = true,
                CodingRuntimeEvent::LoopChanged(progress) if progress.active => {
                    assert_eq!(progress.label, "watch CI");
                    loop_running = true;
                }
                _ => {}
            }
        }

        runtime.handle.stop_loop().await.unwrap();
        loop {
            if let CodingRuntimeEvent::LoopChanged(progress) = next_native_event(&mut runtime).await
            {
                if !progress.active {
                    break;
                }
            }
        }
        runtime.handle.shutdown().await.unwrap();
    }

    fn fake_agent() -> (
        AgentHandle,
        mpsc::UnboundedReceiver<AgentCommand>,
        mpsc::UnboundedSender<AgentEvent>,
    ) {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async {});
        (
            AgentHandle {
                commands,
                events,
                task,
            },
            command_rx,
            event_tx,
        )
    }

    async fn controller_test_runtime(
        provider_factory: Arc<dyn CodingProviderFactory>,
    ) -> (
        CodingRuntimeHandle,
        mpsc::UnboundedReceiver<AgentCommand>,
        mpsc::UnboundedSender<AgentEvent>,
        mpsc::UnboundedReceiver<CodingRuntimeEvent>,
        mpsc::UnboundedSender<WakeupRequest>,
        Arc<std::sync::atomic::AtomicBool>,
        KernelRuntimeAdapter,
    ) {
        let config = native_start(false).agent;
        controller_test_runtime_with_config(provider_factory, config).await
    }

    async fn controller_test_runtime_with_config(
        provider_factory: Arc<dyn CodingProviderFactory>,
        config: CodingAgentConfig,
    ) -> (
        CodingRuntimeHandle,
        mpsc::UnboundedReceiver<AgentCommand>,
        mpsc::UnboundedSender<AgentEvent>,
        mpsc::UnboundedReceiver<CodingRuntimeEvent>,
        mpsc::UnboundedSender<WakeupRequest>,
        Arc<std::sync::atomic::AtomicBool>,
        KernelRuntimeAdapter,
    ) {
        let (agent, kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            prepare,
            plugin_hooks,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let loop_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx: wakeup_tx.clone(),
            loop_active: loop_active.clone(),
            image_preprocessor: None,
        };
        let adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );
        (
            handle,
            kernel_commands,
            kernel_events,
            runtime_events,
            wakeup_tx,
            loop_active,
            adapter,
        )
    }

    #[derive(Clone, Copy)]
    enum ShutdownPersistenceTerminal {
        TurnComplete,
        CompactionFailed,
    }

    fn persistence_failing_on_shutdown_agent(
        status: SnapshotPersistenceStatus,
        terminal: ShutdownPersistenceTerminal,
    ) -> AgentHandle {
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                if matches!(command, AgentCommand::Shutdown) {
                    status.report_uncertain_commit(
                        "shutdown checkpoint failed and rollback was incomplete",
                    );
                    let event = match terminal {
                        ShutdownPersistenceTerminal::TurnComplete => AgentEvent::TurnComplete {
                            turn: None,
                            reason: StopReason::Cancelled,
                        },
                        ShutdownPersistenceTerminal::CompactionFailed => {
                            AgentEvent::CompactionFailed {
                                trigger: CompactTrigger::Manual { focus: None },
                                error: CompactionCheckpointError::new("checkpoint failed"),
                            }
                        }
                    };
                    let _ = event_tx.send(event);
                    break;
                }
            }
        });
        AgentHandle {
            commands,
            events,
            task,
        }
    }

    async fn reconfigure_persistence_race_runtime(
        terminal: ShutdownPersistenceTerminal,
    ) -> (
        CodingRuntimeHandle,
        mpsc::UnboundedReceiver<CodingRuntimeEvent>,
        KernelRuntimeAdapter,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        let parts = prepare_with_plugin_hook_source(
            &start.agent,
            start.prepare.clone(),
            start.plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        let status = parts
            .snapshot_persistence_status()
            .expect("persistent parts must expose snapshot persistence status");
        let agent = persistence_failing_on_shutdown_agent(status, terminal);
        let resources = RuntimeResources {
            config: start.agent,
            prepare: start.prepare,
            provider_factory: start.provider_factory,
            plugin_hooks: start.plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );
        (handle, runtime_events, adapter, home, project)
    }

    fn assert_failed_reconfigure_events(
        runtime_events: &mut mpsc::UnboundedReceiver<CodingRuntimeEvent>,
        expect_turn_terminal: bool,
        expect_compaction_terminal: bool,
    ) {
        let mut saw_error = false;
        let mut saw_turn_terminal = false;
        let mut saw_compaction_terminal = false;
        while let Ok(event) = runtime_events.try_recv() {
            match event {
                CodingRuntimeEvent::Agent(AgentEvent::Error { message, .. }) => {
                    saw_error |= message.contains("persistence became uncertain");
                }
                CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                    reason: StopReason::ProviderError,
                    ..
                }) => saw_turn_terminal = true,
                CodingRuntimeEvent::CompactionFinished {
                    completion: CompactionCompletion::Failed { .. },
                } => saw_compaction_terminal = true,
                CodingRuntimeEvent::ProviderChanged { .. }
                | CodingRuntimeEvent::SessionChanged(_)
                | CodingRuntimeEvent::Reconfigured { .. } => {
                    panic!("uncertain persistence must not publish reconfigure success")
                }
                _ => {}
            }
        }
        assert!(saw_error, "uncertain persistence must emit an agent error");
        assert_eq!(saw_turn_terminal, expect_turn_terminal);
        assert_eq!(saw_compaction_terminal, expect_compaction_terminal);
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn provider_reconfigure_fails_when_stop_drains_an_uncertain_turn_commit() {
        let (handle, mut runtime_events, _adapter, _home, project) =
            reconfigure_persistence_race_runtime(ShutdownPersistenceTerminal::TurnComplete).await;
        handle.submit(UserInput::from("active turn")).await.unwrap();
        let next = CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            "next-model",
            project.path(),
        );

        assert!(matches!(
            handle.reassemble_provider(next.clone()).await,
            Err(RuntimeError::ReconfigureFailed(message))
                if message.contains("persistence became uncertain")
        ));
        assert_eq!(
            handle.status(),
            RuntimeStatus {
                generation: 0,
                phase: RuntimePhase::Failed,
            }
        );
        assert_failed_reconfigure_events(&mut runtime_events, true, false);
        assert_eq!(
            handle.reassemble_provider(next).await,
            Err(RuntimeError::Unavailable),
            "fail-close must remain sticky for this owner"
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn provider_reconfigure_fails_when_stop_drains_an_uncertain_compaction() {
        let (handle, mut runtime_events, _adapter, _home, project) =
            reconfigure_persistence_race_runtime(ShutdownPersistenceTerminal::CompactionFailed)
                .await;
        handle.compact(None).unwrap();
        let next = CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            "next-model",
            project.path(),
        );

        assert!(matches!(
            handle.reassemble_provider(next).await,
            Err(RuntimeError::ReconfigureFailed(message))
                if message.contains("persistence became uncertain")
        ));
        assert_eq!(
            handle.status(),
            RuntimeStatus {
                generation: 0,
                phase: RuntimePhase::Failed,
            }
        );
        assert_failed_reconfigure_events(&mut runtime_events, false, true);
        assert_eq!(
            handle.submit(UserInput::from("must fail")).await,
            Err(RuntimeError::Unavailable)
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn uncertain_snapshot_hook_commit_fail_closes_the_completed_turn() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        let mut parts = prepare_with_plugin_hook_source(
            &start.agent,
            start.prepare.clone(),
            start.plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        parts.report_snapshot_persistence_uncertain(
            "session commit failed and rollback was incomplete",
        );
        let loop_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resources = RuntimeResources {
            config: start.agent,
            prepare: start.prepare,
            provider_factory: start.provider_factory,
            plugin_hooks: start.plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active,
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.submit(UserInput::from("turn")).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();

        let mut saw_error = false;
        let completion = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::Agent(AgentEvent::Error { message, .. })) => {
                        saw_error = message.contains("persistence became uncertain")
                    }
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before the turn terminal"),
                }
            }
        })
        .await
        .expect("uncertain snapshot commit lost the turn terminal");
        assert!(saw_error);
        assert!(matches!(
            completion,
            TurnCompletion::SnapshotUnavailable {
                reason: StopReason::ProviderError,
                ..
            }
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Shutdown)
        ));
        assert_eq!(
            handle.submit(UserInput::from("must fail")).await,
            Err(RuntimeError::Unavailable)
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn uncertain_compaction_checkpoint_fail_closes_the_runtime() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        let mut parts = prepare_with_plugin_hook_source(
            &start.agent,
            start.prepare.clone(),
            start.plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        parts.report_snapshot_persistence_uncertain(
            "compaction commit failed and rollback was incomplete",
        );
        let resources = RuntimeResources {
            config: start.agent,
            prepare: start.prepare,
            provider_factory: start.provider_factory,
            plugin_hooks: start.plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.compact(None).unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Compact { focus: None })
        ));
        kernel_events
            .send(AgentEvent::CompactionFailed {
                trigger: CompactTrigger::Manual { focus: None },
                error: CompactionCheckpointError::new("checkpoint failed"),
            })
            .unwrap();

        let mut saw_failed_compaction = false;
        let error_message = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::CompactionFinished {
                        completion: CompactionCompletion::Failed { .. },
                    }) => saw_failed_compaction = true,
                    Some(CodingRuntimeEvent::Agent(AgentEvent::Error { message, .. })) => {
                        break message
                    }
                    Some(_) => {}
                    None => panic!("runtime events closed before persistence failure"),
                }
            }
        })
        .await
        .expect("uncertain compaction failure was not propagated");
        assert!(saw_failed_compaction);
        assert!(error_message.contains("compaction persistence became uncertain"));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Shutdown)
        ));
        assert_eq!(
            handle.submit(UserInput::from("must fail")).await,
            Err(RuntimeError::Unavailable)
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn met_goal_overrides_held_max_rounds_terminal() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(GoalMetProviderFactory)).await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await;
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::MaxRounds,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("done", vec![])]),
            })
            .unwrap();

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(CodingRuntimeEvent::TurnFinished(completion)) =
                    runtime_events.recv().await
                {
                    break completion;
                }
            }
        })
        .await
        .expect("met goal lost its held turn terminal");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::Stopped,
                ..
            }
        ));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_waits_for_snapshot_and_rejects_a_racing_submit() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_goal("finish the task").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        handle.cancel().await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Cancel)
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        assert_eq!(
            handle
                .submit(UserInput::from("must not become a lost steer"))
                .await,
            Err(RuntimeError::Busy)
        );

        let retained = SessionSnapshot::new(vec![
            Message::user("long running goal"),
            Message::assistant("completed work before interruption", vec![]),
        ]);
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: retained.clone(),
            })
            .unwrap();

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before cancel terminal"),
                }
            }
        })
        .await
        .expect("cancel snapshot did not produce a terminal");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::Cancelled,
                snapshot,
                ..
            } if snapshot.as_ref() == &retained
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);

        handle
            .submit(UserInput::from("continue after cancel"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "continue after cancel"
        ));
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pause_goal_cancels_only_the_turn_and_next_submit_resumes_goal() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_goal("finish the task").await.unwrap();
        let _ = runtime_events.recv().await;
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        handle.pause_goal().await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Cancel)
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: false,
                phase: GoalPhase::Paused,
                terminal: None,
                ..
            }))
        ));

        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("start work")]),
            })
            .unwrap();
        loop {
            if matches!(
                runtime_events.recv().await,
                Some(CodingRuntimeEvent::TurnFinished(_))
            ) {
                break;
            }
        }

        handle.submit(UserInput::from("continue")).await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                phase: GoalPhase::Pursuing,
                condition,
                ..
            })) if condition == "finish the task"
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "continue"
        ));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_clears_a_paused_goal_without_an_active_turn() {
        let (
            handle,
            _kernel_commands,
            _kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_goal("finish the task").await.unwrap();
        let _ = runtime_events.recv().await;
        handle.pause_goal().await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                phase: GoalPhase::Paused,
                ..
            }))
        ));

        handle.cancel().await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: false,
                phase: GoalPhase::Ended,
                terminal: Some(GoalTerminal::Cancelled),
                ..
            }))
        ));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn goal_evaluator_failure_stops_without_replaying_the_main_agent() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: true })).await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await;
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("not evaluated", vec![])]),
            })
            .unwrap();

        let mut saw_inactive_goal = false;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                        active: false,
                        terminal: Some(GoalTerminal::Failed),
                        ..
                    })) => saw_inactive_goal = true,
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before evaluator terminal"),
                }
            }
        })
        .await
        .expect("evaluator failure lost the held terminal");
        assert!(saw_inactive_goal, "evaluator failure must deactivate /goal");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::ProviderError,
                ..
            }
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), kernel_commands.recv())
                .await
                .is_err(),
            "an evaluator failure must not dispatch a synthetic main-agent retry"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn empty_evaluator_response_ends_goal_stopped_without_warning_spam() {
        // Regression for issue #17: when the goal evaluator returns an EMPTY
        // stream, the goal must end as `Stopped` (the agent's work is not failed)
        // and the runtime must NOT emit the `goal evaluator failed` warning line
        // that previously spammed the transcript and made the agent retry.
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(GoalInconclusiveProviderFactory)).await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await;
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("done", vec![])]),
            })
            .unwrap();

        let mut saw_stopped_goal = false;
        let mut saw_failed_goal = false;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                        active: false,
                        terminal: Some(GoalTerminal::Stopped),
                        ..
                    })) => saw_stopped_goal = true,
                    Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                        active: false,
                        terminal: Some(GoalTerminal::Failed),
                        ..
                    })) => saw_failed_goal = true,
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(CodingRuntimeEvent::ControllerWarning(_)) => {
                        panic!("empty evaluator response must not emit a ControllerWarning")
                    }
                    Some(_) => {}
                    None => panic!("runtime events closed before goal terminal"),
                }
            }
        })
        .await
        .expect("empty evaluator response lost the held terminal");
        assert!(
            saw_stopped_goal,
            "empty evaluator response must end the goal as Stopped, not Failed"
        );
        assert!(
            !saw_failed_goal,
            "empty evaluator response must not mark the goal Failed"
        );
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::Stopped,
                ..
            }
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), kernel_commands.recv())
                .await
                .is_err(),
            "an inconclusive evaluation must not dispatch a synthetic main-agent retry"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn recoverable_failures_resume_with_context_even_when_compact_fails() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(GoalNotMetProviderFactory::default())).await;

        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        // First recoverable failure: continue without repeating the recap into every
        // synthetic prompt. The runtime stores one bounded copy for recovery compact.
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::ProviderError,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![
                    Message::user("initial turn"),
                    Message::assistant("edited the files", vec![]),
                ]),
            })
            .unwrap();
        let continuation =
            tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                .await
                .expect("recoverable failure did not dispatch a continuation");
        assert!(matches!(
            continuation,
            Some(AgentCommand::SendSyntheticMessage { text })
                if !text.contains("Progress so far")
        ));

        // Drive the remaining recoverable failures to exhaust MAX_UNPRODUCTIVE.
        for round in 2..=MAX_UNPRODUCTIVE {
            kernel_events
                .send(AgentEvent::TurnComplete {
                    turn: None,
                    reason: StopReason::ProviderError,
                })
                .unwrap();
            assert!(matches!(
                kernel_commands.recv().await,
                Some(AgentCommand::Snapshot)
            ));
            kernel_events
                .send(AgentEvent::Snapshot {
                    snapshot: SessionSnapshot::new(vec![
                        Message::user("initial turn"),
                        Message::assistant("edited the files", vec![]),
                    ]),
                })
                .unwrap();
            if round < MAX_UNPRODUCTIVE {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                        .await
                        .expect("recoverable failure did not dispatch a continuation");
            }
        }

        // The budget-exhausted terminal must PAUSE the goal (still registered,
        // PausedAtCap) instead of clearing it — clearing would force a from-scratch
        // re-run after the user compacts and continues.
        let mut saw_paused_at_cap = false;
        let mut saw_turn_finished = false;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                        active: false,
                        phase: GoalPhase::PausedAtCap,
                        terminal: Some(GoalTerminal::Stopped),
                        ..
                    })) => saw_paused_at_cap = true,
                    Some(CodingRuntimeEvent::TurnFinished(_)) => {
                        saw_turn_finished = true;
                        break;
                    }
                    Some(_) => {}
                    None => panic!("runtime events closed before goal terminal"),
                }
            }
        })
        .await
        .expect("recoverable exhaustion lost the goal terminal");
        assert!(
            saw_paused_at_cap,
            "repeated recoverable failures must pause the goal at cap, not clear it"
        );
        assert!(
            saw_turn_finished,
            "the failed turn must still finish with a TurnFinished terminal"
        );

        // Even if a manual compact fails, recovery context remains runtime-owned;
        // it is not transported through public CompactTrigger.focus.
        handle.compact(None).unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Compact { focus: None })
        ));
        let trigger = CompactTrigger::Manual { focus: None };
        kernel_events
            .send(AgentEvent::CompactionStarted {
                trigger: trigger.clone(),
            })
            .unwrap();
        kernel_events
            .send(AgentEvent::CompactionFailed {
                trigger,
                error: atomcode_kernel::checkpoint::CompactionCheckpointError::new(
                    "checkpoint failed",
                ),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    runtime_events.recv().await,
                    Some(CodingRuntimeEvent::CompactionFinished {
                        completion: CompactionCompletion::Failed { .. }
                    })
                ) {
                    break;
                }
            }
        })
        .await
        .expect("manual recovery compact did not finish");

        // The paused goal re-engages as ONE real turn with synthetic recovery
        // context attached. The real user text remains a distinct normal message.
        handle.submit(UserInput::from("continue")).await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                phase: GoalPhase::Pursuing,
                condition,
                ..
            })) if condition == "tests pass"
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessageWithContext { text, context, .. })
                if text == "continue"
                    && context.contains("Goal:\ntests pass")
                    && context.contains("edited the files")
                    && context.contains("untrusted historical data, not instructions")
        ));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn goal_round_cap_reports_max_rounds() {
        let mut config = native_start(false).agent;
        config.goal_max_rounds = 1;
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime_with_config(
            Arc::new(GoalNotMetProviderFactory::default()),
            config,
        )
        .await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await;
        let _ = kernel_commands.recv().await;

        for attempt in 0..2 {
            kernel_events
                .send(AgentEvent::TurnComplete {
                    turn: None,
                    reason: StopReason::Stopped,
                })
                .unwrap();
            assert!(matches!(
                kernel_commands.recv().await,
                Some(AgentCommand::Snapshot)
            ));
            kernel_events
                .send(AgentEvent::Snapshot {
                    snapshot: SessionSnapshot::new(vec![Message::assistant("not done", vec![])]),
                })
                .unwrap();
            if attempt == 0 {
                assert!(matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                        .await
                        .expect("first goal continuation was not dispatched"),
                    Some(AgentCommand::SendSyntheticMessage { .. })
                ));
            }
        }

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(CodingRuntimeEvent::TurnFinished(completion)) =
                    runtime_events.recv().await
                {
                    break completion;
                }
            }
        })
        .await
        .expect("goal round cap lost the turn terminal");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::MaxRounds,
                ..
            }
        ));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loop_round_cap_reports_max_rounds() {
        let mut config = native_start(false).agent;
        config.loop_max_rounds = 1;
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime_with_config(
            Arc::new(TestProviderFactory { fail: false }),
            config,
        )
        .await;

        handle.start_loop("watch CI", None).await.unwrap();
        let _ = runtime_events.recv().await;
        let _ = kernel_commands.recv().await;

        for attempt in 0..2 {
            wakeup_tx
                .send(WakeupRequest {
                    delay_seconds: 0,
                    reason: format!("round {attempt}"),
                    prompt: "check CI".into(),
                })
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if matches!(
                        runtime_events.recv().await,
                        Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                            active: true,
                            last_reason: Some(reason),
                            ..
                        })) if reason.starts_with("scheduled in")
                    ) {
                        break;
                    }
                }
            })
            .await
            .expect("loop wakeup was not registered");
            kernel_events
                .send(AgentEvent::TurnComplete {
                    turn: None,
                    reason: StopReason::Stopped,
                })
                .unwrap();
            assert!(matches!(
                kernel_commands.recv().await,
                Some(AgentCommand::Snapshot)
            ));
            kernel_events
                .send(AgentEvent::Snapshot {
                    snapshot: SessionSnapshot::new(vec![Message::assistant("checked", vec![])]),
                })
                .unwrap();
            if attempt == 0 {
                assert!(matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        kernel_commands.recv()
                    )
                    .await
                    .expect("first loop continuation was not dispatched"),
                    Some(AgentCommand::SendMessage { text, .. }) if text == "check CI"
                ));
            }
        }

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(CodingRuntimeEvent::TurnFinished(completion)) =
                    runtime_events.recv().await
                {
                    break completion;
                }
            }
        })
        .await
        .expect("loop round cap lost the held terminal");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::MaxRounds,
                ..
            }
        ));

        handle.shutdown().await.unwrap();
    }

    /// A message typed while `/loop` waits, then esc: the turn it opened stops.
    ///
    /// While the loop waits for its next round this owner holds the finished
    /// turn open, and a typed message opens a new agent turn under that hold.
    /// The cancel used to close only the hold — a terminal for a turn that had
    /// already finished — and never told the agent, so the turn actually running
    /// carried on; a second esc then found nothing "active" and did nothing.
    #[tokio::test]
    async fn a_turn_opened_under_a_loop_hold_can_be_cancelled() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_loop("watch CI", None).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnStarted { turn: None })
            .unwrap();
        // The next round is an hour away: the hold stays up for this test.
        wakeup_tx
            .send(WakeupRequest {
                delay_seconds: 3600,
                reason: "later".into(),
                prompt: "check CI".into(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    runtime_events.recv().await,
                    Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                        last_reason: Some(reason),
                        ..
                    })) if reason.starts_with("scheduled in")
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the wakeup was not registered");
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("round one", vec![])]),
            })
            .unwrap();

        // Held. The person types, and the agent opens a turn for it.
        handle
            .submit(UserInput::from("while it waits"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "while it waits"
        ));
        kernel_events
            .send(AgentEvent::TurnStarted { turn: None })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    runtime_events.recv().await,
                    Some(CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { .. }))
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the typed turn's start was not forwarded");

        handle.cancel().await.unwrap();
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                    .await,
                Ok(Some(AgentCommand::Cancel))
            ),
            "the turn running under the hold was never told to stop"
        );
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Cancelled,
            })
            .unwrap();
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("while it waits")]),
            })
            .unwrap();

        let mut terminals = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(300), runtime_events.recv()).await
        {
            if let CodingRuntimeEvent::TurnFinished(completion) = event {
                terminals.push(completion);
            }
        }
        assert_eq!(terminals.len(), 1, "one stop, one terminal: {terminals:?}");
        assert!(matches!(
            terminals[0],
            TurnCompletion::Completed {
                reason: StopReason::Cancelled,
                ..
            }
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loop_continuation_send_failure_reports_provider_error() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            wakeup_tx,
            loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_loop("watch CI", None).await.unwrap();
        let _ = runtime_events.recv().await;
        let _ = kernel_commands.recv().await;
        wakeup_tx
            .send(WakeupRequest {
                delay_seconds: 0,
                reason: "retry".into(),
                prompt: "check CI".into(),
            })
            .unwrap();
        let _ = runtime_events.recv().await;
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        drop(kernel_commands);
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("checked", vec![])]),
            })
            .unwrap();

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(CodingRuntimeEvent::TurnFinished(completion)) =
                    runtime_events.recv().await
                {
                    break completion;
                }
            }
        })
        .await
        .expect("loop continuation failure lost the held terminal");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::ProviderError,
                ..
            }
        ));
        assert!(!loop_active.load(Ordering::Acquire));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);
    }

    #[tokio::test]
    async fn goal_snapshot_dispatch_failure_deactivates_controller() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await;
        let _ = kernel_commands.recv().await;
        drop(kernel_commands);
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();

        let mut saw_inactive_goal = false;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                        active: false, ..
                    })) => saw_inactive_goal = true,
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before snapshot failure terminal"),
                }
            }
        })
        .await
        .expect("snapshot dispatch failure lost the goal terminal");
        assert!(saw_inactive_goal, "snapshot failure must deactivate /goal");
        assert!(matches!(
            terminal,
            TurnCompletion::SnapshotUnavailable {
                reason: StopReason::ProviderError,
                ..
            }
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);
    }

    #[tokio::test]
    async fn loop_snapshot_dispatch_failure_clears_wakeup_and_active_state() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            wakeup_tx,
            loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_loop("watch CI", None).await.unwrap();
        let _ = runtime_events.recv().await;
        let _ = kernel_commands.recv().await;
        wakeup_tx
            .send(WakeupRequest {
                delay_seconds: 60,
                reason: "wait for CI".into(),
                prompt: "check CI".into(),
            })
            .unwrap();
        let _ = runtime_events.recv().await;
        drop(kernel_commands);
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();

        let mut saw_inactive_loop = false;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                        active: false, ..
                    })) => saw_inactive_loop = true,
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before snapshot failure terminal"),
                }
            }
        })
        .await
        .expect("snapshot dispatch failure lost the loop terminal");
        assert!(saw_inactive_loop, "snapshot failure must deactivate /loop");
        assert!(!loop_active.load(Ordering::Acquire));
        assert!(matches!(
            terminal,
            TurnCompletion::SnapshotUnavailable {
                reason: StopReason::ProviderError,
                ..
            }
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);
    }

    #[tokio::test]
    async fn goal_evaluator_continuation_is_a_synthetic_turn() {
        let factory = Arc::new(GoalNotMetProviderFactory::default());
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(factory.clone()).await;

        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "tests pass"
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("not done", vec![])]),
            })
            .unwrap();

        let continuation =
            tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                .await
                .expect("goal evaluator did not dispatch its continuation");
        assert!(matches!(
            continuation,
            Some(AgentCommand::SendSyntheticMessage { text })
                if text.contains("needs more work")
        ));
        assert_eq!(factory.builds.load(Ordering::SeqCst), 1);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn max_rounds_goal_terminal_is_evaluated_before_continuing() {
        let factory = Arc::new(GoalNotMetProviderFactory::default());
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(factory.clone()).await;

        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "tests pass"
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::MaxRounds,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("not done", vec![])]),
            })
            .unwrap();

        let continuation =
            tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                .await
                .expect("max-rounds goal terminal did not produce a continuation decision");
        assert_eq!(
            factory.builds.load(Ordering::SeqCst),
            1,
            "MaxRounds must enter the evaluator instead of the unproductive retry path"
        );
        assert!(matches!(
            continuation,
            Some(AgentCommand::SendSyntheticMessage { text })
                if text.contains("needs more work")
        ));

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tool_loop_detected_terminates_goal_without_continuation() {
        let factory = Arc::new(GoalNotMetProviderFactory::default());
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(factory.clone()).await;

        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "tests pass"
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::ToolLoopDetected,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("looping", vec![])]),
            })
            .unwrap();

        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: false,
                last_reason: Some(reason),
                ..
            })) if reason.contains("ToolLoopDetected")
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::Completed {
                    reason: StopReason::ToolLoopDetected,
                    ..
                }
            ))
        ));
        assert_eq!(
            factory.builds.load(Ordering::SeqCst),
            0,
            "tool-loop detection must not invoke the goal evaluator"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), kernel_commands.recv(),)
                .await
                .is_err(),
            "tool-loop detection must not dispatch a continuation"
        );

        handle.shutdown().await.unwrap();
    }

    struct PanicProviderFactory;

    impl CodingProviderFactory for PanicProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            use atomcode_kernel::provider::ChatOptions;
            use atomcode_kernel::stream::ProviderError;
            use atomcode_kernel::tool::ToolDef;

            struct PanicProvider;
            #[async_trait::async_trait]
            impl LlmProvider for PanicProvider {
                fn model_name(&self) -> &str {
                    "panic"
                }
                async fn chat_stream(
                    &self,
                    _messages: &[Message],
                    _tools: &[ToolDef],
                    _options: &ChatOptions,
                ) -> Result<
                    futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
                    ProviderError,
                > {
                    panic!("evaluator panic test")
                }
            }
            Ok(Arc::new(PanicProvider))
        }
    }

    #[tokio::test]
    async fn goal_evaluator_panic_produces_turn_finished_and_allows_shutdown() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(PanicProviderFactory)).await;

        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("not evaluated", vec![])]),
            })
            .unwrap();

        let mut saw_inactive_goal = false;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                        active: false, ..
                    })) => saw_inactive_goal = true,
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before evaluator terminal"),
                }
            }
        })
        .await
        .expect("evaluator panic did not produce a TurnFinished terminal");
        assert!(saw_inactive_goal, "evaluator panic must deactivate /goal");
        assert!(
            matches!(
                terminal,
                TurnCompletion::Completed {
                    reason: StopReason::ProviderError,
                    ..
                }
            ),
            "evaluator panic must produce ProviderError terminal"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loop_kernel_stream_failure_clears_wakeup_and_active_state() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            wakeup_tx,
            loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_loop("watch CI", None).await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: true,
                ..
            }))
        ));
        assert!(loop_active.load(Ordering::Acquire));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "watch CI"
        ));
        wakeup_tx
            .send(WakeupRequest {
                delay_seconds: 60,
                reason: "wait for CI".into(),
                prompt: "check CI".into(),
            })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: true,
                ..
            }))
        ));

        drop(kernel_events);
        let mut saw_inactive_loop = false;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                        active: false, ..
                    })) => saw_inactive_loop = true,
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime event stream closed before loop terminal"),
                }
            }
        })
        .await
        .expect("kernel stream failure lost the loop terminal");

        assert!(saw_inactive_loop, "abnormal terminal must deactivate /loop");
        assert!(
            !loop_active.load(Ordering::Acquire),
            "abnormal terminal must disable schedule_wakeup immediately"
        );
        assert!(matches!(
            terminal,
            TurnCompletion::SnapshotUnavailable {
                reason: StopReason::ProviderError,
                ..
            }
        ));
    }

    fn shutdown_reporting_agent(report_started: bool, report_compacted: bool) -> AgentHandle {
        shutdown_reporting_agent_with_snapshot(report_started, report_compacted, None)
    }

    fn shutdown_reporting_agent_with_snapshot(
        report_started: bool,
        report_compacted: bool,
        compacted_snapshot: Option<SessionSnapshot>,
    ) -> AgentHandle {
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut pending = None;
            let mut compacted_snapshot = compacted_snapshot;
            while let Some(command) = command_rx.recv().await {
                match command {
                    AgentCommand::Compact { focus } => {
                        let trigger = CompactTrigger::Manual { focus };
                        pending = Some(trigger.clone());
                        if report_started {
                            let _ = event_tx.send(AgentEvent::CompactionStarted { trigger });
                        }
                    }
                    AgentCommand::Shutdown => {
                        if report_compacted {
                            if let Some(trigger) = pending.take() {
                                let _ = event_tx.send(AgentEvent::Compacted {
                                    trigger,
                                    epoch: 1,
                                    removed: 2,
                                    bytes_before: 100,
                                    bytes_after: 50,
                                    committed: true,
                                    snapshot: compacted_snapshot.take(),
                                });
                            }
                        }
                        break;
                    }
                    _ => {}
                }
            }
        });
        AgentHandle {
            commands,
            events,
            task,
        }
    }

    fn shutdown_silent_agent() -> (AgentHandle, oneshot::Receiver<()>) {
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (delivered_tx, delivered_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _event_tx = event_tx;
            let mut delivered_tx = Some(delivered_tx);
            while let Some(command) = command_rx.recv().await {
                match command {
                    AgentCommand::Compact { .. } => {
                        if let Some(tx) = delivered_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    AgentCommand::Shutdown => break,
                    _ => {}
                }
            }
        });
        (
            AgentHandle {
                commands,
                events,
                task,
            },
            delivered_rx,
        )
    }

    #[test]
    fn outcome_scales_observed_usage_by_byte_ratio() {
        let outcome = CompactionOutcome::from_kernel(
            CompactTrigger::Auto { utilization: 0.8 },
            2,
            129,
            170_000,
            44_000,
            true,
            Some(42_900),
        );

        assert_eq!(outcome.estimated_tokens_after, 11_103);
    }

    #[test]
    fn silent_auto_tool_fold_only_for_auto_in_place_stubbing() {
        // Auto + no messages drained = pure in-place tool-output stubbing → silent.
        let auto_stub = CompactionOutcome::from_kernel(
            CompactTrigger::Auto { utilization: 0.5 },
            1,
            0,
            10_000,
            2_000,
            true,
            None,
        );
        assert!(auto_stub.is_silent_auto_tool_fold());

        // Auto drain (messages removed) is a real reduction → keep the mark.
        let auto_drain = CompactionOutcome::from_kernel(
            CompactTrigger::Auto { utilization: 0.8 },
            1,
            5,
            10_000,
            2_000,
            true,
            None,
        );
        assert!(!auto_drain.is_silent_auto_tool_fold());

        // Manual compactions always stay visible, even a stub-only one.
        let manual_stub = CompactionOutcome::from_kernel(
            CompactTrigger::Manual { focus: None },
            1,
            0,
            10_000,
            2_000,
            true,
            None,
        );
        assert!(!manual_stub.is_silent_auto_tool_fold());
    }

    #[test]
    fn outcome_falls_back_to_bytes_when_usage_is_missing() {
        let outcome = CompactionOutcome::from_kernel(
            CompactTrigger::Manual { focus: None },
            1,
            3,
            40_000,
            20_000,
            true,
            None,
        );

        assert_eq!(
            (
                outcome.estimated_tokens_before,
                outcome.estimated_tokens_after
            ),
            (10_000, 5_000)
        );
    }

    #[test]
    fn outcome_keeps_usage_when_input_bytes_are_zero() {
        let outcome = CompactionOutcome::from_kernel(
            CompactTrigger::Manual { focus: None },
            0,
            0,
            0,
            0,
            false,
            Some(5_000),
        );

        assert_eq!(outcome.estimated_tokens_after, 5_000);
    }

    #[tokio::test]
    async fn compact_emits_kernel_command() {
        let (handle, mut controls) = coding_runtime_control_channel();

        handle.compact(Some("recent tool output".into())).unwrap();

        assert!(matches!(
            controls.recv().await,
            Some(CodingRuntimeControl::Compact { focus: Some(focus), .. })
                if focus == "recent tool output"
        ));
    }

    #[tokio::test]
    async fn policy_resolution_is_runtime_owned_and_never_becomes_agent_input() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner_with_protocol(
            agent, controls, runtime_tx, true, true, None, None, None,
        );

        let intervention = PolicyIntervention::credential_shell_blocked();
        let intervention_id = intervention.id;
        kernel_events
            .send(AgentEvent::PolicyIntervention { intervention })
            .unwrap();
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::Agent(
                AgentEvent::PolicyIntervention { .. }
            ))
        ));

        assert_eq!(
            handle
                .submit(UserInput::from("must not start a new turn"))
                .await,
            Err(RuntimeError::Busy),
        );
        let unavailable_config = CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            "replacement",
            std::env::current_dir().unwrap(),
        );
        assert_eq!(
            handle.reassemble_provider(unavailable_config).await,
            Err(RuntimeError::Unavailable),
        );

        assert_eq!(
            handle
                .resolve_policy_intervention(
                    intervention_id,
                    PolicyRecoveryAction::ViewSafeInstructions,
                )
                .await,
            Err(RuntimeError::InvalidPolicyRecoveryAction)
        );
        assert!(runtime_rx.try_recv().is_err());

        handle
            .resolve_policy_intervention(intervention_id, PolicyRecoveryAction::SkipStep)
            .await
            .unwrap();
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::PolicyInterventionResolved {
                intervention_id: resolved_id,
                action: PolicyRecoveryAction::SkipStep
            }) if resolved_id == intervention_id
        ));
        assert!(kernel_commands.try_recv().is_err());

        let next_intervention = PolicyIntervention::credential_shell_blocked();
        let next_intervention_id = next_intervention.id;
        kernel_events
            .send(AgentEvent::PolicyIntervention {
                intervention: next_intervention,
            })
            .unwrap();
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::Agent(
                AgentEvent::PolicyIntervention { .. }
            ))
        ));
        assert_eq!(
            handle
                .resolve_policy_intervention(intervention_id, PolicyRecoveryAction::SkipStep)
                .await,
            Err(RuntimeError::NoPendingPolicyIntervention)
        );
        handle
            .resolve_policy_intervention(next_intervention_id, PolicyRecoveryAction::EndTask)
            .await
            .unwrap();

        handle.dispatch(DriverCommand::Shutdown).unwrap();
        let _ = adapter.owner_task.await;
    }

    #[test]
    fn closed_runtime_returns_typed_error() {
        let (handle, controls) = coding_runtime_control_channel();
        drop(controls);

        assert_eq!(handle.compact(None), Err(RuntimeUnavailable));
    }

    #[tokio::test]
    async fn owner_routes_compaction_without_exposing_it_to_adapter() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let mut adapter = spawn_runtime_owner(agent, controls, runtime_tx, true);

        handle.compact(Some("files".into())).unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Compact { focus: Some(focus) }) if focus == "files"
        ));

        kernel_events
            .send(AgentEvent::CompactionStarted {
                trigger: CompactTrigger::Manual {
                    focus: Some("files".into()),
                },
            })
            .unwrap();
        let committed_snapshot = SessionSnapshot::new(vec![Message::user("after compact")]);
        kernel_events
            .send(AgentEvent::Compacted {
                trigger: CompactTrigger::Manual {
                    focus: Some("files".into()),
                },
                epoch: 1,
                removed: 4,
                bytes_before: 400,
                bytes_after: 100,
                committed: true,
                snapshot: Some(committed_snapshot.clone()),
            })
            .unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionStarted { .. })
        ));
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome)
            })
                if outcome.committed
                    && outcome.removed_messages == 4
                    && outcome.committed_snapshot.as_deref() == Some(&committed_snapshot)
        ));
        assert!(adapter.events.try_recv().is_err());
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn owner_routes_checkpoint_failure_as_terminal_without_adapter_leak() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let mut adapter = spawn_runtime_owner(agent, controls, runtime_tx, true);

        handle.compact(None).unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Compact { focus: None })
        ));
        kernel_events
            .send(AgentEvent::CompactionFailed {
                trigger: CompactTrigger::Manual { focus: None },
                error: CompactionCheckpointError::new("read-only filesystem"),
            })
            .unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Failed { error, .. }
            }) if error.message() == "read-only filesystem"
        ));
        assert!(adapter.events.try_recv().is_err());
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stable_handle_targets_replacement_agent() {
        let (first, mut first_commands, _first_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        handle.compact(Some("before".into())).unwrap();
        assert!(matches!(
            first_commands.recv().await,
            Some(AgentCommand::Compact { focus: Some(focus) }) if focus == "before"
        ));

        let (second, mut second_commands, _second_events) = fake_agent();
        adapter.replace_agent(second).await.unwrap();
        handle.compact(Some("after".into())).unwrap();
        assert!(matches!(
            second_commands.recv().await,
            Some(AgentCommand::Compact { focus: Some(focus) }) if focus == "after"
        ));
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn suspended_runtime_rejects_until_replacement_is_resumed() {
        let (first, mut first_commands, _first_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        adapter.suspend_compaction().await.unwrap();
        assert_eq!(
            handle.compact(Some("during rebuild".into())),
            Err(RuntimeUnavailable)
        );
        assert!(first_commands.try_recv().is_err());

        let (second, mut second_commands, _second_events) = fake_agent();
        adapter.replace_agent(second).await.unwrap();
        assert_eq!(
            handle.compact(Some("before resume".into())),
            Err(RuntimeUnavailable)
        );
        assert!(second_commands.try_recv().is_err());

        adapter.resume_compaction().await.unwrap();
        handle.compact(Some("after rebuild".into())).unwrap();
        assert!(matches!(
            second_commands.recv().await,
            Some(AgentCommand::Compact { focus: Some(focus) }) if focus == "after rebuild"
        ));
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replace_drains_compacted_before_dropping_old_agent() {
        let first = shutdown_reporting_agent(true, true);
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        handle.compact(Some("old agent".into())).unwrap();
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionStarted { .. })
        ));

        let (second, _second_commands, _second_events) = fake_agent();
        adapter.replace_agent(second).await.unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome),
            }) if outcome.committed && outcome.removed_messages == 2
        ));
        assert!(runtime_rx.try_recv().is_err());
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stop_agent_drains_compacted_before_returning() {
        let first = shutdown_reporting_agent(true, true);
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        handle.compact(Some("before reload".into())).unwrap();
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionStarted { .. })
        ));

        adapter.stop_agent().await.unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome),
            }) if outcome.committed && outcome.removed_messages == 2
        ));
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stopped_agent_retains_a_committed_compaction_snapshot_and_revision() {
        let compacted = SessionSnapshot::new(vec![Message::user("compacted")]);
        let mut agent = Some(shutdown_reporting_agent_with_snapshot(
            true,
            true,
            Some(compacted.clone()),
        ));
        let trigger = CompactTrigger::Manual {
            focus: Some("before reload".into()),
        };
        agent
            .as_ref()
            .unwrap()
            .commands
            .send(AgentCommand::Compact {
                focus: Some("before reload".into()),
            })
            .unwrap();
        let mut compactions = CompactionTracker::default();
        compactions.accepted_manual(trigger);
        let mut observed_tokens = None;
        let (raw, _events) = mpsc::unbounded_channel();
        let emitter = RuntimeEventEmitter {
            raw,
            tagged: None,
            generation: Arc::new(AtomicU64::new(0)),
        };

        let report = stop_current_agent(
            &mut agent,
            &mut compactions,
            &mut observed_tokens,
            &emitter,
            CompactionInterruption::RuntimeReconfigured,
            None,
            None,
        )
        .await;

        assert!(report.conversation_changed);
        assert_eq!(report.snapshot, Some(compacted));
        let mut active_turn = None;
        let mut terminal_reason = None;
        let mut turn_stats = RuntimeTurnStats::default();
        let mut conversation_revision = 7;
        let mut snapshot_waiters = Vec::new();
        finish_stopped_native_turn(
            &report,
            None,
            &mut active_turn,
            &mut terminal_reason,
            &mut turn_stats,
            &mut conversation_revision,
            &mut snapshot_waiters,
            &emitter,
        );
        assert_eq!(conversation_revision, 8);
    }

    #[tokio::test]
    async fn stopping_runtime_agent_terminates_detached_team_members() {
        use atomcode_capabilities::team::{
            TeamDifficulty, TeamPermission, TeamRoleId, TeamTaskSpec,
        };

        let manager = crate::team::TeamRunManager::new(crate::team::TeamRuntimeConfig {
            cancel_grace: std::time::Duration::from_millis(1),
            ..Default::default()
        });
        manager.begin_generation(4);
        let factory: crate::team::TeamJobFactory =
            Arc::new(|_, _, _| Box::pin(std::future::pending::<crate::team::TeamMemberOutcome>()));
        let run = manager
            .delegate(
                vec![TeamTaskSpec {
                    description: "inspect".into(),
                    prompt: "inspect".into(),
                    role: TeamRoleId::Explorer,
                    permission: TeamPermission::Explore,
                    difficulty: TeamDifficulty::Simple,
                    scope: Vec::new(),
                }],
                factory,
                Arc::new(|_| "test-model".to_string()),
            )
            .await
            .unwrap();
        tokio::task::yield_now().await;

        let mut agent = None;
        let mut compactions = CompactionTracker::default();
        let mut observed_tokens = None;
        let (raw, _events) = mpsc::unbounded_channel();
        let emitter = RuntimeEventEmitter {
            raw,
            tagged: None,
            generation: Arc::new(AtomicU64::new(4)),
        };
        stop_current_agent(
            &mut agent,
            &mut compactions,
            &mut observed_tokens,
            &emitter,
            CompactionInterruption::RuntimeShutdown,
            Some(&manager),
            None,
        )
        .await;

        assert!(
            manager
                .wait(&run, std::time::Duration::from_millis(20))
                .await
                .unwrap()
                .terminal
        );
        assert_eq!(manager.snapshot(Some(&run)).unwrap().runs[0].stopped, 1);
    }

    #[tokio::test]
    async fn reassembling_provider_preserves_detached_team_members() {
        use atomcode_capabilities::team::{
            TeamDifficulty, TeamPermission, TeamRoleId, TeamTaskSpec,
        };

        let manager = crate::team::TeamRunManager::new(crate::team::TeamRuntimeConfig {
            cancel_grace: std::time::Duration::from_millis(1),
            ..Default::default()
        });
        manager.begin_generation(4);
        let factory: crate::team::TeamJobFactory =
            Arc::new(|_, _, _| Box::pin(std::future::pending::<crate::team::TeamMemberOutcome>()));
        let run = manager
            .delegate(
                vec![TeamTaskSpec {
                    description: "inspect".into(),
                    prompt: "inspect".into(),
                    role: TeamRoleId::Explorer,
                    permission: TeamPermission::Explore,
                    difficulty: TeamDifficulty::Simple,
                    scope: Vec::new(),
                }],
                factory,
                Arc::new(|_| "test-model".to_string()),
            )
            .await
            .unwrap();
        tokio::task::yield_now().await;

        let mut agent = None;
        let mut compactions = CompactionTracker::default();
        let mut observed_tokens = None;
        let (raw, _events) = mpsc::unbounded_channel();
        let emitter = RuntimeEventEmitter {
            raw,
            tagged: None,
            generation: Arc::new(AtomicU64::new(4)),
        };
        // A `/model` reassemble passes `None` for the manager: the session
        // continues, so the detached member must NOT be terminated.
        stop_current_agent(
            &mut agent,
            &mut compactions,
            &mut observed_tokens,
            &emitter,
            CompactionInterruption::RuntimeReconfigured,
            None,
            None,
        )
        .await;

        // The member is still pending (std::future::pending), so the run is NOT terminal.
        assert!(
            !manager
                .wait(&run, std::time::Duration::from_millis(20))
                .await
                .unwrap()
                .terminal,
            "reassemble must preserve in-flight team members"
        );
        assert_eq!(manager.snapshot(Some(&run)).unwrap().runs[0].stopped, 0);
    }

    #[tokio::test]
    async fn undo_rejects_an_accepted_compaction_before_mutating_state() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner_with_protocol(
            agent, controls, runtime_tx, true, true, None, None, None,
        );
        let snapshot_task = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.snapshot_with_revision().await.unwrap() })
        };
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        let original_snapshot = SessionSnapshot::new(vec![Message::user("first prompt")]);
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: original_snapshot,
            })
            .unwrap();
        let original = snapshot_task.await.unwrap();
        let undo = undo_snapshot_to_prompt(&original.snapshot, None).unwrap();

        handle.compact(Some("in flight".into())).unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Compact { .. })
        ));
        let (done, result) = oneshot::channel();
        handle
            .tx
            .send(CodingRuntimeControl::ApplyUndo {
                code_rewound_to: None,
                generation: handle.status().generation,
                expected_revision: original.revision,
                original: original.snapshot,
                truncated: undo.snapshot,
                restored_prompt: undo.restored_prompt,
                target_n: undo.target_n,
                prompts_before: undo.prompts_before,
                done,
            })
            .unwrap();

        assert!(matches!(result.await.unwrap(), Err(RuntimeError::Busy)));
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replace_interrupts_started_compaction_without_kernel_terminal() {
        let first = shutdown_reporting_agent(true, false);
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        handle.compact(Some("old agent".into())).unwrap();
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionStarted { .. })
        ));

        let (second, _second_commands, _second_events) = fake_agent();
        adapter.replace_agent(second).await.unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Interrupted {
                    trigger: CompactTrigger::Manual { focus: Some(focus) },
                    reason: CompactionInterruption::RuntimeReconfigured,
                },
            }) if focus == "old agent"
        ));
        assert!(runtime_rx.try_recv().is_err());
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_interrupts_started_compaction_with_shutdown_reason() {
        let first = shutdown_reporting_agent(true, false);
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        handle.compact(None).unwrap();
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionStarted { .. })
        ));
        adapter.shutdown().await.unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Interrupted {
                    trigger: CompactTrigger::Manual { focus: None },
                    reason: CompactionInterruption::RuntimeShutdown,
                },
            })
        ));
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn shutdown_interrupts_control_accepted_but_not_yet_delivered() {
        let (agent, mut kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(agent, controls, runtime_tx, true);

        handle
            .compact(Some("queued before shutdown".into()))
            .unwrap();
        adapter.shutdown().await.unwrap();

        assert!(matches!(
            kernel_commands.try_recv(),
            Ok(AgentCommand::Shutdown)
        ));
        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Interrupted {
                    trigger: CompactTrigger::Manual { focus: Some(focus) },
                    reason: CompactionInterruption::RuntimeShutdown,
                },
            }) if focus == "queued before shutdown"
        ));
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn replace_interrupts_delivered_compaction_that_never_started() {
        let (first, delivered) = shutdown_silent_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        handle.compact(Some("queued in old agent".into())).unwrap();
        delivered.await.unwrap();
        let (second, _second_commands, _second_events) = fake_agent();
        adapter.replace_agent(second).await.unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Interrupted {
                    trigger: CompactTrigger::Manual { focus: Some(focus) },
                    reason: CompactionInterruption::RuntimeReconfigured,
                },
            }) if focus == "queued in old agent"
        ));
        assert!(runtime_rx.try_recv().is_err());
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stale_generation_is_interrupted_instead_of_reaching_replacement() {
        let (first, _first_commands, _first_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let stale_generation = runtime_state_generation(handle.state.load(Ordering::Acquire));
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(first, controls, runtime_tx, true);

        adapter.suspend_compaction().await.unwrap();
        handle
            .tx
            .send(CodingRuntimeControl::Compact {
                generation: stale_generation,
                focus: Some("stale".into()),
            })
            .unwrap();
        let (second, mut second_commands, _second_events) = fake_agent();
        adapter.replace_agent(second).await.unwrap();
        adapter.resume_compaction().await.unwrap();

        assert!(matches!(
            runtime_rx.recv().await,
            Some(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Interrupted {
                    trigger: CompactTrigger::Manual { focus: Some(focus) },
                    reason: CompactionInterruption::RuntimeReconfigured,
                },
            }) if focus == "stale"
        ));
        assert!(second_commands.try_recv().is_err());
        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stopped_or_degraded_runtime_rejects_compaction() {
        let (agent, _commands, _events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(agent, controls, runtime_tx, false);

        assert_eq!(handle.compact(None), Err(RuntimeUnavailable));
        assert!(!handle.accepts(&DriverCommand::Submit(UserInput::from("blocked"))));
        assert!(
            handle.accepts(&DriverCommand::ReloadProvider(CodingAgentConfig::new(
                "key",
                "https://example.test/v1",
                "model",
                ".",
            )))
        );
        assert!(handle.accepts(&DriverCommand::Shutdown));
        assert!(runtime_rx.try_recv().is_err());

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_handle_shutdown_waiters_share_one_terminal_result() {
        let (agent, mut kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
        let _adapter = spawn_runtime_owner(agent, controls, runtime_tx, true);

        let first = handle.clone();
        let second = handle.clone();
        let (first_result, second_result) = tokio::join!(first.shutdown(), second.shutdown());

        assert_eq!(first_result, second_result);
        assert_eq!(
            first_result.unwrap().reason,
            RuntimeExitReason::ShutdownRequested
        );
        assert!(matches!(
            kernel_commands.try_recv(),
            Ok(AgentCommand::Shutdown)
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Stopped);
        assert_eq!(handle.compact(None), Err(RuntimeUnavailable));
    }

    #[tokio::test]
    async fn shutdown_after_owner_stopped_returns_the_recorded_terminal() {
        let (agent, _kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner(agent, controls, runtime_tx, true);

        adapter.shutdown().await.unwrap();

        assert_eq!(
            handle.shutdown().await.unwrap().reason,
            RuntimeExitReason::ShutdownRequested
        );
        assert_eq!(handle.status().phase, RuntimePhase::Stopped);
    }

    #[tokio::test]
    async fn native_start_owns_agent_and_emits_sequenced_shutdown_terminal() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);

        let exit = runtime.handle.shutdown().await.unwrap();
        let terminal = runtime.events.recv().await.unwrap();

        assert_eq!(terminal.generation, 0);
        assert_eq!(terminal.sequence, 0);
        assert!(matches!(
            terminal.event,
            CodingRuntimeEvent::RuntimeStopped(event_exit) if event_exit == exit
        ));
        assert_eq!(runtime.task.await.unwrap(), exit);
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn runtime_replays_a_safe_recovered_prompt_through_a_normal_turn() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let dir = tempfile::tempdir().unwrap();
        let id = "resume-safe-prompt";
        let manager = atomcode_capabilities::session::SessionManager::for_project(dir.path());
        let canonical = SessionSnapshot::new(vec![Message::user("completed")]);
        persist_native_session(&manager, id, dir.path(), &canonical);
        let inflight = SessionSnapshot::new(vec![
            Message::user("completed"),
            Message::user("continue after crash"),
        ]);
        std::fs::write(
            manager.root().join(format!("{id}.snapshot.inflight")),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "replay_safe": true,
                "snapshot": inflight,
            }))
            .unwrap(),
        )
        .unwrap();

        let mut start = native_start(false);
        start.agent.working_dir = dir.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(id.to_string());
        let mut runtime = CodingRuntime::start(start).await.unwrap();

        wait_for_turn_finished(&mut runtime).await;
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();

        let loaded = manager.load_native_session(id).unwrap();
        assert!(loaded
            .snapshot
            .messages
            .iter()
            .any(|message| message.text == "continue after crash"));
        assert!(
            !manager
                .root()
                .join(format!("{id}.snapshot.inflight"))
                .exists(),
            "a completed replay must clear its recovery checkpoint"
        );
    }

    #[tokio::test]
    async fn native_start_returns_provider_error_without_degraded_handle() {
        assert!(matches!(
            CodingRuntime::start(native_start(true)).await,
            Err(RuntimeStartError::Provider(crate::ProviderBuildError::Adapter(message)))
                if message == "expected failure"
        ));
    }

    #[tokio::test]
    async fn recoverable_auth_gap_starts_awaiting_provider_and_can_reassemble() {
        let factory = Arc::new(RecoverableAuthFactory {
            fail: std::sync::atomic::AtomicBool::new(true),
        });
        let mut start = native_start(false);
        start.provider_factory = factory.clone();

        let runtime =
            CodingRuntime::start_with_bootstrap(start, ProviderBootstrap::RecoverAuthentication)
                .await
                .unwrap();

        assert_eq!(
            runtime.handle.status().phase,
            RuntimePhase::AwaitingProvider
        );
        assert_eq!(
            runtime.handle.provider_unavailable_reason(),
            Some(ProviderUnavailableReason::AuthenticationRequired)
        );
        assert!(matches!(
            runtime.handle.submit(UserInput::from("blocked")).await,
            Err(RuntimeError::ProviderUnavailable(
                ProviderUnavailableReason::AuthenticationRequired
            ))
        ));

        factory.fail.store(false, Ordering::Release);
        let next = CodingAgentConfig::new("key", "https://example.test/v1", "ready", ".");
        assert_eq!(
            runtime.handle.reassemble_provider(next).await.unwrap(),
            RuntimeGeneration(1)
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        assert_eq!(runtime.handle.provider_unavailable_reason(), None);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn source_build_gateway_gap_starts_awaiting_provider_and_can_switch() {
        let mut start = native_start(false);
        start.agent.base_url = "https://llm-api.atomgit.com/v1".into();
        start.provider_factory = Arc::new(SourceBuildGatewayFactory);

        let runtime =
            CodingRuntime::start_with_bootstrap(start, ProviderBootstrap::RecoverAuthentication)
                .await
                .unwrap();

        assert_eq!(
            runtime.handle.status().phase,
            RuntimePhase::AwaitingProvider
        );
        assert_eq!(
            runtime.handle.provider_unavailable_reason(),
            Some(ProviderUnavailableReason::UnsupportedBuild)
        );
        assert!(matches!(
            runtime.handle.submit(UserInput::from("blocked")).await,
            Err(RuntimeError::ProviderUnavailable(
                ProviderUnavailableReason::UnsupportedBuild
            ))
        ));

        let next = CodingAgentConfig::new("key", "https://example.test/v1", "ready", ".");
        assert_eq!(
            runtime.handle.reassemble_provider(next).await.unwrap(),
            RuntimeGeneration(1)
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        assert_eq!(runtime.handle.provider_unavailable_reason(), None);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn required_source_build_gateway_gap_remains_startup_error() {
        let mut start = native_start(false);
        start.agent.base_url = "https://llm-api.atomgit.com/v1".into();
        start.provider_factory = Arc::new(SourceBuildGatewayFactory);

        assert!(matches!(
            CodingRuntime::start_with_bootstrap(start, ProviderBootstrap::Required).await,
            Err(RuntimeStartError::Provider(
                crate::ProviderBuildError::SourceBuildGatewayUnsupported { base_url }
            )) if base_url == "https://llm-api.atomgit.com/v1"
        ));
    }

    #[tokio::test]
    async fn deactivate_provider_drops_ready_agent_and_allows_recovery() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();

        assert_eq!(
            runtime
                .handle
                .deactivate_provider(ProviderUnavailableReason::AuthenticationRequired)
                .await
                .unwrap(),
            RuntimeGeneration(1)
        );
        assert_eq!(
            runtime.handle.status().phase,
            RuntimePhase::AwaitingProvider
        );
        let unavailable = loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(1), runtime.events.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if let CodingRuntimeEvent::ProviderUnavailable { reason, forced } = event.event {
                break (reason, forced);
            }
        };
        assert_eq!(
            unavailable,
            (ProviderUnavailableReason::AuthenticationRequired, false)
        );
        assert!(matches!(
            runtime.handle.submit(UserInput::from("blocked")).await,
            Err(RuntimeError::ProviderUnavailable(
                ProviderUnavailableReason::AuthenticationRequired
            ))
        ));

        let next = CodingAgentConfig::new("key", "https://example.test/v1", "ready", ".");
        assert_eq!(
            runtime.handle.reassemble_provider(next).await.unwrap(),
            RuntimeGeneration(2)
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    /// Recovery has to hand back a runtime that can actually run a turn.
    ///
    /// The scenario above stops at `phase == Ready`, and that is not the same
    /// claim: a deactivate STOPS the agent, so a recovery path that only put a
    /// new provider in place would report Ready with nothing behind it. On the
    /// harness engine that is a live hazard — a model swap there is a patch,
    /// and a patch cannot bring back an agent that was torn down — so this
    /// submits after recovering and insists the turn starts.
    #[tokio::test]
    async fn a_recovered_runtime_can_actually_run_a_turn() {
        let runtime = CodingRuntime::start(native_start(false)).await.unwrap();
        runtime
            .handle
            .deactivate_provider(ProviderUnavailableReason::AuthenticationRequired)
            .await
            .unwrap();
        let next = CodingAgentConfig::new("key", "https://example.test/v1", "after-login", ".");
        runtime.handle.reassemble_provider(next).await.unwrap();

        let receipt = runtime.handle.submit(UserInput::from("after login")).await;
        assert!(
            matches!(receipt, Ok(SubmitReceipt::Started { .. })),
            "a recovered runtime reported Ready but could not start a turn: {receipt:?}"
        );
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_turn_steers_and_finishes_only_after_real_snapshot() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let _adapter = spawn_runtime_owner_with_protocol(
            agent, controls, runtime_tx, true, true, None, None, None,
        );

        assert_eq!(
            handle.submit(UserInput::from("first")).await.unwrap(),
            SubmitReceipt::Started {
                generation: 0,
                turn_id: 1,
            }
        );
        assert_eq!(
            handle.submit(UserInput::from("steer")).await.unwrap(),
            SubmitReceipt::Steered {
                generation: 0,
                turn_id: 1,
            }
        );
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "first"
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "steer"
        ));

        kernel_events
            .send(AgentEvent::TurnStarted { turn: None })
            .unwrap();
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { .. }))
        ));
        assert!(runtime_events.try_recv().is_err());

        let expected = SessionSnapshot::new(vec![Message::user("persisted")]);
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: expected.clone(),
            })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                turn_id: 1,
                reason: StopReason::Stopped,
                snapshot,
                ..
            })) if snapshot.as_ref() == &expected
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);
        handle.shutdown().await.unwrap();
    }

    /// `/model` in the middle of a turn, then esc: the stop still reaches the
    /// turn.
    ///
    /// On the harness a model switch is a patch — the turn runs on under the new
    /// generation. The phase was left at `Reconfiguring` with the *old*
    /// generation, so `cancel()` stamped every request with a generation the
    /// owner no longer had and each was refused as stale ("unavailable") while
    /// the turn carried on.
    #[tokio::test]
    async fn a_model_switched_mid_turn_leaves_the_turn_stoppable() {
        let mut start = native_start(false);
        start.provider_factory = Arc::new(PendingProviderFactory);
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        runtime
            .handle
            .submit(UserInput::from("a long answer"))
            .await
            .unwrap();

        let next = CodingAgentConfig::new("key", "https://example.test/v1", "after-switch", ".");
        runtime.handle.reassemble_provider(next).await.unwrap();
        assert_eq!(
            runtime.handle.status().phase,
            RuntimePhase::InTurn,
            "the patch did not end the turn, so the phase must still say it runs"
        );

        runtime
            .handle
            .cancel()
            .await
            .expect("a cancel after a mid-turn model switch was refused as stale");
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let CodingRuntimeEvent::TurnFinished(completion) =
                    runtime.events.recv().await.unwrap().event
                {
                    break completion;
                }
            }
        })
        .await
        .expect("the cancelled turn never ended");
        assert!(
            matches!(
                terminal,
                TurnCompletion::Completed {
                    reason: StopReason::Cancelled,
                    ..
                }
            ),
            "{terminal:?}"
        );
        runtime.handle.shutdown().await.unwrap();
    }

    /// Stopping the loop does not stop the turn the person opened under it.
    ///
    /// The hold is this owner's bookkeeping, not work: with a turn the agent
    /// opened under it, closing the hold reported a terminal for a turn that had
    /// already finished and left `active_turn` empty — so the running one was
    /// accounted for by nobody, and the second terminal it produced named turn 0.
    /// `/loop` stopping is not a reason to stop what the person typed, so the
    /// hold goes and the running turn keeps the id it had.
    #[tokio::test]
    async fn stopping_the_loop_leaves_the_turn_opened_under_it_running() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(TestProviderFactory { fail: false })).await;

        handle.start_loop("watch CI", None).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnStarted { turn: None })
            .unwrap();
        wakeup_tx
            .send(WakeupRequest {
                delay_seconds: 3600,
                reason: "later".into(),
                prompt: "check CI".into(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    runtime_events.recv().await,
                    Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                        last_reason: Some(reason),
                        ..
                    })) if reason.starts_with("scheduled in")
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the wakeup was not registered");
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("round one", vec![])]),
            })
            .unwrap();

        // Held, and the person types; the agent opens a turn for it.
        handle
            .submit(UserInput::from("while it waits"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnStarted { turn: None })
            .unwrap();

        handle.stop_loop().await.unwrap();
        // Nothing was asked of the agent: what is running is the person's own.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                kernel_commands.recv()
            )
            .await
            .is_err(),
            "stopping the loop stopped the turn the person had opened"
        );

        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("answered", vec![])]),
            })
            .unwrap();

        let mut terminals = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(300), runtime_events.recv()).await
        {
            if let CodingRuntimeEvent::TurnFinished(completion) = event {
                terminals.push(completion);
            }
        }
        assert_eq!(
            terminals.len(),
            1,
            "one turn ran under the hold, so one terminal: {terminals:?}"
        );
        assert!(
            matches!(
                terminals[0],
                TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Stopped,
                    ..
                }
            ),
            "{terminals:?}"
        );
        handle.shutdown().await.unwrap();
    }

    /// A picture being read is not a reason esc cannot be heard.
    ///
    /// Recognition runs inside the owner's own loop, and it is a model call with
    /// no overall cap — so a `Cancel` sent while a pasted image was being read
    /// sat unread on the channel for the whole call, and the person watched
    /// nothing happen. The stop is fired before the command now, which is what
    /// lets the await see it.
    #[tokio::test]
    async fn a_stop_is_heard_while_a_pasted_picture_is_being_read() {
        struct NeverReads;

        #[async_trait::async_trait]
        impl ImagePreprocessor for NeverReads {
            async fn preprocess(
                &self,
                _text: String,
                _images: Vec<atomcode_kernel::message::ImageContent>,
                _supports_vision: bool,
                _session_id: Option<String>,
            ) -> (UserInput, Option<VisionNotice>) {
                std::future::pending().await
            }
        }

        let (agent, mut kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, _runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            plugin_hooks,
            provider_factory,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: Some(Arc::new(NeverReads)),
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        let submitted = handle.submit(UserInput {
            text: "what is in this".into(),
            images: vec![atomcode_kernel::message::ImageContent {
                media_type: "image/png".into(),
                data: "x".into(),
            }],
        });
        // The submit itself does not come back until recognition does — it is
        // the same await. What must not wait is the stop.
        tokio::pin!(submitted);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut submitted)
                .await
                .is_err(),
            "the control: the picture is still being read"
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), handle.cancel())
            .await
            .expect("the stop waited for the picture to be read")
            .expect("the stop was refused");
        assert!(
            kernel_commands.try_recv().is_err()
                || !matches!(
                    kernel_commands.try_recv(),
                    Ok(AgentCommand::SendMessage { .. })
                ),
            "the turn the person stopped must not reach the agent"
        );
        handle.shutdown().await.unwrap();
    }

    /// A turn the agent opened itself is still one a person can stop.
    ///
    /// Not every turn comes through `submit`: a catalog command — `/init`,
    /// `/worklog`, a skill such as `/setup` — puts its prompt straight into the
    /// agent's inbox, and the agent wakes and runs it. This owner used to learn
    /// of such a turn only as a phase (`InTurn`), with no `active_turn`, so a
    /// cancel read it as idle, answered `Ok` and never told the kernel: esc
    /// said 正在停止 while the turn ran on to its end, and quitting waited out
    /// its timeout for a terminal that was not coming.
    #[tokio::test]
    async fn a_turn_the_agent_started_itself_can_still_be_cancelled() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let _adapter = spawn_runtime_owner_with_protocol(
            agent, controls, runtime_tx, true, true, None, None, None,
        );

        // No submit: the turn is the agent's own.
        kernel_events
            .send(AgentEvent::TurnStarted { turn: None })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { .. }))
        ));

        handle.cancel().await.unwrap();
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                    .await,
                Ok(Some(AgentCommand::Cancel))
            ),
            "the cancel was answered but never reached the agent"
        );
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));

        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Cancelled,
            })
            .unwrap();
        let kept = SessionSnapshot::new(vec![Message::user("/setup")]);
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: kept.clone(),
            })
            .unwrap();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before the cancel's terminal"),
                }
            }
        })
        .await
        .expect("a cancelled turn must end in a terminal the driver can see");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::Cancelled,
                snapshot,
                ..
            } if snapshot.as_ref() == &kept
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_turn_gets_terminal_when_kernel_event_stream_closes_early() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let _adapter = spawn_runtime_owner_with_protocol(
            agent, controls, runtime_tx, true, true, None, None, None,
        );

        assert!(matches!(
            handle.submit(UserInput::from("accepted")).await.unwrap(),
            SubmitReceipt::Started { turn_id: 1, .. }
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "accepted"
        ));

        drop(kernel_events);

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), runtime_events.recv())
                .await
                .expect("missing terminal after kernel event stream closed"),
            Some(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::SnapshotUnavailable {
                    turn_id: 1,
                    reason: StopReason::ProviderError,
                    ..
                }
            ))
        ));
    }

    #[tokio::test]
    async fn goal_evaluator_failure_finishes_the_held_snapshot_without_failing_runtime() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        let expected = SessionSnapshot::new(vec![Message::user("persisted")]);
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: expected.clone(),
            })
            .unwrap();
        drop(kernel_commands);

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(CodingRuntimeEvent::TurnFinished(completion)) =
                    runtime_events.recv().await
                {
                    break completion;
                }
            }
        })
        .await
        .expect("evaluator failure lost the held turn terminal");

        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                turn_id: 1,
                reason: StopReason::ProviderError,
                snapshot,
                ..
            } if snapshot.as_ref() == &expected
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);
    }

    // An `ImagePreprocessor` that fails: clears images from the model request
    // and returns a Failed notice (as the real CLI adapter does on VL failure).
    struct FailingPreprocessor;
    #[async_trait::async_trait]
    impl ImagePreprocessor for FailingPreprocessor {
        async fn preprocess(
            &self,
            text: String,
            _images: Vec<ImageContent>,
            _supports_vision: bool,
            _session_id: Option<String>,
        ) -> (UserInput, Option<VisionNotice>) {
            (
                UserInput {
                    text: format!("{text}\n\n[图片识别失败]"),
                    images: Vec::new(),
                },
                Some(VisionNotice::Failed {
                    reason: "boom".into(),
                }),
            )
        }
    }

    // A recording `ImagePreprocessor` that folds images into text (mimicking a
    // VL description) and clears them, flagging whether it was ever called.
    struct RecordingPreprocessor {
        called: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl ImagePreprocessor for RecordingPreprocessor {
        async fn preprocess(
            &self,
            text: String,
            _images: Vec<ImageContent>,
            _supports_vision: bool,
            _session_id: Option<String>,
        ) -> (UserInput, Option<VisionNotice>) {
            self.called.store(true, Ordering::Release);
            (
                UserInput {
                    text: format!("VL[{text}]"),
                    images: Vec::new(),
                },
                Some(VisionNotice::Recognised {
                    vl_model: "vl".into(),
                    char_count: 3,
                }),
            )
        }
    }

    struct ConcurrentPreprocessor {
        active: Arc<std::sync::atomic::AtomicUsize>,
        max_active: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ImagePreprocessor for ConcurrentPreprocessor {
        async fn preprocess(
            &self,
            text: String,
            _images: Vec<ImageContent>,
            _supports_vision: bool,
            _session_id: Option<String>,
        ) -> (UserInput, Option<VisionNotice>) {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active.fetch_max(active, Ordering::AcqRel);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::AcqRel);
            (
                UserInput {
                    text: format!("VL[{text}]"),
                    images: Vec::new(),
                },
                None,
            )
        }
    }

    async fn spawn_with_preprocessor(
        pp: Option<Arc<dyn ImagePreprocessor>>,
        supports_vision: bool,
    ) -> (
        CodingRuntimeHandle,
        mpsc::UnboundedReceiver<AgentCommand>,
        mpsc::UnboundedSender<AgentEvent>,
        mpsc::UnboundedReceiver<CodingRuntimeEvent>,
        KernelRuntimeAdapter,
    ) {
        let (agent, kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: mut config,
            prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        config.supports_vision = supports_vision;
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: pp,
        };
        let adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );
        (
            handle,
            kernel_commands,
            kernel_events,
            runtime_events,
            adapter,
        )
    }

    // The installed preprocessor runs on an image-carrying submit, and its
    // rewritten `(text, images)` — not the raw input — is what reaches the
    // kernel. This is the seam that restores TUI VL image recognition.
    #[tokio::test]
    async fn image_submit_runs_installed_preprocessor_before_kernel() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, mut kernel_commands, _kernel_events, mut runtime_events, _adapter) =
            spawn_with_preprocessor(
                Some(Arc::new(RecordingPreprocessor {
                    called: called.clone(),
                })),
                false,
            )
            .await;

        handle
            .submit(UserInput {
                text: "look at this".into(),
                images: vec![ImageContent {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
            })
            .await
            .unwrap();

        match kernel_commands.recv().await {
            Some(AgentCommand::SendMessage { text, images }) => {
                assert_eq!(
                    text, "VL[look at this]",
                    "preprocessor output must reach the kernel"
                );
                assert!(
                    images.is_empty(),
                    "images must be cleared after preprocessing"
                );
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
        assert!(called.load(Ordering::Acquire), "preprocessor must have run");

        // The recognition notice must be emitted so the driver can render the
        // "✓ VL recognised image, returned N chars" status line.
        let mut saw_success = false;
        while let Ok(ev) = runtime_events.try_recv() {
            if let CodingRuntimeEvent::VisionPreprocessSuccess { char_count, .. } = ev {
                assert_eq!(char_count, 3);
                saw_success = true;
            }
        }
        assert!(
            saw_success,
            "runtime must emit VisionPreprocessSuccess for the toast"
        );
    }

    #[tokio::test]
    async fn request_user_input_image_response_runs_preprocessor_for_text_model() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, mut kernel_commands, kernel_events, mut runtime_events, _adapter) =
            spawn_with_preprocessor(
                Some(Arc::new(RecordingPreprocessor {
                    called: called.clone(),
                })),
                false,
            )
            .await;

        kernel_events
            .send(AgentEvent::Request {
                id: 7,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: serde_json::json!({"question": "show me"}),
            })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Request(RuntimeRequest { id: 7, .. }))
        ));

        let response = UserInputResponse {
            declined: false,
            selected: vec!["[Image #1]".into()],
            text: None,
            images: vec![test_image("raw-image")],
        };
        handle
            .respond(7, serde_json::to_value(response).unwrap())
            .await
            .unwrap();

        match kernel_commands.recv().await {
            Some(AgentCommand::Respond { id: 7, value }) => {
                let response: UserInputResponse = serde_json::from_value(value).unwrap();
                assert_eq!(response.selected, vec!["[Image #1]"]);
                assert_eq!(response.text.as_deref(), Some("VL[]"));
                assert!(response.images.is_empty());
            }
            other => panic!("expected preprocessed Respond, got {other:?}"),
        }
        assert!(called.load(Ordering::Acquire));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::VisionPreprocessSuccess { char_count: 3, .. })
        ));
    }

    #[tokio::test]
    async fn request_user_input_batch_images_preprocess_concurrently_in_original_order() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (handle, mut kernel_commands, kernel_events, mut runtime_events, _adapter) =
            spawn_with_preprocessor(
                Some(Arc::new(ConcurrentPreprocessor {
                    active: active.clone(),
                    max_active: max_active.clone(),
                })),
                false,
            )
            .await;

        kernel_events
            .send(AgentEvent::Request {
                id: 9,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: serde_json::json!({"question": "show me"}),
            })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Request(RuntimeRequest { id: 9, .. }))
        ));

        let responses = vec![
            UserInputResponse {
                declined: false,
                selected: Vec::new(),
                text: Some("first".into()),
                images: vec![test_image("first-image")],
            },
            UserInputResponse {
                declined: false,
                selected: Vec::new(),
                text: Some("second".into()),
                images: vec![test_image("second-image")],
            },
        ];
        handle
            .respond(9, serde_json::json!({ "responses": responses }))
            .await
            .unwrap();

        match kernel_commands.recv().await {
            Some(AgentCommand::Respond { id: 9, value }) => {
                let responses = value["responses"].as_array().unwrap();
                assert_eq!(responses[0]["text"], "VL[first]");
                assert_eq!(responses[1]["text"], "VL[second]");
            }
            other => panic!("expected preprocessed batch Respond, got {other:?}"),
        }
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert!(
            max_active.load(Ordering::Acquire) >= 2,
            "batch image preprocessing should overlap"
        );
    }

    #[tokio::test]
    async fn request_user_input_image_response_preserves_native_vision_images() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, mut kernel_commands, kernel_events, mut runtime_events, _adapter) =
            spawn_with_preprocessor(
                Some(Arc::new(RecordingPreprocessor {
                    called: called.clone(),
                })),
                true,
            )
            .await;
        kernel_events
            .send(AgentEvent::Request {
                id: 8,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: serde_json::json!({"question": "show me"}),
            })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Request(RuntimeRequest { id: 8, .. }))
        ));

        let response = UserInputResponse {
            declined: false,
            selected: vec!["[Image #1]".into()],
            text: None,
            images: vec![test_image("raw-image")],
        };
        handle
            .respond(8, serde_json::to_value(&response).unwrap())
            .await
            .unwrap();
        match kernel_commands.recv().await {
            Some(AgentCommand::Respond { id: 8, value }) => {
                assert_eq!(
                    serde_json::from_value::<UserInputResponse>(value).unwrap(),
                    response
                );
            }
            other => panic!("expected untouched Respond, got {other:?}"),
        }
        assert!(!called.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn image_steer_acknowledges_the_driver_original_after_preprocessing() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, mut kernel_commands, kernel_events, mut runtime_events, _adapter) =
            spawn_with_preprocessor(
                Some(Arc::new(RecordingPreprocessor {
                    called: called.clone(),
                })),
                false,
            )
            .await;

        handle.submit(UserInput::from("first")).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "first"
        ));

        let original = UserInput {
            text: "before\n[Image #1]\nafter".into(),
            images: vec![test_image("raw-image")],
        };
        assert!(matches!(
            handle.submit(original.clone()).await.unwrap(),
            SubmitReceipt::Steered { .. }
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, images })
                if text == "VL[before\n[Image #1]\nafter]" && images.is_empty()
        ));

        kernel_events
            .send(AgentEvent::Steered {
                turn: None,
                count: 1,
                inputs: vec![atomcode_kernel::event::SteeredInput {
                    text: "VL[before\n[Image #1]\nafter]".into(),
                    images: Vec::new(),
                }],
            })
            .unwrap();

        let (authoritative, acknowledged) =
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let mut authoritative = None;
                let mut acknowledged = None;
                loop {
                    match runtime_events.recv().await {
                        Some(CodingRuntimeEvent::Agent(AgentEvent::Steered { inputs, .. })) => {
                            authoritative = Some(inputs)
                        }
                        Some(CodingRuntimeEvent::SteerAcknowledged { inputs }) => {
                            acknowledged = Some(inputs)
                        }
                        Some(_) => {}
                        None => panic!("runtime event stream closed"),
                    }
                    if authoritative.is_some() && acknowledged.is_some() {
                        break (authoritative.unwrap(), acknowledged.unwrap());
                    }
                }
            })
            .await
            .expect("missing steered acknowledgement");
        assert_eq!(
            authoritative,
            vec![atomcode_kernel::event::SteeredInput {
                text: "VL[before\n[Image #1]\nafter]".into(),
                images: Vec::new(),
            }],
            "kernel Steered must preserve the input actually folded into conversation"
        );
        assert_eq!(acknowledged, vec![original]);
        assert!(called.load(Ordering::Acquire));
    }

    // Guard: a text-only submit skips the preprocessor entirely (no images),
    // so the original text passes through untouched.
    #[tokio::test]
    async fn text_only_submit_skips_preprocessor() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, mut kernel_commands, _kernel_events, _runtime_events, _adapter) =
            spawn_with_preprocessor(
                Some(Arc::new(RecordingPreprocessor {
                    called: called.clone(),
                })),
                false,
            )
            .await;

        handle
            .submit(UserInput::from("no images here"))
            .await
            .unwrap();

        match kernel_commands.recv().await {
            Some(AgentCommand::SendMessage { text, .. }) => {
                assert_eq!(text, "no images here", "text-only submit must be unchanged");
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
        assert!(
            !called.load(Ordering::Acquire),
            "preprocessor must NOT run without images"
        );
    }

    // On VL failure the runtime emits VisionPreprocessFailed (the driver
    // re-attaches its remembered images) and the kernel still gets a
    // text-only turn.
    #[tokio::test]
    async fn image_submit_failure_emits_failed_event_and_text_only_turn() {
        let (handle, mut kernel_commands, _kernel_events, mut runtime_events, _adapter) =
            spawn_with_preprocessor(Some(Arc::new(FailingPreprocessor)), false).await;

        handle
            .submit(UserInput {
                text: "look".into(),
                images: vec![ImageContent {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
            })
            .await
            .unwrap();

        // Kernel receives text-only (images cleared for the non-vision model).
        match kernel_commands.recv().await {
            Some(AgentCommand::SendMessage { images, .. }) => {
                assert!(
                    images.is_empty(),
                    "failed VL must clear images for the model"
                );
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        let mut saw_failed = false;
        while let Ok(ev) = runtime_events.try_recv() {
            if let CodingRuntimeEvent::VisionPreprocessFailed { reason } = ev {
                assert_eq!(reason, "boom");
                saw_failed = true;
            }
        }
        assert!(saw_failed, "runtime must emit VisionPreprocessFailed");
    }

    #[tokio::test]
    async fn recoverable_goal_continuation_send_failure_deactivates_goal_and_fails_runtime() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await;
        let _ = kernel_commands.recv().await;
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::ProviderError,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        drop(kernel_commands);
        let expected = SessionSnapshot::new(vec![Message::user("persisted")]);
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: expected.clone(),
            })
            .unwrap();

        let mut inactive_goal = false;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                        active: false,
                        last_reason,
                        ..
                    })) if last_reason.as_deref() == Some("continuation dispatch failed") => {
                        inactive_goal = true;
                    }
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before terminal"),
                }
            }
        })
        .await
        .expect("recoverable continuation failure lost terminal");

        assert!(inactive_goal, "goal must be explicitly deactivated");
        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::ProviderError,
                snapshot,
                ..
            } if snapshot.as_ref() == &expected
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);
    }

    #[tokio::test]
    async fn cancelled_first_turn_does_not_build_ai_naming_provider() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: mut config,
            prepare,
            plugin_hooks,
            ..
        } = native_start(false);
        config.subagent_config = Some(Arc::new(atomcode_config::config::Config::default()));
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let factory = Arc::new(CountingProviderFactory::default());
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory: factory.clone(),
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.submit(UserInput::from("cancel me")).await.unwrap();
        let _ = kernel_commands.recv().await;
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Cancelled,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("cancel me")]),
            })
            .unwrap();

        while !matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(_))
        ) {}
        assert_eq!(factory.builds.load(Ordering::SeqCst), 0);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replacing_loop_with_goal_finishes_held_turn_once() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx: wakeup_tx.clone(),
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.start_loop("watch CI", None).await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        wakeup_tx
            .send(WakeupRequest {
                delay_seconds: 60,
                reason: "wait for CI".into(),
                prompt: "check CI".into(),
            })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: true,
                ..
            }))
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("persisted")]),
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match handle.start_goal("tests pass").await {
                    Ok(()) => break,
                    Err(RuntimeError::Busy) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected start_goal error: {error}"),
                }
            }
        })
        .await
        .expect("snapshot never entered the held-turn state");
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: false,
                ..
            }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Cancelled,
                    ..
                }
            ))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        // The goal that took the slot opens its own first round, so what the
        // held turn's terminal left behind is a session back at work — not an
        // idle one.
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "tests pass"
        ));
        assert_eq!(handle.status().phase, RuntimePhase::InTurn);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replacing_goal_with_loop_finishes_held_turn_once() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            plugin_hooks,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory: Arc::new(PendingProviderFactory),
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: true,
                ..
            }))
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("persisted")]),
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match handle.start_loop("watch CI", None).await {
                    Ok(()) => break,
                    Err(RuntimeError::Busy) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected start_loop error: {error}"),
                }
            }
        })
        .await
        .expect("snapshot never entered the held-turn state");
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress {
                active: false,
                ..
            }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Cancelled,
                    ..
                }
            ))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: true,
                ..
            }))
        ));
        // Same as the other way round: the loop that took the slot runs its
        // own first pass.
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "watch CI"
        ));
        assert_eq!(handle.status().phase, RuntimePhase::InTurn);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_transitions_are_rejected_before_preflight_while_turn_is_active() {
        let (agent, mut kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle
            .submit(UserInput::from("still running"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        let missing_session_id = format!("missing-{}", uuid::Uuid::new_v4());
        assert_eq!(
            handle.resume_session(missing_session_id).await,
            Err(RuntimeError::Busy)
        );
        assert_eq!(handle.fresh_session().await, Err(RuntimeError::Busy));
        let other_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            handle
                .change_directory(other_dir.path().to_path_buf())
                .await,
            Err(RuntimeError::Busy)
        );
        assert_eq!(handle.status().phase, RuntimePhase::InTurn);
        assert!(runtime_events.try_recv().is_err());
        assert!(kernel_commands.try_recv().is_err());

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn fresh_session_rejects_a_held_loop_without_cancelling_it() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx: wakeup_tx.clone(),
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.start_loop("watch CI", None).await.unwrap();
        let _ = runtime_events.recv().await;
        let _ = kernel_commands.recv().await;
        wakeup_tx
            .send(WakeupRequest {
                delay_seconds: 60,
                reason: "wait for CI".into(),
                prompt: "check CI".into(),
            })
            .unwrap();
        let _ = runtime_events.recv().await;
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("persisted")]),
            })
            .unwrap();

        assert_eq!(handle.fresh_session().await, Err(RuntimeError::Busy));
        assert_eq!(handle.status().phase, RuntimePhase::InTurn);
        assert!(runtime_events.try_recv().is_err());
        assert!(kernel_commands.try_recv().is_err());

        handle.stop_loop().await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: false,
                ..
            }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Cancelled,
                    ..
                }
            ))
        ));
        assert!(kernel_commands.try_recv().is_err());
        assert_eq!(handle.status().phase, RuntimePhase::Ready);

        assert!(runtime_events.try_recv().is_err());
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn mcp_withdrawal_and_same_session_reload_reject_an_active_turn() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let (agent, mut kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            mut prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        prepare.session = crate::SessionMode::Fresh;
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.submit(UserInput::from("active")).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        assert_eq!(handle.withdraw_mcp_tools().await, Err(RuntimeError::Busy));
        assert_eq!(handle.reload_capabilities().await, Err(RuntimeError::Busy));
        assert_eq!(handle.status().phase, RuntimePhase::InTurn);
        assert!(runtime_events.try_recv().is_err());

        handle.shutdown().await.unwrap();
    }

    /// The other half of that rule: the actions that withdraw the tools, and the
    /// ones that only reach the session through a rebuild, wait for an idle
    /// session — up front, before anything is written, so a refusal never leaves
    /// a change on disk that the session does not have. `Disable` takes tools
    /// away in place rather than widening anything, and a mid-session switch is
    /// the point (design §5.4), so it runs mid-turn the way `SwitchTool` does.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn mcp_act_that_needs_an_idle_session_rejects_an_active_turn_but_a_disable_goes_through()
    {
        use crate::parts::McpAction;

        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        // `native_start` hands out "." as the working dir; this test's `Disable`
        // writes `.mcp.json`, so the project is a temp dir rather than the crate's
        // own directory.
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".mcp.json"),
            r#"{"mcpServers":{"srv":{"command":"npx","args":["-y","x"]}}}"#,
        )
        .unwrap();

        let (agent, mut kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, _runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: mut config,
            mut prepare,
            provider_factory,
            plugin_hooks,
            ..
        } = native_start(false);
        config.working_dir = project.path().to_path_buf();
        prepare.session = crate::SessionMode::Fresh;
        let parts =
            prepare_with_plugin_hook_source(&config, prepare.clone(), plugin_hooks.as_ref())
                .await
                .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.submit(UserInput::from("active")).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        // Both withdrawing actions take the refusal the dedicated control takes.
        let untrust = handle.mcp_act("srv".into(), McpAction::Untrust).await;
        assert_eq!(untrust, Err(RuntimeError::Busy));
        let logout = handle.mcp_act("srv".into(), McpAction::Logout).await;
        assert_eq!(logout, Err(RuntimeError::Busy));
        // And the two that only a rebuild can apply.
        for action in [McpAction::Trust, McpAction::Enable] {
            assert_eq!(
                handle.mcp_act("srv".into(), action).await,
                Err(RuntimeError::Busy),
                "{action:?} is applied by a rebuild, which a running turn refuses"
            );
        }
        assert!(
            !std::fs::read_to_string(project.path().join(".mcp.json"))
                .unwrap()
                .contains("disabled"),
            "a refused action wrote nothing"
        );

        // The non-withdrawing one goes through: the flag reached the file.
        handle
            .mcp_act("srv".into(), McpAction::Disable)
            .await
            .unwrap();
        let text = std::fs::read_to_string(project.path().join(".mcp.json")).unwrap();
        assert!(
            text.contains("\"disabled\": true"),
            "a mid-turn disable still writes the flag: {text}"
        );

        handle.shutdown().await.unwrap();
    }

    /// Trusting a project from the panel connects its servers.
    ///
    /// Trust is read when the graph is prepared, so writing it and stopping
    /// there left the server listed as untrusted with 信任 still on offer — the
    /// action looked like it had not happened. After it, the server has to be
    /// something other than blocked: connecting, connected, or failed on its own
    /// terms.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn trusting_a_project_from_the_panel_reaches_the_session() {
        use crate::parts::McpAction;
        use atomcode_capabilities::mcp::ServerStatus;

        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".mcp.json"),
            r#"{"mcpServers":{"local":{"command":"/nonexistent/atomcode-test-mcp"}}}"#,
        )
        .unwrap();
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.mcp = true;
        let runtime = CodingRuntime::start(start).await.unwrap();
        let handle = runtime.handle.clone();

        let status = |rows: &McpRowsSnapshot| {
            rows.rows
                .iter()
                .find(|row| row.name == "local")
                .map(|row| row.status.clone())
                .expect("the project server is listed")
        };
        assert_eq!(
            status(&handle.mcp_rows().await.unwrap()),
            ServerStatus::BlockedUntrusted,
            "an untrusted project's server starts blocked"
        );

        handle
            .mcp_act("local".into(), McpAction::Trust)
            .await
            .unwrap();
        assert_ne!(
            status(&handle.mcp_rows().await.unwrap()),
            ServerStatus::BlockedUntrusted,
            "after 信任 the server is no longer held back"
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_approval_is_correlated_and_shutdown_fails_it_closed() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let _adapter = spawn_runtime_owner_with_protocol(
            agent, controls, runtime_tx, true, true, None, None, None,
        );

        handle
            .submit(UserInput::from("needs approval"))
            .await
            .unwrap();
        let _ = kernel_commands.recv().await;
        kernel_events
            .send(AgentEvent::Request {
                id: 42,
                kind: "tool_approval".into(),
                payload: serde_json::json!({"tool": "bash"}),
            })
            .unwrap();

        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Request(RuntimeRequest { id: 42, .. }))
        ));
        assert_eq!(handle.status().phase, RuntimePhase::WaitingApproval);
        assert_eq!(
            handle.respond(41, serde_json::Value::Null).await,
            Err(RuntimeError::StaleRequest { id: 41 })
        );

        handle.shutdown().await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Respond {
                id: 42,
                value: serde_json::Value::Null,
            })
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Cancel)
        ));
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Shutdown)
        ));
    }

    #[tokio::test]
    async fn native_auto_mode_answers_only_approval_requests_without_prompting_driver() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let start = native_start(false);
        let parts = prepare_with_plugin_hook_source(
            &start.agent,
            start.prepare.clone(),
            start.plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        let resources = RuntimeResources {
            config: start.agent,
            prepare: start.prepare,
            provider_factory: start.provider_factory,
            plugin_hooks: start.plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let _adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );

        handle.set_mode(RuntimeMode::Auto).await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::ModeChanged {
                mode: RuntimeMode::Auto
            })
        ));
        handle.submit(UserInput::from("auto turn")).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        kernel_events
            .send(AgentEvent::Request {
                id: 42,
                kind: APPROVAL_KIND.into(),
                payload: serde_json::json!({"tool": "task"}),
            })
            .unwrap();
        let expected = serde_json::to_value(ApprovalResponse::allow()).unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Respond { id: 42, value }) if value == expected
        ));
        assert_eq!(handle.status().phase, RuntimePhase::InTurn);
        assert!(runtime_events.try_recv().is_err());

        kernel_events
            .send(AgentEvent::Request {
                id: 43,
                kind: "request_user_input".into(),
                payload: serde_json::json!({"question": "choose"}),
            })
            .unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Request(RuntimeRequest {
                id: 43,
                ref kind,
                ..
            })) if kind == "request_user_input"
        ));
        assert_eq!(handle.status().phase, RuntimePhase::WaitingApproval);

        handle.respond(43, serde_json::Value::Null).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Respond { id: 43, .. })
        ));
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn provider_reassemble_commits_one_generation_and_tags_events_at_emit_time() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();
        let mut next = CodingAgentConfig::new(
            "next-key",
            "https://next.example.test/v1",
            "next-model",
            ".",
        );
        next.provider_name = "next-provider".into();
        next.provider_type = "openai".into();

        assert_eq!(
            runtime.handle.reassemble_provider(next).await.unwrap(),
            RuntimeGeneration(1)
        );
        assert_eq!(runtime.handle.status().generation, 1);
        assert_eq!(
            runtime.handle.context_stats().await.unwrap().model,
            "next-model"
        );

        let first = runtime.events.recv().await.unwrap();
        let second = runtime.events.recv().await.unwrap();
        let third = runtime.events.recv().await.unwrap();
        let fourth = runtime.events.recv().await.unwrap();
        assert_eq!(first.generation, 0);
        assert!(matches!(
            first.event,
            CodingRuntimeEvent::Reconfiguring {
                operation: ReconfigureKind::Provider
            }
        ));
        assert_eq!(second.generation, 1);
        assert!(matches!(
            second.event,
            CodingRuntimeEvent::ProviderChanged { ref provider, ref model }
                if provider == "next-provider" && model == "next-model"
        ));
        assert_eq!(third.generation, 1);
        assert!(matches!(
            third.event,
            CodingRuntimeEvent::ReasoningEffortChanged {
                ref provider,
                effort: None,
                applicable: false
            } if provider == "next-provider"
        ));
        assert_eq!(fourth.generation, 1);
        assert!(matches!(
            fourth.event,
            CodingRuntimeEvent::Reconfigured {
                operation: ReconfigureKind::Provider
            }
        ));
        assert!(
            first.sequence < second.sequence
                && second.sequence < third.sequence
                && third.sequence < fourth.sequence
        );
        runtime.handle.shutdown().await.unwrap();
    }

    async fn hanging_provider_reassemble_runtime(
        emit_verified_terminal: bool,
    ) -> (
        CodingRuntimeHandle,
        mpsc::UnboundedReceiver<CodingRuntimeEvent>,
        KernelRuntimeAdapter,
    ) {
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                if matches!(command, AgentCommand::Shutdown) {
                    if emit_verified_terminal {
                        let _ = event_tx.send(AgentEvent::TurnComplete {
                            turn: None,
                            reason: StopReason::Cancelled,
                        });
                        let _ = event_tx.send(AgentEvent::Snapshot {
                            snapshot: SessionSnapshot::new(vec![Message::user(
                                "verified interrupted turn",
                            )]),
                        });
                    }
                    std::future::pending::<()>().await;
                }
            }
        });
        let agent = AgentHandle {
            commands,
            events,
            task,
        };
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let start = native_start(false);
        let parts = prepare_with_plugin_hook_source(
            &start.agent,
            start.prepare.clone(),
            start.plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        let resources = RuntimeResources {
            config: start.agent,
            prepare: start.prepare,
            provider_factory: start.provider_factory,
            plugin_hooks: start.plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor: None,
        };
        let adapter = spawn_runtime_owner_with_protocol(
            agent,
            controls,
            runtime_tx,
            true,
            true,
            None,
            Some(resources),
            Some(wakeup_rx),
        );
        (handle, runtime_events, adapter)
    }

    #[tokio::test]
    async fn provider_reassemble_fails_closed_when_the_active_agent_cannot_stop() {
        let (handle, mut runtime_events, _adapter) =
            hanging_provider_reassemble_runtime(false).await;

        handle
            .submit(UserInput::from("unfinished turn"))
            .await
            .unwrap();
        let next = CodingAgentConfig::new(
            "next-key",
            "https://next.example.test/v1",
            "next-model",
            ".",
        );
        assert!(matches!(
            handle.reassemble_provider(next).await,
            Err(RuntimeError::ReconfigureFailed(message))
                if message.contains("latest conversation snapshot could not be verified")
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);

        let mut saw_unavailable_terminal = false;
        while let Ok(event) = runtime_events.try_recv() {
            match event {
                CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                    ..
                }) => {
                    saw_unavailable_terminal = true;
                }
                CodingRuntimeEvent::ProviderChanged { .. }
                | CodingRuntimeEvent::Reconfigured { .. } => {
                    panic!("a forced stop must not publish provider reconfigure success")
                }
                _ => {}
            }
        }
        assert!(saw_unavailable_terminal);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn provider_reassemble_accepts_a_verified_terminal_before_forced_cleanup() {
        let (handle, mut runtime_events, _adapter) =
            hanging_provider_reassemble_runtime(true).await;
        handle
            .submit(UserInput::from("unfinished turn"))
            .await
            .unwrap();
        let next = CodingAgentConfig::new(
            "next-key",
            "https://next.example.test/v1",
            "next-model",
            ".",
        );

        assert_eq!(
            handle.reassemble_provider(next).await.unwrap(),
            RuntimeGeneration(1)
        );
        assert_eq!(handle.status().phase, RuntimePhase::Ready);
        let mut changed = false;
        while let Ok(event) = runtime_events.try_recv() {
            if matches!(event, CodingRuntimeEvent::ProviderChanged { .. }) {
                changed = true;
            }
            assert!(!matches!(
                event,
                CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable { .. })
            ));
        }
        assert!(changed);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn provider_reassemble_updates_cost_attribution_and_failed_reload_keeps_current_model() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());

        let factory = Arc::new(UsageProviderFactory::default());
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.agent.provider_name = "provider-a".into();
        start.agent.model = "model-a".into();
        start.prepare.session = crate::SessionMode::Fresh;
        start.provider_factory = factory.clone();
        let mut runtime = CodingRuntime::start(start).await.unwrap();

        runtime
            .handle
            .submit(UserInput::from("model a turn"))
            .await
            .unwrap();
        wait_for_turn_finished(&mut runtime).await;
        let model_a_context = runtime.handle.context_stats().await.unwrap();
        assert!(model_a_context.used_tokens > 0);

        let mut model_b =
            CodingAgentConfig::new("key", "https://example.test/v1", "model-b", project.path());
        model_b.provider_name = "provider-b".into();
        runtime
            .handle
            .reassemble_provider(model_b.clone())
            .await
            .unwrap();
        let reassembled_context = runtime.handle.context_stats().await.unwrap();
        assert_eq!(reassembled_context.model, "model-b");
        assert_eq!(reassembled_context.used_tokens, model_a_context.used_tokens);
        runtime
            .handle
            .submit(UserInput::from("model b turn"))
            .await
            .unwrap();
        wait_for_turn_finished(&mut runtime).await;

        *factory.fail_model.lock().unwrap() = Some("model-fail".into());
        let mut failed = model_b;
        failed.provider_name = "provider-fail".into();
        failed.model = "model-fail".into();
        assert!(matches!(
            runtime.handle.reassemble_provider(failed).await,
            Err(RuntimeError::ReconfigureFailed(message))
                if message.contains("expected usage-provider reload failure")
        ));
        runtime
            .handle
            .submit(UserInput::from("model b after failed reload"))
            .await
            .unwrap();
        wait_for_turn_finished(&mut runtime).await;

        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let sessions = manager.list();
        assert_eq!(sessions.len(), 1);
        let report = atomcode_capabilities::session::aggregate_session_cost(
            &manager.read_meta(&sessions[0].id).unwrap(),
        );
        assert_eq!(report.models.len(), 2);
        assert_eq!(report.models[0].provider_id, "provider-a");
        assert_eq!(report.models[0].model_id, "model-a");
        assert_eq!(report.models[0].tokens.total(), 110);
        assert_eq!(report.models[1].provider_id, "provider-b");
        assert_eq!(report.models[1].model_id, "model-b");
        assert_eq!(report.models[1].tokens.total(), 220);

        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn missing_resume_rolls_back_without_silent_fresh_session() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();

        let missing_session_id = format!("missing-{}", uuid::Uuid::new_v4());
        let result = runtime.handle.resume_session(missing_session_id).await;

        assert!(matches!(result, Err(RuntimeError::ReconfigureFailed(_))));
        assert_eq!(
            runtime.handle.status(),
            RuntimeStatus {
                generation: 0,
                phase: RuntimePhase::Ready,
            }
        );
        assert!(matches!(
            runtime.events.recv().await.unwrap().event,
            CodingRuntimeEvent::Reconfiguring {
                operation: ReconfigureKind::ResumeSession
            }
        ));
        assert!(runtime.events.try_recv().is_err());
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn fresh_session_is_runtime_owned_and_returns_new_identity() {
        let runtime = CodingRuntime::start(native_start(false)).await.unwrap();

        let changed = runtime.handle.fresh_session().await.unwrap();

        assert_eq!(changed.generation, RuntimeGeneration(1));
        assert!(changed.session_id.as_ref().is_some_and(|id| !id.is_empty()));
        assert_eq!(runtime.handle.status().generation, 1);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn change_directory_to_current_path_is_a_runtime_noop() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        let before = runtime.handle.status();

        let unchanged = runtime
            .handle
            .change_directory(project.path().join("."))
            .await
            .unwrap();

        assert_eq!(unchanged.generation, RuntimeGeneration(before.generation));
        assert_eq!(unchanged.working_dir, project.path());
        assert_eq!(runtime.handle.status(), before);
        assert!(
            runtime.events.try_recv().is_err(),
            "a no-op directory change must not emit reconfiguration events"
        );
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn failed_fresh_candidate_keeps_previous_runtime_ready() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let factory = Arc::new(FailAfterFirstBuildFactory {
            builds: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.provider_factory = factory.clone();
        let runtime = CodingRuntime::start(start).await.unwrap();
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        assert!(manager.list().is_empty());

        assert!(matches!(
            runtime.handle.fresh_session().await,
            Err(RuntimeError::ReconfigureFailed(message))
                if message.contains("candidate provider failed")
        ));
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        assert_eq!(factory.builds.load(Ordering::Acquire), 2);
        assert!(
            manager.list().is_empty(),
            "a failed candidate must not leave a visible catalog session"
        );

        runtime
            .handle
            .submit(UserInput::from("old runtime still works"))
            .await
            .unwrap();
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial(atomcode_home)]
    async fn fresh_candidate_is_not_catalog_visible_while_provider_builds() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let factory = Arc::new(BlockAndFailSecondBuildFactory {
            builds: std::sync::atomic::AtomicUsize::new(0),
            entered: entered.clone(),
            release: release.clone(),
        });
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.provider_factory = factory;
        let runtime = CodingRuntime::start(start).await.unwrap();
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let handle = runtime.handle.clone();
        let transition = tokio::spawn(async move { handle.fresh_session().await });

        entered.wait();
        assert!(
            manager.list().is_empty(),
            "a candidate is not committed while its provider graph is still fallible"
        );
        release.wait();
        assert!(matches!(
            transition.await.unwrap(),
            Err(RuntimeError::ReconfigureFailed(message))
                if message.contains("blocked candidate failed")
        ));
        assert!(manager.list().is_empty());

        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn runtime_holds_one_session_lease_reuses_it_and_releases_on_shutdown() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let session_id = "leased-runtime";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        persist_native_session(
            &manager,
            session_id,
            project.path(),
            &SessionSnapshot::new(vec![Message::user("persisted")]),
        );
        let start = || {
            let mut start = native_start(false);
            start.agent.working_dir = project.path().to_path_buf();
            start.prepare.session = crate::SessionMode::Resume(session_id.into());
            start
        };

        let first = CodingRuntime::start(start()).await.unwrap();
        let second_error = match CodingRuntime::start(start()).await {
            Ok(_) => panic!("a second runtime must not own the same session"),
            Err(error) => error,
        };
        assert!(matches!(
            second_error,
            RuntimeStartError::SessionInUse { ref id } if id == session_id
        ));

        first.handle.reload_capabilities().await.unwrap();
        first.handle.shutdown().await.unwrap();

        let second = CodingRuntime::start(start()).await.unwrap();
        second.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn config_reprepare_advances_generation_and_keeps_the_current_session() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let session_id = "config-reprepare-session";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        persist_native_session(
            &manager,
            session_id,
            project.path(),
            &SessionSnapshot::new(vec![Message::user("persisted before reprepare")]),
        );
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(session_id.into());
        let mut next_config = start.agent.clone();
        next_config.todo.enabled = !next_config.todo.enabled;
        let runtime = CodingRuntime::start(start).await.unwrap();
        let before = runtime.handle.status();

        let changed = runtime.handle.reprepare_config(next_config).await.unwrap();

        assert!(changed.generation.0 > before.generation);
        assert_eq!(changed.session_id.as_deref(), Some(session_id));
        assert_eq!(changed.working_dir, project.path());
        assert_eq!(runtime.handle.status().generation, changed.generation.0);
        let snapshot = runtime.handle.snapshot().await.unwrap();
        assert!(snapshot
            .messages
            .iter()
            .any(|message| message.text == "persisted before reprepare"));
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn importer_lease_is_transferred_without_an_unlocked_resume_window() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let id = "imported-runtime";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        persist_native_session(
            &manager,
            id,
            project.path(),
            &SessionSnapshot::new(vec![Message::user("persisted")]),
        );
        let lease = manager.acquire_lease(id).unwrap();
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(id.into());

        let runtime =
            CodingRuntime::start_with_session_lease(start, ProviderBootstrap::Required, lease)
                .await
                .unwrap();

        assert!(matches!(
            manager.acquire_lease(id),
            Err(SessionStoreError::SessionInUse { .. })
        ));
        runtime.handle.shutdown().await.unwrap();
        manager.acquire_lease(id).unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn prepared_resume_transfers_exact_lease_and_keeps_old_snapshot_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let target_id = "prepared-target";
        let target_snapshot = SessionSnapshot::new(vec![Message::user("target history")]);
        let target_lease = manager.acquire_lease(target_id).unwrap();
        let mut target_meta = SessionMeta::new(target_id, project.path().to_string_lossy(), 1);
        target_meta.owner = StorageOwner::Native;
        target_meta.message_count = 1;
        manager
            .commit_native_import(
                &target_lease,
                Some(&target_snapshot),
                Some(&PresentationFile::default()),
                &target_meta,
            )
            .unwrap();

        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        let runtime = CodingRuntime::start(start).await.unwrap();
        let old_id = runtime.session.as_ref().unwrap().id.clone();
        let old_snapshot = manager.load_snapshot(&old_id).unwrap();

        let changed = runtime
            .handle
            .resume_session_with_lease(target_id, project.path().to_path_buf(), target_lease)
            .await
            .unwrap();

        assert_eq!(changed.session_id.as_deref(), Some(target_id));
        let resumed = runtime.handle.snapshot().await.unwrap();
        assert!(resumed.messages.iter().any(|message| {
            message.role == atomcode_kernel::message::Role::User && message.text == "target history"
        }));
        assert_eq!(manager.load_snapshot(&old_id).unwrap(), old_snapshot);
        assert!(manager.acquire_lease(&old_id).is_ok());
        assert!(matches!(
            manager.acquire_lease(target_id),
            Err(SessionStoreError::SessionInUse { .. })
        ));

        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn cancelled_preflight_keeps_the_current_runtime_authoritative() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let target_id = "cancelled-target";
        persist_native_session(
            &manager,
            target_id,
            project.path(),
            &SessionSnapshot::new(vec![Message::user("target history")]),
        );
        let target_lease = manager.acquire_lease(target_id).unwrap();

        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        let runtime = CodingRuntime::start(start).await.unwrap();
        let old_id = runtime.session.as_ref().unwrap().id.clone();
        let old_snapshot = runtime.handle.snapshot().await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        let result = runtime
            .handle
            .resume_session_with_lease_cancelable(
                target_id,
                project.path().to_path_buf(),
                target_lease,
                cancel,
            )
            .await;

        assert_eq!(result, Err(RuntimeError::Cancelled));
        assert_eq!(runtime.handle.status().generation, 0);
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        assert_eq!(runtime.handle.snapshot().await.unwrap(), old_snapshot);
        assert!(matches!(
            manager.acquire_lease(&old_id),
            Err(SessionStoreError::SessionInUse { .. })
        ));
        assert!(manager.acquire_lease(target_id).is_ok());

        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn prepared_resume_rejects_a_lease_for_another_session() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        manager
            .save_snapshot(
                "target-session",
                &SessionSnapshot::new(vec![Message::user("target history")]),
            )
            .unwrap();
        let wrong_lease = manager.acquire_lease("other-session").unwrap();

        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        let runtime = CodingRuntime::start(start).await.unwrap();
        let old_id = runtime.session.as_ref().unwrap().id.clone();

        let result = runtime
            .handle
            .resume_session_with_lease("target-session", project.path().to_path_buf(), wrong_lease)
            .await;

        assert!(matches!(result, Err(RuntimeError::ReconfigureFailed(_))));
        assert_eq!(runtime.handle.status().generation, 0);
        assert!(matches!(
            manager.acquire_lease(&old_id),
            Err(SessionStoreError::SessionInUse { .. })
        ));
        assert!(manager.acquire_lease("target-session").is_ok());
        assert!(manager.acquire_lease("other-session").is_ok());

        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn session_switch_conflict_keeps_old_owner_then_transfers_both_leases() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        for id in ["session-a", "session-b"] {
            persist_native_session(
                &manager,
                id,
                project.path(),
                &SessionSnapshot::new(vec![Message::user(format!("snapshot {id}"))]),
            );
        }
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume("session-a".into());
        let runtime = CodingRuntime::start(start).await.unwrap();
        let session_b_owner = manager.acquire_lease("session-b").unwrap();

        assert_eq!(
            runtime.handle.resume_session("session-b").await,
            Err(RuntimeError::SessionInUse {
                id: "session-b".into()
            })
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        assert!(matches!(
            manager.acquire_lease("session-a"),
            Err(SessionStoreError::SessionInUse { .. })
        ));

        drop(session_b_owner);
        runtime.handle.resume_session("session-b").await.unwrap();
        manager.acquire_lease("session-a").unwrap();
        assert!(matches!(
            manager.acquire_lease("session-b"),
            Err(SessionStoreError::SessionInUse { .. })
        ));

        runtime.handle.shutdown().await.unwrap();
        manager.acquire_lease("session-b").unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn incomplete_resume_fails_before_runtime_can_accept_a_turn() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let session_id = "incomplete-runtime";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let snapshot = SessionSnapshot::new(vec![Message::user("persisted")]);
        manager.save_snapshot(session_id, &snapshot).unwrap();
        let mut meta = SessionMeta::new(session_id, project.path().to_string_lossy(), 1);
        meta.owner = StorageOwner::Native;
        manager.write_meta(&meta).unwrap();

        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(session_id.into());
        let error = match CodingRuntime::start(start).await {
            Ok(runtime) => {
                runtime.handle.shutdown().await.unwrap();
                panic!("an incomplete aggregate must not produce a runtime handle");
            }
            Err(error) => error,
        };
        let RuntimeStartError::Prepare(error) = error else {
            panic!("expected prepare failure, got {error}");
        };
        assert!(matches!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<SessionStoreError>()),
            Some(SessionStoreError::NotFound { path })
                if path == &manager.presentation_path(session_id).unwrap()
        ));
        manager.acquire_lease(session_id).unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn startup_failure_releases_the_prepared_session_lease() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let session_id = "failed-runtime";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        persist_native_session(
            &manager,
            session_id,
            project.path(),
            &SessionSnapshot::new(vec![Message::user("persisted")]),
        );
        let mut start = native_start(true);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(session_id.into());

        assert!(matches!(
            CodingRuntime::start(start).await,
            Err(RuntimeStartError::Provider(_))
        ));
        manager.acquire_lease(session_id).unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn dropping_runtime_releases_its_session_lease() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let session_id = "dropped-runtime";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        persist_native_session(
            &manager,
            session_id,
            project.path(),
            &SessionSnapshot::new(vec![Message::user("persisted")]),
        );
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(session_id.into());
        let runtime = CodingRuntime::start(start).await.unwrap();

        drop(runtime);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match manager.acquire_lease(session_id) {
                    Ok(_) => break,
                    Err(SessionStoreError::SessionInUse { .. }) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected lease error: {error}"),
                }
            }
        })
        .await
        .expect("runtime drop did not release the session lease");
    }

    #[tokio::test]
    async fn undo_preserves_snapshot_identity_and_reassembles_sessionless_runtime() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();
        runtime
            .handle
            .submit(UserInput::from("first prompt"))
            .await
            .unwrap();
        loop {
            if matches!(
                runtime.events.recv().await.unwrap().event,
                CodingRuntimeEvent::TurnFinished(_)
            ) {
                break;
            }
        }

        let result = runtime.handle.undo_to_prompt(None).await.unwrap();

        assert_eq!(result.restored_prompt, "first prompt");
        assert_eq!(result.target_n, 1);
        assert_eq!(result.prompts_before, 1);
        assert_eq!(result.generation, RuntimeGeneration(1));
        assert!(result
            .snapshot
            .messages
            .iter()
            .all(|message| message.text != "first prompt"));
        let current = runtime.handle.snapshot().await.unwrap();
        assert_eq!(current.messages, result.snapshot.messages);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn rewind_catalog_and_conversation_scope_are_runtime_owned() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(project.path())
            .status()
            .unwrap();
        assert!(status.success());

        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        runtime
            .handle
            .submit(UserInput::from("first rewind prompt"))
            .await
            .unwrap();
        wait_for_turn_finished(&mut runtime).await;

        let catalog = runtime.handle.rewind_points().await.unwrap();
        assert_eq!(catalog.points.len(), 1);
        assert_eq!(catalog.points[0].prompt_number, 1);
        assert_eq!(catalog.points[0].prompt_preview, "first rewind prompt");
        // The kind, not a substring of a sentence. This used to hunt for
        // "off by default" — with a comment explaining that the *other* reason
        // also mentions `ATOMCODE_CODE_REWIND`, so the obvious substring could
        // not tell the two apart. That was the reason being a sentence; it is a
        // kind now, and the two states are simply two values.
        assert_eq!(catalog.code_unavailable, Some(CodeUnavailable::NotEnabled));

        let code_error = runtime
            .handle
            .rewind(catalog.points[0].turn_id, RewindScope::Code)
            .await
            .unwrap_err();
        assert!(matches!(code_error, RuntimeError::CodeRewindUnavailable(_)));
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);

        let result = runtime
            .handle
            .rewind(catalog.points[0].turn_id, RewindScope::Conversation)
            .await
            .unwrap();
        assert_eq!(result.scope, RewindScope::Conversation);
        assert_eq!(
            result.restored_prompt.as_deref(),
            Some("first rewind prompt")
        );
        assert!(result.restored_files.is_empty());
        assert!(result
            .snapshot
            .messages
            .iter()
            .all(|message| message.text != "first rewind prompt"));
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    async fn mutating_rewind_runtime(
        fail_second_build: bool,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        std::path::PathBuf,
        CodingRuntime,
        RewindPoint,
    ) {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(project.path())
            .status()
            .unwrap();
        assert!(status.success());

        let generated = project.path().join("generated.txt");
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Fresh;
        start.provider_factory = Arc::new(MutatingProviderFactory {
            path: generated.clone(),
            fail_second_build,
            builds: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        runtime
            .handle
            .submit(UserInput::from("write generated.txt"))
            .await
            .unwrap();
        wait_for_turn_finished(&mut runtime).await;
        assert_eq!(
            std::fs::read_to_string(&generated).unwrap(),
            "generated by the agent\n"
        );
        let point = runtime
            .handle
            .rewind_points()
            .await
            .unwrap()
            .points
            .into_iter()
            .next()
            .unwrap();
        assert!(point.files.iter().any(|file| file.path == "generated.txt"));
        (home, project, generated, runtime, point)
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn code_only_rewind_restores_workspace_but_keeps_conversation() {
        let (_home, _project, generated, runtime, point) = mutating_rewind_runtime(false).await;

        let result = runtime
            .handle
            .rewind(point.turn_id, RewindScope::Code)
            .await
            .unwrap();

        assert!(!generated.exists());
        assert_eq!(result.restored_prompt, None);
        assert_eq!(result.restored_files, vec!["generated.txt"]);
        assert!(result
            .snapshot
            .messages
            .iter()
            .any(|message| message.text == "write generated.txt"));
        assert!(runtime
            .handle
            .rewind_points()
            .await
            .unwrap()
            .points
            .is_empty());
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn combined_rewind_restores_workspace_and_conversation() {
        let (_home, _project, generated, runtime, point) = mutating_rewind_runtime(false).await;

        let result = runtime
            .handle
            .rewind(point.turn_id, RewindScope::ConversationAndCode)
            .await
            .unwrap();

        assert!(!generated.exists());
        assert_eq!(
            result.restored_prompt.as_deref(),
            Some("write generated.txt")
        );
        assert_eq!(result.restored_files, vec!["generated.txt"]);
        assert!(result
            .snapshot
            .messages
            .iter()
            .all(|message| message.text != "write generated.txt"));
        assert!(runtime
            .handle
            .rewind_points()
            .await
            .unwrap()
            .points
            .is_empty());
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn code_rewind_preserves_workspace_changes_made_after_the_turn() {
        let (_home, _project, generated, runtime, point) = mutating_rewind_runtime(false).await;
        std::fs::write(&generated, "user changed this after the turn\n").unwrap();

        let error = runtime
            .handle
            .rewind(point.turn_id, RewindScope::Code)
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::CodeRewindUnavailable(_)));
        assert_eq!(
            std::fs::read_to_string(&generated).unwrap(),
            "user changed this after the turn\n"
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn combined_rewind_compensates_workspace_when_agent_rebuild_fails() {
        let (_home, _project, generated, runtime, point) = mutating_rewind_runtime(true).await;

        let error = runtime
            .handle
            .rewind(point.turn_id, RewindScope::ConversationAndCode)
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::ReconfigureFailed(_)));
        assert_eq!(
            std::fs::read_to_string(&generated).unwrap(),
            "generated by the agent\n"
        );
        let snapshot = runtime.handle.snapshot().await.unwrap();
        assert!(snapshot
            .messages
            .iter()
            .any(|message| message.text == "write generated.txt"));
        assert_eq!(
            runtime.handle.rewind_points().await.unwrap().points,
            vec![point]
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn cancelled_rewind_transaction_compensates_and_releases_runtime() {
        let (_home, _project, generated, runtime, point) = mutating_rewind_runtime(false).await;
        let catalog = runtime.handle.rewind_points().await.unwrap();
        let (done, result) = oneshot::channel();
        runtime
            .handle
            .tx
            .send(CodingRuntimeControl::BeginRewind {
                generation: catalog.generation.0,
                expected_revision: catalog.revision,
                point: point.clone(),
                restore_code: true,
                target_snapshot: None,
                recovery_tx: runtime.handle.tx.clone(),
                done,
            })
            .unwrap();
        let transaction = result.await.unwrap().unwrap();
        assert!(!generated.exists());

        drop(transaction);

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if runtime.handle.status().phase == RuntimePhase::Ready
                    && generated.exists()
                    && runtime
                        .handle
                        .rewind_points()
                        .await
                        .is_ok_and(|catalog| catalog.points == vec![point.clone()])
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled rewind did not compensate");
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn cancelled_begin_receiver_is_recovered_by_runtime_owner() {
        let (_home, _project, generated, runtime, point) = mutating_rewind_runtime(false).await;
        let catalog = runtime.handle.rewind_points().await.unwrap();
        let (done, result) = oneshot::channel();
        drop(result);
        runtime
            .handle
            .tx
            .send(CodingRuntimeControl::BeginRewind {
                generation: catalog.generation.0,
                expected_revision: catalog.revision,
                point: point.clone(),
                restore_code: true,
                target_snapshot: None,
                recovery_tx: runtime.handle.tx.clone(),
                done,
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if runtime.handle.status().phase == RuntimePhase::Ready
                    && generated.exists()
                    && runtime
                        .handle
                        .rewind_points()
                        .await
                        .is_ok_and(|catalog| catalog.points == vec![point.clone()])
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owner did not recover an undelivered BeginRewind receipt");
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn abandoned_rewind_recovers_after_undo_advances_generation() {
        let (_home, _project, _generated, runtime, point) = mutating_rewind_runtime(false).await;
        let catalog = runtime.handle.rewind_points().await.unwrap();
        let original = runtime.handle.snapshot_with_revision().await.unwrap();
        let undo =
            undo_snapshot_to_prompt(&original.undo_snapshot, Some(point.prompt_number)).unwrap();
        let target_snapshot = undo.snapshot.clone();
        let (done, result) = oneshot::channel();
        runtime
            .handle
            .tx
            .send(CodingRuntimeControl::BeginRewind {
                generation: catalog.generation.0,
                expected_revision: catalog.revision,
                point,
                restore_code: true,
                target_snapshot: Some(target_snapshot.clone()),
                recovery_tx: runtime.handle.tx.clone(),
                done,
            })
            .unwrap();
        let transaction = result.await.unwrap().unwrap();

        let applied = runtime
            .handle
            .apply_undo(
                catalog.generation.0,
                catalog.revision,
                original.undo_snapshot,
                undo,
                None,
            )
            .await
            .unwrap();
        assert_ne!(applied.generation, catalog.generation);

        let receipt = transaction.commit();
        runtime
            .handle
            .finish_rewind(catalog.generation.0, receipt, RewindFinalization::Recover)
            .await
            .unwrap();
        assert_eq!(
            runtime.handle.snapshot().await.unwrap().as_ref(),
            &target_snapshot
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    #[ignore = "workspace Rewind is intentionally disabled in v5.0.5"]
    async fn rewind_from_stale_catalog_is_not_reinterpreted_against_live_state() {
        let (_home, _project, _generated, runtime, point) = mutating_rewind_runtime(false).await;
        let mut stale = runtime.handle.rewind_points().await.unwrap();
        stale.generation = RuntimeGeneration(stale.generation.0.saturating_add(100));

        let error = runtime
            .handle
            .rewind_from_catalog(stale, point.turn_id, RewindScope::Conversation)
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::Busy));
        assert_eq!(
            runtime.handle.rewind_points().await.unwrap().points,
            vec![point]
        );
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn provider_reassemble_preserves_the_latest_sessionless_snapshot() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();
        runtime
            .handle
            .submit(UserInput::from("first prompt"))
            .await
            .unwrap();
        let before = loop {
            if let CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                snapshot, ..
            }) = runtime.events.recv().await.unwrap().event
            {
                break snapshot;
            }
        };
        let next = CodingAgentConfig::new("key", "https://example.test/v1", "next", ".");

        runtime.handle.reassemble_provider(next).await.unwrap();
        let visible = |snapshot: &SessionSnapshot| {
            snapshot
                .messages
                .iter()
                .filter(|message| message.role != atomcode_kernel::message::Role::System)
                .cloned()
                .collect::<Vec<_>>()
        };
        let next_again =
            CodingAgentConfig::new("key", "https://example.test/v1", "next-again", ".");
        runtime
            .handle
            .reassemble_provider(next_again)
            .await
            .unwrap();
        let after_second_reassemble = runtime.handle.snapshot().await.unwrap();
        // The conversation is kept whole; what follows it is only the two
        // switches, each told to the model where it happened.
        let after = visible(&after_second_reassemble);
        let (kept, added) = after.split_at(visible(&before).len().min(after.len()));
        assert_eq!(visible(&before), kept);
        assert_eq!(added.len(), 2, "{added:#?}");
        assert!(
            added
                .iter()
                .all(|message| message.synthetic && message.text.contains("switched from")),
            "{added:#?}"
        );
        let persona = after_second_reassemble
            .messages
            .iter()
            .find(|message| {
                message.role == atomcode_kernel::message::Role::System
                    && message.text.starts_with("You are AtomCode")
            })
            .expect("sessionless reassemble must preserve a coding persona");
        assert!(persona.text.contains("next-again"));
        assert!(!persona.text.contains("running test"));
        runtime.handle.shutdown().await.unwrap();
    }

    /// A conversation the log cannot say — one with a prompt the session never
    /// saw — is restored by rebuilding, and a candidate provider that cannot be
    /// built takes the conversation back to what it was.
    ///
    /// The route matters as much as the rollback: see `live_can_say`. A restore
    /// that committed that prompt as a fact would hand the live agent something
    /// to answer, which is a restore that talks to the model.
    #[tokio::test]
    async fn failed_sessionless_restore_rolls_back_to_the_original_snapshot() {
        let factory = Arc::new(FailSecondBuildFactory {
            builds: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut start = native_start(false);
        start.provider_factory = factory;
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        runtime
            .handle
            .submit(UserInput::from("original prompt"))
            .await
            .unwrap();
        let original = loop {
            if let CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                snapshot, ..
            }) = runtime.events.recv().await.unwrap().event
            {
                break snapshot;
            }
        };
        let mut candidate = original.as_ref().clone();
        candidate.messages.push(Message::user("replacement prompt"));

        assert!(matches!(
            runtime.handle.restore_snapshot(candidate).await,
            Err(RuntimeError::ReconfigureFailed(message))
                if message.contains("candidate provider failed")
        ));
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        let restored = runtime.handle.snapshot().await.unwrap();
        assert_eq!(restored.as_ref(), original.as_ref());
        assert!(restored
            .messages
            .iter()
            .all(|message| message.text != "replacement prompt"));
        runtime.handle.shutdown().await.unwrap();
    }

    /// A restore the log *can* say — going back to a conversation this session
    /// already had — is facts in the live log and nothing else: the provider the
    /// conversation is running on is kept, and the model is not asked anything
    /// (`docs/adr/0022` §2, `docs/adr/0024` §17).
    ///
    /// A rebuild here would close the fact stream every front end is reading
    /// and build a second provider for a session that never changed.
    #[tokio::test]
    async fn a_restore_the_log_can_say_is_facts_and_keeps_the_provider() {
        let factory = Arc::new(FailSecondBuildFactory {
            builds: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut start = native_start(false);
        start.provider_factory = factory.clone();
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        let mut after = Vec::new();
        for text in ["first prompt", "second prompt"] {
            runtime.handle.submit(UserInput::from(text)).await.unwrap();
            loop {
                if let CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    snapshot,
                    ..
                }) = runtime.events.recv().await.unwrap().event
                {
                    after.push(snapshot);
                    break;
                }
            }
        }
        let built = factory.builds.load(Ordering::Acquire);
        // The conversation as it stood after the first turn: a truncation of the
        // one that is live, which is what an undo is and what the log says with
        // one `Rewound`.
        let candidate = after[0].as_ref().clone();

        runtime
            .handle
            .restore_snapshot(candidate)
            .await
            .expect("a restore the log can say needs nothing built");
        assert_eq!(
            factory.builds.load(Ordering::Acquire),
            built,
            "the live session kept the provider it was running on: a second \
             build is a rebuilt runtime"
        );
        let restored = runtime.handle.snapshot().await.unwrap();
        assert!(
            restored
                .messages
                .iter()
                .all(|message| message.text != "second prompt"),
            "the conversation went back: {:#?}",
            restored.messages
        );
        // And nothing was sent: a restore is not a prompt. `TurnStarted` after
        // the restore is the live agent answering a fact the restore committed.
        let mut started = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(300), runtime.events.recv()).await
        {
            if matches!(
                event.event,
                CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { .. })
            ) {
                started.push(event.event);
            }
        }
        assert!(
            started.is_empty(),
            "a restore asked the model for something: {started:#?}"
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Ready);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn undo_rejects_a_snapshot_after_the_conversation_revision_changes() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();
        runtime
            .handle
            .submit(UserInput::from("first prompt"))
            .await
            .unwrap();
        loop {
            if matches!(
                runtime.events.recv().await.unwrap().event,
                CodingRuntimeEvent::TurnFinished(_)
            ) {
                break;
            }
        }

        let original = runtime.handle.snapshot_with_revision().await.unwrap();
        let undo = undo_snapshot_to_prompt(&original.snapshot, None).unwrap();

        runtime
            .handle
            .submit(UserInput::from("second prompt"))
            .await
            .unwrap();
        loop {
            if matches!(
                runtime.events.recv().await.unwrap().event,
                CodingRuntimeEvent::TurnFinished(_)
            ) {
                break;
            }
        }

        let (done, result) = oneshot::channel();
        runtime
            .handle
            .tx
            .send(CodingRuntimeControl::ApplyUndo {
                code_rewound_to: None,
                generation: runtime.handle.status().generation,
                expected_revision: original.revision,
                original: original.snapshot,
                truncated: undo.snapshot,
                restored_prompt: undo.restored_prompt,
                target_n: undo.target_n,
                prompts_before: undo.prompts_before,
                done,
            })
            .unwrap();

        assert!(matches!(result.await.unwrap(), Err(RuntimeError::Busy)));
        let current = runtime.handle.snapshot().await.unwrap();
        assert!(current
            .messages
            .iter()
            .any(|message| message.text == "second prompt"));
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn native_undo_snapshot_cas_preserves_a_newer_canonical_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let id = "undo-snapshot-cas";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let initial = SessionSnapshot::new(vec![
            Message::user("first prompt"),
            Message::assistant("first answer", Vec::new()),
        ]);
        persist_native_session(&manager, id, project.path(), &initial);
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(id.into());
        let runtime = CodingRuntime::start(start).await.unwrap();
        let original = runtime.handle.snapshot_with_revision().await.unwrap();
        let undo = undo_snapshot_to_prompt(&original.undo_snapshot, None).unwrap();
        let newer = SessionSnapshot::new(vec![
            Message::user("first prompt"),
            Message::assistant("first answer", Vec::new()),
            Message::user("concurrent prompt"),
        ]);
        // Another writer's fact lands in the log behind the runtime's back.
        let stored = manager.load_events(id).unwrap();
        let next = stored.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        let turn = stored.iter().map(|e| e.event.turn()).max().unwrap_or(0) + 1;
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(manager.events_path(id).unwrap())
            .unwrap();
        for (seq, event) in [
            (
                next,
                atomcode_kernel::session::SessionEvent::TurnStart { turn },
            ),
            (
                next + 1,
                atomcode_kernel::session::SessionEvent::UserMessage {
                    turn,
                    text: "concurrent prompt".into(),
                    images: Vec::new(),
                },
            ),
        ] {
            use std::io::Write;
            writeln!(
                log,
                "{}",
                serde_json::json!({ "seq": seq, "at": 0, "event": event })
            )
            .unwrap();
        }
        let (done, result) = oneshot::channel();
        runtime
            .handle
            .tx
            .send(CodingRuntimeControl::ApplyUndo {
                code_rewound_to: None,
                generation: runtime.handle.status().generation,
                expected_revision: original.revision,
                original: original.undo_snapshot,
                truncated: undo.snapshot,
                restored_prompt: undo.restored_prompt,
                target_n: undo.target_n,
                prompts_before: undo.prompts_before,
                done,
            })
            .unwrap();

        assert!(matches!(result.await.unwrap(), Err(RuntimeError::Busy)));
        assert_eq!(manager.load_snapshot(id).unwrap().messages, newer.messages);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn native_undo_persistence_error_fail_closes_an_unhealthy_aggregate() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let id = "undo-unhealthy-native";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let snapshot = SessionSnapshot::new(vec![
            Message::user("first prompt"),
            Message::assistant("answer", Vec::new()),
        ]);
        persist_native_session(&manager, id, project.path(), &snapshot);
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(id.into());
        let runtime = CodingRuntime::start(start).await.unwrap();
        let log_path = manager.events_path(id).unwrap();
        std::fs::remove_file(&log_path).unwrap();

        let error = runtime.handle.undo_to_prompt(None).await.unwrap_err();

        let RuntimeError::ReconfigureFailed(message) = &error else {
            panic!("expected session log persistence error, got {error:?}");
        };
        assert!(
            message.contains(log_path.to_string_lossy().as_ref()),
            "expected missing session log path in error, got {error:?}"
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Failed);
        assert_eq!(
            runtime.handle.submit(UserInput::from("must fail")).await,
            Err(RuntimeError::Unavailable)
        );
        // Sticky, not just refused once: a runtime that could not prove the
        // undo was kept does not come back through a reload either.
        assert_eq!(
            runtime.handle.reload_capabilities().await,
            Err(RuntimeError::Unavailable)
        );
        runtime.handle.shutdown().await.unwrap();
    }

    /// The rebuild's own rollback, when *that* cannot be persisted either: the
    /// runtime stops and stays stopped.
    ///
    /// Judged here rather than on the undo route as well, because an undo of the
    /// live session no longer rebuilds (`live_can_say`) — only a conversation
    /// the log cannot say still goes that way, and this is it.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn restore_snapshot_rollback_persistence_failure_is_sticky() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let id = "restore-rollback-persistence-failure";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let initial = SessionSnapshot::new(vec![Message::user("initial")]);
        persist_native_session(&manager, id, project.path(), &initial);
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(id.into());
        start.provider_factory = Arc::new(DeleteLogAndFailSecondBuildFactory {
            builds: std::sync::atomic::AtomicUsize::new(0),
            log_path: manager.events_path(id).unwrap(),
        });
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        let live_snapshot = runtime.handle.snapshot().await.unwrap();
        let mut replacement = live_snapshot.as_ref().clone();
        replacement.messages.push(Message::user("replacement"));

        let restore = runtime.handle.restore_snapshot(replacement).await;
        assert!(
            matches!(
                &restore,
                Err(RuntimeError::ReconfigureFailed(message))
                    if message.contains("snapshot restore failed")
            ),
            "unexpected restore result: {restore:?}"
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Failed);
        let mut saw_rollback_persistence_error = false;
        while let Ok(event) = runtime.events.try_recv() {
            if let CodingRuntimeEvent::Agent(AgentEvent::Error { message, .. }) = event.event {
                saw_rollback_persistence_error |=
                    message.contains("snapshot rollback persistence failed");
            }
        }
        assert!(saw_rollback_persistence_error);
        assert_eq!(
            runtime.handle.submit(UserInput::from("must fail")).await,
            Err(RuntimeError::Unavailable)
        );
        assert_eq!(
            runtime.handle.reload_capabilities().await,
            Err(RuntimeError::Unavailable)
        );
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn native_undo_rollback_merges_concurrent_sidecar_updates() {
        use atomcode_capabilities::session::presentation::PRESENTATION_VERSION;
        use atomcode_capabilities::session::{
            ImportInfo, ImportKind, PresentationRole, SessionManager,
        };

        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let id = "undo-sidecar-merge";
        let manager = SessionManager::for_project(project.path());
        let original_snapshot = SessionSnapshot::new(vec![
            Message::user("first"),
            Message::assistant("first answer", Vec::new()),
            Message::user("second"),
            Message::assistant("second answer", Vec::new()),
        ]);
        let original_stats = vec![
            TurnStat {
                after_message: 2,
                position_valid: true,
                turn_id: 1,
                round_count: 1,
                tool_call_count: 0,
                duration_ms: 10,
                total_tokens: 20,
                errored: false,
                used_tokens: 10,
                ctx_window: 1_000,
                model_usage: Vec::new(),
            },
            TurnStat {
                after_message: 4,
                position_valid: true,
                turn_id: 2,
                round_count: 1,
                tool_call_count: 1,
                duration_ms: 30,
                total_tokens: 40,
                errored: false,
                used_tokens: 20,
                ctx_window: 1_000,
                model_usage: Vec::new(),
            },
        ];
        let at_start = PresentationEntry {
            anchor: DisplayAnchor::AtStart,
            role: PresentationRole::Assistant,
            text: "session header".into(),
        };
        let first_turn = PresentationEntry {
            anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
            role: PresentationRole::Assistant,
            text: "first divider".into(),
        };
        let removed_second_turn = PresentationEntry {
            anchor: DisplayAnchor::AfterTurn { turn_id: 2 },
            role: PresentationRole::Assistant,
            text: "second divider".into(),
        };
        let initial_presentation = PresentationFile {
            v: PRESENTATION_VERSION,
            entries: vec![
                at_start.clone(),
                first_turn.clone(),
                removed_second_turn.clone(),
            ],
        };
        let mut original_meta = SessionMeta::new(id, project.path().to_string_lossy(), 100);
        original_meta.owner = StorageOwner::Native;
        original_meta.message_count = 4;
        original_meta.turn_count = 2;
        original_meta.turn_stats = original_stats.clone();
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .commit_native_import(
                &lease,
                Some(&original_snapshot),
                Some(&initial_presentation),
                &original_meta,
            )
            .unwrap();
        drop(lease);

        let CodingRuntimeStart {
            mut agent,
            mut prepare,
            provider_factory,
            plugin_hooks,
            image_preprocessor,
        } = native_start(false);
        agent.working_dir = project.path().to_path_buf();
        prepare.session = crate::SessionMode::Resume(id.into());
        let parts = prepare_with_plugin_hook_source(&agent, prepare.clone(), plugin_hooks.as_ref())
            .await
            .unwrap();
        let (wakeup_tx, _wakeup_rx) = mpsc::unbounded_channel();
        let mut resources = RuntimeResources {
            config: agent,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            harness_app: None,
            harness_providers: None,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor,
        };

        let mut truncated = original_snapshot.clone();
        truncated.messages.truncate(2);
        let receipt =
            persist_runtime_undo(&mut resources, Some(&original_snapshot), &truncated, None)
                .unwrap()
                .expect("native undo must retain a sidecar rollback receipt");
        assert_eq!(
            manager.load_snapshot(id).unwrap().messages,
            truncated.messages
        );
        let persisted_meta = manager.read_meta(id).unwrap();
        assert_eq!(persisted_meta.turn_stats, vec![original_stats[0].clone()]);
        assert_eq!(persisted_meta.turn_count, 1);
        assert_eq!(persisted_meta.detached_unattributed_tokens, 40);
        assert_eq!(
            manager.read_presentation(id).unwrap().entries,
            vec![at_start.clone(), first_turn.clone()]
        );

        let concurrent_import = ImportInfo {
            legacy_schema: "test-v1".into(),
            source_sha256: "a".repeat(64),
            importer_version: 3,
            kind: ImportKind::MetadataOnly,
        };
        manager.rename(id, "renamed while undo rebuilds").unwrap();
        manager
            .update_meta(id, |meta| {
                meta.ai_named = true;
                meta.import_info = Some(concurrent_import.clone());
                meta.detached_unattributed_tokens =
                    meta.detached_unattributed_tokens.saturating_add(3);
                meta.updated_at = 1;
            })
            .unwrap();
        let concurrent_append = PresentationEntry {
            anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
            role: PresentationRole::User,
            text: "appended while undo rebuilds".into(),
        };
        manager
            .append_presentation(id, concurrent_append.clone())
            .unwrap();
        let rollback_started_at = atomcode_capabilities::session::now_ms();

        restore_runtime_undo(
            &mut resources,
            &truncated,
            &original_snapshot,
            Some(receipt),
        )
        .unwrap();

        assert_eq!(
            manager.load_snapshot(id).unwrap().messages,
            original_snapshot.messages
        );
        let restored_meta = manager.read_meta(id).unwrap();
        assert_eq!(restored_meta.owner, StorageOwner::Native);
        assert_eq!(restored_meta.name, "renamed while undo rebuilds");
        assert!(restored_meta.user_renamed);
        assert!(restored_meta.ai_named);
        assert_eq!(restored_meta.import_info, Some(concurrent_import));
        assert_eq!(restored_meta.message_count, 4);
        assert_eq!(restored_meta.turn_count, 2);
        assert_eq!(restored_meta.turn_stats, original_stats);
        assert_eq!(
            restored_meta.detached_unattributed_tokens, 3,
            "rollback must remove only its archive delta and preserve concurrent usage"
        );
        assert!(restored_meta.updated_at >= rollback_started_at);
        assert_eq!(
            manager.read_presentation(id).unwrap().entries,
            vec![at_start, first_turn, removed_second_turn, concurrent_append,]
        );

        let mut second_truncated = original_snapshot.clone();
        second_truncated.messages.truncate(2);
        let second_receipt = persist_runtime_undo(
            &mut resources,
            Some(&original_snapshot),
            &second_truncated,
            None,
        )
        .unwrap()
        .expect("second native undo must retain a rollback receipt");
        let concurrently_advanced = SessionSnapshot::new(vec![
            Message::user("concurrent"),
            Message::assistant("newer answer", Vec::new()),
        ]);
        let binding = resources.parts.session.as_ref().unwrap();
        manager
            .append_conversation_change(&binding.lease, &concurrently_advanced.messages, 1)
            .unwrap();

        let error = restore_runtime_undo(
            &mut resources,
            &second_truncated,
            &original_snapshot,
            Some(second_receipt),
        )
        .unwrap_err();
        assert!(error.is_snapshot_conflict());
        assert_eq!(
            manager.load_snapshot(id).unwrap().messages,
            concurrently_advanced.messages
        );
    }

    #[test]
    fn uncertain_session_commit_requires_runtime_fail_close() {
        let error = NativePersistenceError::from(SessionStoreError::UncertainCommit {
            id: "s1".into(),
            commit_error: "meta fsync failed".into(),
            rollback_errors: vec!["snapshot rollback failed".into()],
        });

        assert!(error.requires_fail_close());
        assert!(error.to_string().contains("rollback was incomplete"));
        assert_eq!(
            persistence_fail_close_reason(&error, None),
            Some(error.to_string())
        );
        let certain_candidate = NativePersistenceError::certain("provider build failed");
        let restore_error = NativePersistenceError::certain("rollback write failed");
        assert_eq!(
            persistence_fail_close_reason(&certain_candidate, Some(&restore_error)),
            Some("snapshot rollback persistence failed: rollback write failed".into())
        );
    }

    #[test]
    fn offline_undo_preserves_snapshot_counters_and_truncates_at_selected_prompt() {
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system("system"),
            Message::user("first"),
            Message::assistant("answer", Vec::new()),
            Message::user("second"),
            Message::assistant("answer 2", Vec::new()),
        ]);
        snapshot.turn_counter = 9;
        snapshot.request_counter = 12;

        let undo = undo_snapshot_to_prompt(&snapshot, Some(2)).unwrap();

        assert_eq!(undo.restored_prompt, "second");
        assert_eq!(undo.prompts_before, 2);
        assert_eq!(undo.snapshot.messages.len(), 3);
        assert_eq!(undo.snapshot.turn_counter, 9);
        assert_eq!(undo.snapshot.request_counter, 12);
    }

    #[tokio::test]
    async fn persisted_snapshot_uses_v2_strategy_and_kernel_apply() {
        use atomcode_kernel::stream::StreamEvent;
        use atomcode_kernel::testkit::MockProvider;

        let messages = vec![
            Message::system("persona"),
            Message::user("original task"),
            Message::assistant("x".repeat(40_000), Vec::new()),
            Message::user("follow-up"),
            Message::assistant("y".repeat(40_000), Vec::new()),
            Message::user("active turn"),
        ];
        let provider = Arc::new(
            MockProvider::new(vec![vec![
                StreamEvent::TextDelta("anchored summary".into()),
                StreamEvent::Done { truncated: false },
            ]])
            .with_ctx_window(128_000),
        );

        let result = compact_snapshot(messages, provider, None).await;

        assert!(result.outcome.committed);
        assert!(matches!(
            result.mutation,
            SnapshotCompactionMutation::Replace { .. }
        ));
        assert_eq!(result.messages[0].text, "persona");
        assert_eq!(result.messages[1].text, "original task");
        assert!(result
            .messages
            .iter()
            .any(|message| message.text.contains("anchored summary")));
    }

    // After the evaluator returns Met, the runtime must keep the goal registered
    // with phase=Satisfied (not clear it).  The last GoalChanged event must carry
    // phase==Satisfied and active==false.
    //
    // NOTE: This test validates the *event contract only* — it observes the
    // GoalChanged(phase=Satisfied) event, which fires before any potential
    // goal=None clearing, so it was green even before the keep-goal-on-Met fix
    // and does NOT falsify the in-memory keep-goal change.  The falsifying
    // coverage (confirming a still-registered goal after Met) arrives in Task 4
    // (Submit-while-Satisfied), which can only succeed if `goal` is still set.
    #[tokio::test]
    async fn met_goal_keeps_goal_registered_with_phase_satisfied() {
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime(Arc::new(GoalMetProviderFactory)).await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await; // GoalChanged(active=true)
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("done", vec![])]),
            })
            .unwrap();

        // Drain events until TurnFinished; collect the last GoalChanged seen.
        let mut last_goal_progress: Option<GoalProgress> = None;
        let _terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(p)) => last_goal_progress = Some(p),
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before met terminal"),
                }
            }
        })
        .await
        .expect("met goal did not produce a turn terminal");

        let progress = last_goal_progress.expect("no GoalChanged event was emitted after Met");
        assert_eq!(
            progress.phase,
            GoalPhase::Satisfied,
            "goal phase must be Satisfied after Met; got {:?}",
            progress.phase
        );
        assert!(!progress.active, "goal must be inactive after Met");
        assert_eq!(
            progress.terminal,
            Some(GoalTerminal::Met),
            "goal terminal must be Met"
        );

        handle.shutdown().await.unwrap();
    }

    // After a round-cap fires, the runtime must keep the goal registered with
    // phase=PausedAtCap (not clear it).  The last GoalChanged event must carry
    // phase==PausedAtCap and active==false.
    #[tokio::test]
    async fn cap_goal_keeps_goal_registered_with_phase_paused_at_cap() {
        let mut config = native_start(false).agent;
        config.goal_max_rounds = 1;
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime_with_config(
            Arc::new(GoalNotMetProviderFactory::default()),
            config,
        )
        .await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await; // GoalChanged(active=true)
        let _ = kernel_commands.recv().await;

        for attempt in 0..2 {
            kernel_events
                .send(AgentEvent::TurnComplete {
                    turn: None,
                    reason: StopReason::Stopped,
                })
                .unwrap();
            assert!(matches!(
                kernel_commands.recv().await,
                Some(AgentCommand::Snapshot)
            ));
            kernel_events
                .send(AgentEvent::Snapshot {
                    snapshot: SessionSnapshot::new(vec![Message::assistant("not done", vec![])]),
                })
                .unwrap();
            if attempt == 0 {
                // First round: evaluator says NotMet → continuation dispatched.
                assert!(matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                        .await
                        .expect("first goal continuation was not dispatched"),
                    Some(AgentCommand::SendSyntheticMessage { .. })
                ));
            }
        }

        // Drain events until TurnFinished; collect the last GoalChanged seen.
        let mut last_goal_progress: Option<GoalProgress> = None;
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(p)) => last_goal_progress = Some(p),
                    Some(CodingRuntimeEvent::TurnFinished(completion)) => break completion,
                    Some(_) => {}
                    None => panic!("runtime events closed before cap terminal"),
                }
            }
        })
        .await
        .expect("goal round cap did not produce a turn terminal");

        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                reason: StopReason::MaxRounds,
                ..
            }
        ));

        let progress =
            last_goal_progress.expect("no GoalChanged event was emitted after round cap");
        assert_eq!(
            progress.phase,
            GoalPhase::PausedAtCap,
            "goal phase must be PausedAtCap after round cap; got {:?}",
            progress.phase
        );
        assert!(!progress.active, "goal must be inactive after round cap");
        assert_eq!(
            progress.terminal,
            Some(GoalTerminal::Stopped),
            "goal terminal must be Stopped after round cap"
        );

        handle.shutdown().await.unwrap();
    }

    // When the goal is PausedAtCap and the user submits, the runtime must:
    // 1. Emit GoalChanged with phase==Pursuing (round reset to 0)
    // 2. Deliver the submitted input as the normal user message (SendMessage)
    #[tokio::test]
    async fn submit_while_paused_at_cap_resumes_goal_and_delivers_message() {
        let mut config = native_start(false).agent;
        config.goal_max_rounds = 1;
        let (
            handle,
            mut kernel_commands,
            kernel_events,
            mut runtime_events,
            _wakeup_tx,
            _loop_active,
            _adapter,
        ) = controller_test_runtime_with_config(
            Arc::new(GoalNotMetProviderFactory::default()),
            config,
        )
        .await;

        // --- Drive the goal to PausedAtCap ---
        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await; // GoalChanged(active=true)
        let _ = kernel_commands.recv().await; // SendMessage

        for attempt in 0..2 {
            kernel_events
                .send(AgentEvent::TurnComplete {
                    turn: None,
                    reason: StopReason::Stopped,
                })
                .unwrap();
            assert!(matches!(
                kernel_commands.recv().await,
                Some(AgentCommand::Snapshot)
            ));
            kernel_events
                .send(AgentEvent::Snapshot {
                    snapshot: SessionSnapshot::new(vec![Message::assistant("not done", vec![])]),
                })
                .unwrap();
            if attempt == 0 {
                // First round: evaluator says NotMet → continuation dispatched.
                assert!(matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), kernel_commands.recv())
                        .await
                        .expect("first goal continuation was not dispatched"),
                    Some(AgentCommand::SendSyntheticMessage { .. })
                ));
            }
        }

        // Wait until we see TurnFinished (goal capped) - drain GoalChanged events.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                    Some(_) => {}
                    None => panic!("events closed before cap terminal"),
                }
            }
        })
        .await
        .expect("cap turn did not finish");

        // --- Goal is now PausedAtCap; submit new message ---
        let submit_text = "please continue";
        handle.submit(UserInput::from(submit_text)).await.unwrap();

        // Collect events until we see SendMessage or timeout.
        let mut saw_goal_changed_pursuing = false;
        let mut saw_send_message_with_input = false;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    // biased; ensures runtime_events is drained before
                    // kernel_commands, matching the emission order (GoalChanged
                    // is enqueued before SendMessage) and preventing CI flakes.
                    biased;
                    event = runtime_events.recv() => {
                        match event {
                            Some(CodingRuntimeEvent::GoalChanged(p)) => {
                                if p.phase == GoalPhase::Pursuing && p.round == 0 {
                                    saw_goal_changed_pursuing = true;
                                }
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                    cmd = kernel_commands.recv() => {
                        match cmd {
                            Some(AgentCommand::SendMessage { text, .. }) => {
                                if text == submit_text {
                                    saw_send_message_with_input = true;
                                }
                                break;
                            }
                            _ => break,
                        }
                    }
                }
            }
        })
        .await
        .expect("submit after PausedAtCap did not deliver message within timeout");

        assert!(
            saw_goal_changed_pursuing,
            "submit while PausedAtCap must emit GoalChanged(phase=Pursuing, round=0)"
        );
        assert!(
            saw_send_message_with_input,
            "submit while PausedAtCap must deliver the input as a user message"
        );

        handle.shutdown().await.unwrap();
    }

    /// A slow classifier: the follow-up question takes a while to answer, the
    /// way a real model call does. The evaluator's own verdict stays instant so
    /// only the classifier is being measured.
    /// A provider that takes its time on every call, the way a real one does.
    ///
    /// The negative control for the keypress path: any model call the runtime
    /// makes *on that path* shows up as time on the clock. Its reply serves the
    /// goal evaluator, which is the one call that is allowed to be slow — it
    /// runs spawned, off the owner loop, while the turn is held.
    struct SlowProvider {
        delay: std::time::Duration,
    }
    #[async_trait::async_trait]
    impl LlmProvider for SlowProvider {
        fn model_name(&self) -> &str {
            "slow-test-provider"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[atomcode_kernel::tool::ToolDef],
            _options: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            use atomcode_kernel::stream::StreamEvent;
            tokio::time::sleep(self.delay).await;
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("Verdict: yes goal met".into()),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }
    struct SlowProviderFactory {
        delay: std::time::Duration,
    }
    impl CodingProviderFactory for SlowProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(SlowProvider { delay: self.delay }))
        }
    }

    /// A provider whose call never returns: the goal's evaluator is still
    /// thinking, so the turn it is judging stays held.
    ///
    /// `built` fires when the runtime asks for an evaluator, which it does on
    /// the owner loop immediately before holding the turn — the test's proof
    /// that the hold is in place rather than a race with it.
    struct NeverAnsweringProviderFactory {
        built: mpsc::UnboundedSender<()>,
    }
    struct NeverAnsweringProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NeverAnsweringProvider {
        fn model_name(&self) -> &str {
            "never-answering-test-provider"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[atomcode_kernel::tool::ToolDef],
            _options: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            std::future::pending().await
        }
    }
    impl CodingProviderFactory for NeverAnsweringProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            let _ = self.built.send(());
            Ok(Arc::new(NeverAnsweringProvider))
        }
    }

    /// Typing while a goal round is being judged starts a turn, it does not
    /// steer one.
    ///
    /// Between rounds the agent has finished and the runtime is HOLDING the
    /// turn open while the evaluator decides. There is nothing live to fold
    /// into: a message sent now opens a turn of its own at the agent. Reporting
    /// it as a steer was a claim about a turn that had already ended, and two
    /// things followed from it — a driver waiting for the `Steered` that closes
    /// its steering panel waited forever, and the verdict, arriving later,
    /// closed the HELD turn while the agent was busy with the person's new one,
    /// so the screen went idle in the middle of work.
    ///
    /// The goal is not harmed by this: it stays Pursuing, and the end of the
    /// turn this starts evaluates and continues it like any other round's.
    #[tokio::test]
    async fn a_message_typed_while_a_goal_round_is_judged_starts_its_own_turn() {
        let (built_tx, mut built) = mpsc::unbounded_channel();
        let (handle, mut kernel_commands, kernel_events, mut runtime_events, _w, _l, _a) =
            controller_test_runtime(Arc::new(NeverAnsweringProviderFactory { built: built_tx }))
                .await;

        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await; // GoalChanged(active=true)
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        while built.try_recv().is_ok() {} // anything built while starting up
                                          // The round ends; the runtime holds the turn and asks the evaluator,
                                          // which never answers.
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("round one", vec![])]),
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), built.recv())
            .await
            .expect("the runtime never asked for an evaluator")
            .expect("the evaluator channel closed");

        // Nothing is live at the agent now. What a person types opens a turn.
        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle.submit(UserInput::from("actually, do this first")),
        )
        .await
        .expect("submit did not answer while the turn was held")
        .expect("submit was refused while the turn was held");
        assert!(
            matches!(receipt, SubmitReceipt::Started { .. }),
            "a held turn has no live turn to steer: {receipt:?}"
        );

        // And the round it was holding is reported finished rather than left
        // open to be closed later, under the agent's new turn.
        let mut finished = false;
        while let Ok(event) = runtime_events.try_recv() {
            if matches!(event, CodingRuntimeEvent::TurnFinished(_)) {
                finished = true;
            }
        }
        assert!(finished, "the held round must report its terminal");
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), kernel_commands.recv())
                .await
                .expect("the message never reached the agent"),
            Some(AgentCommand::SendMessage { .. })
        ));
        handle.shutdown().await.unwrap();
    }

    /// A met goal is CLOSED, so the next thing a person says is an ordinary
    /// turn — and it reaches the agent on the keypress.
    ///
    /// Both halves are the criterion, and both come from the same report.
    ///
    /// * **At once.** A met goal used to stay registered and put the next
    ///   message to a classifier ("does this continue the goal?") — awaited on
    ///   the owner loop, up to 4s. Nothing on screen said so: the composer was
    ///   already cleared, no fact had been logged for the transcript, and a
    ///   screen that is idle after a goal ends draws no steering panel either.
    ///   That was the reported "我说的话一会之后才出现, 出现之前屏幕上没有任何
    ///   变化". The provider here is slow on every call, so any model call made
    ///   on the keypress path shows up as time on the clock.
    /// * **Ordinary.** No `GoalChanged` puts a goal back into `Pursuing`. An
    ///   autonomous loop that came back on its own would be one nobody was told
    ///   about — the badge went when the goal was met. `/goal` starts another.
    ///
    /// Virtual clock: the delay is never really waited out, and what is measured
    /// is the runtime's own await rather than the machine.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_met_goal_is_closed_so_the_next_message_is_an_ordinary_turn_at_once() {
        let (handle, mut kernel_commands, kernel_events, mut runtime_events, _w, _l, _a) =
            controller_test_runtime(Arc::new(SlowProviderFactory {
                delay: std::time::Duration::from_secs(3),
            }))
            .await;

        // --- Drive the goal to Met ---
        handle.start_goal("tests pass").await.unwrap();
        let _ = runtime_events.recv().await; // GoalChanged(active=true)
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::assistant("done", vec![])]),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match runtime_events.recv().await {
                    Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                    Some(_) => {}
                    None => panic!("events closed before met terminal"),
                }
            }
        })
        .await
        .expect("satisfied turn did not finish");

        // --- The keypress ---
        let submit_text = "so what about the other half?";
        let at = tokio::time::Instant::now();
        handle.submit(UserInput::from(submit_text)).await.unwrap();
        let reached = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match kernel_commands.recv().await {
                    Some(AgentCommand::SendMessage { text, .. }) if text.contains(submit_text) => {
                        break tokio::time::Instant::now()
                    }
                    Some(_) => {}
                    None => panic!("commands closed before the message was forwarded"),
                }
            }
        })
        .await
        .expect("the message never reached the agent");

        let waited = reached.duration_since(at);
        assert!(
            waited < std::time::Duration::from_millis(100),
            "a person's message waited {waited:?} inside the runtime before reaching the agent"
        );
        let mut reengaged = None;
        while let Ok(event) = runtime_events.try_recv() {
            if let CodingRuntimeEvent::GoalChanged(progress) = event {
                if progress.phase == GoalPhase::Pursuing {
                    reengaged = Some(progress);
                }
            }
        }
        assert!(
            reengaged.is_none(),
            "a met goal must not come back on its own: {reengaged:?}"
        );
        handle.shutdown().await.unwrap();
    }
}
