//! Stable driver control plane and kernel-agent owner for a coding runtime.
//!
//! The runtime owns the replaceable kernel [`AgentHandle`] so native controls and
//! events never need to traverse a legacy driver adapter.

use std::collections::{BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::checkpoint::CompactionCheckpointError;
use atomcode_kernel::event::{AgentCommand, AgentEvent, RequestId, StopReason};
pub use atomcode_kernel::message::CompactTrigger;
use atomcode_kernel::message::{
    CompactionStrategy, CompactionView, Conversation, ImageContent, Message, MessageMeta,
    SessionSnapshot,
};
use atomcode_kernel::provider::LlmProvider;
use tokio::sync::{mpsc, oneshot, watch};

use crate::controllers::{
    evaluate_goal, goal_continuation_message, summarize_for_goal, EvalOutcome, GoalProgress,
    GoalResult, GoalState, LoopProgress, LoopState, ScheduleWakeupTool, WakeupRequest,
    MAX_EVAL_FAILURES, MAX_UNPRODUCTIVE,
};
use crate::{
    assemble, prepare_with_plugin_hook_source, CodingAgentConfig, CodingProviderFactory,
    PluginHookSource, PrepareOptions,
};

/// Runtime facts emitted by the coding engine without depending on the legacy
/// `atomcode-core` driver protocol.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum CodingRuntimeEvent {
    /// A kernel observation that is not owned as a runtime terminal/request.
    Agent(AgentEvent),
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
    SessionNameSuggested {
        name: String,
    },
    SessionChanged(SessionChanged),
    WorkingDirectoryChanged(std::path::PathBuf),
    GoalChanged(GoalProgress),
    LoopChanged(LoopProgress),
    UndoFinished(Result<UndoResult, RuntimeError>),
    ContextStatsRefreshed(Result<RuntimeContextStats, RuntimeError>),
    SnapshotRestoreFinished {
        correlation_id: u64,
        result: Result<Arc<SessionSnapshot>, RuntimeError>,
    },
    ProviderReloadFinished(Result<RuntimeGeneration, RuntimeError>),
    ControllerWarning(String),
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

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
)]
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
    pub duration: std::time::Duration,
    pub turn_count: usize,
    pub tool_call_count: usize,
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

/// Ordered, fire-and-forget driver requests. This is the native replacement for
/// the core AgentCommand channel during asynchronous runtime startup.
#[derive(Clone, Debug)]
pub enum DriverCommand {
    Submit(UserInput),
    Respond {
        id: RequestId,
        value: serde_json::Value,
    },
    Cancel,
    Compact(Option<String>),
    SetMode(RuntimeMode),
    QueueLocalContext(LocalContextInput),
    ReloadProvider(CodingAgentConfig),
    UndoToPrompt(Option<usize>),
    RefreshContextStats,
    FreshSession,
    ReloadCapabilities,
    ResumeSession(String),
    ChangeDirectory(std::path::PathBuf),
    RestoreSnapshot(SessionSnapshot),
    RestoreSnapshotCorrelated {
        snapshot: SessionSnapshot,
        correlation_id: u64,
    },
    StartGoal(String),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitReceipt {
    Started { generation: u64, turn_id: u64 },
    Steered { generation: u64, turn_id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    Busy,
    StaleRequest { id: RequestId },
    DeliveryFailed,
    Unavailable,
    SnapshotUnavailable(String),
    ReconfigureFailed(String),
    InvalidWorkingDirectory(String),
    UndoOutOfRange { requested: usize, available: usize },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("coding runtime is busy"),
            Self::StaleRequest { id } => write!(f, "runtime request {id} is stale"),
            Self::DeliveryFailed => f.write_str("kernel command delivery failed"),
            Self::Unavailable => f.write_str("coding runtime is unavailable"),
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
        }
    }
}

impl Error for RuntimeError {}

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
pub struct CodingRuntimeStart {
    pub agent: CodingAgentConfig,
    pub prepare: PrepareOptions,
    pub provider_factory: Arc<dyn CodingProviderFactory>,
    pub plugin_hooks: Arc<dyn PluginHookSource>,
}

struct RuntimeResources {
    config: CodingAgentConfig,
    prepare: PrepareOptions,
    provider_factory: Arc<dyn CodingProviderFactory>,
    plugin_hooks: Arc<dyn PluginHookSource>,
    parts: crate::CodingParts,
    wakeup_tx: mpsc::UnboundedSender<WakeupRequest>,
    loop_active: Arc<std::sync::atomic::AtomicBool>,
}

/// A native coding runtime. Dropping `events` causes a fail-closed shutdown.
pub struct CodingRuntime {
    pub handle: CodingRuntimeHandle,
    pub events: CodingRuntimeEvents,
    pub task: tokio::task::JoinHandle<RuntimeExit>,
    pub session: Option<RuntimeSessionInfo>,
    pub mcp_events: Vec<atomcode_capabilities::mcp::McpConnectEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSessionInfo {
    pub id: String,
    pub resumed: bool,
}

#[derive(Debug)]
pub enum RuntimeStartError {
    Prepare(std::io::Error),
    Provider(crate::ProviderBuildError),
    Assemble(std::io::Error),
}

impl fmt::Display for RuntimeStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(error) => write!(f, "coding runtime prepare failed: {error}"),
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
    terminal: watch::Receiver<Option<RuntimeExit>>,
}

impl CodingRuntimeHandle {
    pub fn is_stopped(&self) -> bool {
        self.terminal.borrow().is_some() || self.tx.is_closed()
    }

    /// Current actor-owned lifecycle state projected for fast driver checks.
    pub fn status(&self) -> RuntimeStatus {
        runtime_status(self.state.load(Ordering::Acquire))
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
        if !runtime_state_available(state) && !matches!(command, DriverCommand::Shutdown) {
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
            DriverCommand::Cancel => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Cancel { generation, done }
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
            DriverCommand::UndoToPrompt(_) | DriverCommand::RefreshContextStats => {
                unreachable!("handled before control conversion")
            }
            DriverCommand::FreshSession => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Reprepare {
                    generation,
                    target: ReprepareTarget::Fresh,
                    done,
                }
            }
            DriverCommand::ReloadCapabilities => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Reprepare {
                    generation,
                    target: ReprepareTarget::Reload,
                    done,
                }
            }
            DriverCommand::ResumeSession(id) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Reprepare {
                    generation,
                    target: ReprepareTarget::Resume(id),
                    done,
                }
            }
            DriverCommand::ChangeDirectory(directory) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::Reprepare {
                    generation,
                    target: ReprepareTarget::ChangeDirectory(directory),
                    done,
                }
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
            DriverCommand::StartGoal(condition) => {
                let (done, _result) = oneshot::channel();
                CodingRuntimeControl::StartGoal {
                    generation,
                    condition,
                    done,
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
                    done,
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

    pub async fn snapshot(&self) -> Result<Arc<SessionSnapshot>, RuntimeError> {
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
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::Cancel {
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
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ContextStats {
                generation: runtime_state_generation(state),
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

    pub async fn reprepare(&self, input: ReprepareInput) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Exact(input)).await
    }

    pub async fn fresh_session(&self) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Fresh).await
    }

    pub async fn reload_capabilities(&self) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Reload).await
    }

    pub async fn resume_session(
        &self,
        id: impl Into<String>,
    ) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Resume(id.into()))
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
        let original = self.snapshot().await?;
        let plan = compute_runtime_undo(&original.messages, nth)?;
        let mut truncated = original.as_ref().clone();
        truncated.messages = plan.truncated;
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ApplyUndo {
                generation,
                original,
                truncated,
                restored_prompt: plan.restored_prompt,
                target_n: plan.target_n,
                prompts_before: plan.prompts_before,
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

    pub async fn start_goal(&self, condition: impl Into<String>) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::StartGoal {
                generation: runtime_state_generation(state),
                condition: condition.into(),
                done,
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

    pub async fn start_loop(&self, prompt: impl Into<String>) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::StartLoop {
                generation: runtime_state_generation(state),
                prompt: prompt.into(),
                done,
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
        let CodingRuntimeStart {
            mut agent,
            prepare,
            provider_factory,
            plugin_hooks,
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
        let mut parts =
            prepare_with_plugin_hook_source(&agent, prepare.clone(), plugin_hooks.as_ref())
                .await
                .map_err(RuntimeStartError::Prepare)?;
        parts.register_extra_tool(Arc::new(ScheduleWakeupTool::new(
            wakeup_tx.clone(),
            Arc::clone(&loop_active),
        )));
        let session_id = parts.session.as_ref().map(|binding| binding.id.as_str());
        let session = parts.session.as_ref().map(|binding| RuntimeSessionInfo {
            id: binding.id.clone(),
            resumed: binding.resume.is_some(),
        });
        let mcp_events = parts.mcp_events.clone();
        let provider = provider_factory
            .build(&agent, session_id)
            .map_err(RuntimeStartError::Provider)?;
        let kernel_agent = assemble(&mut parts, &agent, provider)
            .map_err(RuntimeStartError::Assemble)?
            .spawn();

        let (handle, controls) = coding_runtime_control_channel();
        let (raw_event_tx, _raw_events) = mpsc::unbounded_channel();
        let (tagged_event_tx, mut tagged_events) = mpsc::unbounded_channel();
        let adapter = spawn_runtime_owner_with_protocol(
            kernel_agent,
            controls,
            raw_event_tx,
            true,
            true,
            Some(tagged_event_tx),
            Some(RuntimeResources {
                config: agent,
                prepare,
                provider_factory,
                plugin_hooks,
                parts,
                wakeup_tx,
                loop_active,
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
            mcp_events,
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
    terminal_tx: watch::Sender<Option<RuntimeExit>>,
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
    Snapshot {
        generation: u64,
        done: oneshot::Sender<Result<Arc<SessionSnapshot>, RuntimeError>>,
    },
    Cancel {
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
        done: oneshot::Sender<Result<RuntimeContextStats, RuntimeError>>,
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
    Reprepare {
        generation: u64,
        target: ReprepareTarget,
        done: oneshot::Sender<Result<SessionChanged, RuntimeError>>,
    },
    ApplyUndo {
        generation: u64,
        original: Arc<SessionSnapshot>,
        truncated: SessionSnapshot,
        restored_prompt: String,
        target_n: usize,
        prompts_before: usize,
        done: oneshot::Sender<Result<UndoResult, RuntimeError>>,
    },
    RestoreSnapshot {
        generation: u64,
        snapshot: SessionSnapshot,
        done: oneshot::Sender<Result<SessionChanged, RuntimeError>>,
    },
    StartGoal {
        generation: u64,
        condition: String,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    StopGoal {
        generation: u64,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    StartLoop {
        generation: u64,
        prompt: String,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
    StopLoop {
        generation: u64,
        done: oneshot::Sender<Result<(), RuntimeError>>,
    },
}

#[doc(hidden)]
#[derive(Clone)]
pub enum ReprepareTarget {
    Exact(ReprepareInput),
    Reload,
    Fresh,
    Resume(String),
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
    (
        CodingRuntimeHandle {
            tx,
            state: Arc::clone(&state),
            terminal,
        },
        CodingRuntimeControlReceiver {
            rx,
            state,
            terminal_tx,
        },
    )
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
        _ => RuntimePhase::Failed,
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
    mut controls: CodingRuntimeControlReceiver,
    runtime_event_tx: mpsc::UnboundedSender<CodingRuntimeEvent>,
    initial_agent_available: bool,
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
    let (session_name_tx, mut session_name_rx) = mpsc::unbounded_channel::<(u64, String)>();
    let mut generation = 0;
    let event_generation = Arc::new(AtomicU64::new(generation));
    let runtime_event_tx = RuntimeEventEmitter {
        raw: runtime_event_tx,
        tagged: tagged_event_tx,
        generation: Arc::clone(&event_generation),
    };
    controls.state.store(
        runtime_state(generation, initial_agent_available),
        Ordering::Release,
    );

    let owner_task = tokio::spawn(async move {
        // Keep the fallback receiver pending for transitional owner tests/adapters
        // that do not mount the runtime-owned schedule_wakeup tool.
        let _wakeup_guard = _closed_wakeup_tx;
        let mut agent = initial;
        let mut observed_tokens = None;
        let mut controls_open = true;
        let mut compaction_suspended = false;
        let mut agent_available = initial_agent_available;
        let mut compactions = CompactionTracker::default();
        let mut shutdown_was_handled = false;
        let mut forced_shutdown = false;
        let mut exit_reason = RuntimeExitReason::OwnerStopped;
        let mut next_turn_id = 0u64;
        let mut active_turn = None;
        let mut pending_requests = BTreeSet::new();
        let mut snapshot_waiters = Vec::new();
        let mut snapshot_in_flight = false;
        let mut terminal_reason = None;
        let mut turn_stats = RuntimeTurnStats::default();
        let mut turn_started_at: Option<std::time::Instant> = None;
        let mut pending_local_context = Vec::new();
        let mut next_controller_id = 0u64;
        let mut goal: Option<GoalState> = None;
        let mut loop_state: Option<LoopState> = None;
        let mut pending_wakeup: Option<WakeupRequest> = None;
        let mut held_turn: Option<(u64, StopReason, Arc<SessionSnapshot>, RuntimeTurnStats)> = None;
        let mut ai_name_attempted = false;
        loop {
            tokio::select! {
                biased;
                management = owner_rx.recv() => match management {
                    Some(OwnerControl::SuspendCompaction { done }) => {
                        if !compaction_suspended {
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
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
                        let _ = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                        )
                        .await;
                        agent = noop_agent_handle();
                        agent_available = false;
                        observed_tokens = None;
                        let _ = done.send(());
                    }
                    Some(OwnerControl::Replace { agent: replacement, done }) => {
                        let resume_after_replace = !compaction_suspended;
                        if resume_after_replace {
                            generation = generation.wrapping_add(1);
                            event_generation.store(generation, Ordering::Release);
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
                        let _ = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                        )
                        .await;
                        agent = replacement;
                        agent_available = true;
                        observed_tokens = None;
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
                                &mut snapshot_waiters,
                                &runtime_event_tx,
                            );
                        }
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
                suggestion = session_name_rx.recv(), if native_protocol => {
                    let Some((name_generation, name)) = suggestion else { continue };
                    if name_generation == generation {
                        let _ = runtime_event_tx.send(
                            CodingRuntimeEvent::SessionNameSuggested { name },
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
                    let mut finish = false;
                    let mut continuation = None;
                    match outcome.result {
                        GoalResult::Met(verdict) => {
                            if let Some(state) = goal.as_mut() {
                                state.active = false;
                                state.last_reason = Some(verdict);
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            finish = true;
                        }
                        GoalResult::NotMet(verdict) => {
                            if let Some(state) = goal.as_mut() {
                                state.round = state.round.saturating_add(1);
                                state.evaluator_failures = 0;
                                state.last_reason = Some(verdict.clone());
                                continuation = Some(goal_continuation_message(&verdict, &state.condition));
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                        }
                        GoalResult::Error(error) => {
                            if let Some(state) = goal.as_mut() {
                                state.evaluator_failures = state.evaluator_failures.saturating_add(1);
                                state.last_reason = Some(format!("evaluator failed: {error}"));
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                if state.evaluator_failures >= MAX_EVAL_FAILURES {
                                    state.active = false;
                                    finish = true;
                                } else {
                                    continuation = Some(goal_continuation_message(&format!("evaluator error: {error}"), &state.condition));
                                }
                            }
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(format!("goal evaluator failed: {error}")));
                        }
                    }
                    if finish {
                        if let Some((turn_id, reason, snapshot, stats)) = held_turn.take() {
                            active_turn = None;
                            goal = None;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed { turn_id, reason, snapshot, stats }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        }
                    } else if let Some(text) = continuation {
                        if agent.commands.send(AgentCommand::SendMessage { text, images: vec![] }).is_ok() {
                            held_turn = None;
                            terminal_reason = None;
                            turn_stats = RuntimeTurnStats::default();
                        } else {
                            agent_available = false;
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("continuation dispatch failed".into());
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(
                                "goal stopped: continuation dispatch failed".into(),
                            ));
                            if let Some((turn_id, reason, snapshot, stats)) = held_turn.take() {
                                active_turn = None;
                                terminal_reason = None;
                                turn_stats = RuntimeTurnStats::default();
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                                    TurnCompletion::Completed { turn_id, reason, snapshot, stats },
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
                    let at_limit = loop_state.as_ref().is_some_and(|state| state.round >= state.max_rounds);
                    if at_limit {
                        if let Some(mut state) = loop_state.take() {
                            state.active = false;
                            state.last_reason = Some("round limit".into());
                            state.cancel.cancel();
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                        }
                        if let Some(runtime) = resources.as_ref() { runtime.loop_active.store(false, Ordering::Release); }
                        if let Some((turn_id, reason, snapshot, stats)) = held_turn.take() {
                            active_turn = None;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed { turn_id, reason, snapshot, stats }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        }
                    } else {
                        if let Some(state) = loop_state.as_mut() {
                            state.round = state.round.saturating_add(1);
                            state.last_reason = Some(wakeup.reason);
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                        }
                        if agent.commands.send(AgentCommand::SendMessage { text: wakeup.prompt, images: vec![] }).is_ok() {
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
                            if let Some((turn_id, reason, snapshot, stats)) = held_turn.take() {
                                active_turn = None;
                                terminal_reason = None;
                                turn_stats = RuntimeTurnStats::default();
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(
                                    TurnCompletion::Completed { turn_id, reason, snapshot, stats },
                                ));
                            }
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Failed), Ordering::Release);
                        }
                    }
                }
                control = controls.recv(), if controls_open => match control {
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
                        } else if agent.commands.send(AgentCommand::Compact { focus }).is_ok() {
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
                        if !native_protocol || request_generation != generation || !agent_available {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
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
                        if !pending_local_context.is_empty() {
                            let prefix = pending_local_context.drain(..).collect::<Vec<_>>().join("\n\n");
                            input.text = if input.text.is_empty() {
                                prefix
                            } else {
                                format!("{prefix}\n\n{}", input.text)
                            };
                        }
                        if agent
                            .commands
                            .send(AgentCommand::SendMessage {
                                text: input.text,
                                images: input.images,
                            })
                            .is_ok()
                        {
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
                        } else if !pending_requests.remove(&id) {
                            let _ = done.send(Err(RuntimeError::StaleRequest { id }));
                        } else if agent.commands.send(AgentCommand::Respond { id, value }).is_err() {
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
                    Some(CodingRuntimeControl::Snapshot {
                        generation: request_generation,
                        done,
                    }) => {
                        if !native_protocol || request_generation != generation || !agent_available {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                        } else {
                            snapshot_waiters.push(done);
                            if active_turn.is_none() && !snapshot_in_flight {
                                if agent.commands.send(AgentCommand::Snapshot).is_ok() {
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
                    Some(CodingRuntimeControl::Cancel {
                        generation: request_generation,
                        done,
                    }) => {
                        if !native_protocol || request_generation != generation || !agent_available {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                        } else if let Some((turn_id, _, snapshot, stats)) = held_turn.take() {
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("cancelled by user".into());
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
                            let _ = done.send(Ok(()));
                        } else {
                            if let Some(mut state) = goal.take() {
                                state.cancel.cancel();
                                state.active = false;
                                state.last_reason = Some("cancelled by user".into());
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
                            for id in pending_requests.iter().copied() {
                                let _ = agent.commands.send(AgentCommand::Respond {
                                    id,
                                    value: serde_json::Value::Null,
                                });
                            }
                            pending_requests.clear();
                            if agent.commands.send(AgentCommand::Cancel).is_ok() {
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
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::ModeChanged { mode });
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::ContextStats {
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
                        let used_tokens = observed_tokens
                            .unwrap_or_default()
                            .min(u32::MAX as usize) as u32;
                        let context_window = runtime.config.context_window;
                        let utilization = if context_window == 0 {
                            0.0
                        } else {
                            used_tokens as f32 / context_window as f32
                        };
                        let _ = done.send(Ok(RuntimeContextStats {
                            context_window,
                            used_tokens,
                            utilization,
                            model: runtime.config.model.clone(),
                            working_dir: runtime.config.working_dir.clone(),
                        }));
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
                        let refresh_routing = if let Some(config) = routing {
                            if next.subagent_fast_provider.is_none()
                                && next.subagent_capable_provider.is_none()
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

                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation: ReconfigureKind::Provider,
                        });
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
                        )
                        .await;
                        finish_stopped_native_turn(
                            &stop_report,
                            Some(&runtime),
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut snapshot_waiters,
                            &runtime_event_tx,
                        );
                        preserve_sessionless_snapshot(&mut runtime, &stop_report);

                        let old_config = runtime.config.clone();
                        match assemble(&mut runtime.parts, &next, candidate_provider) {
                            Ok(candidate) => {
                                if let Some(config) = refresh_routing {
                                    crate::provider_factory::refresh_subagent_tiers(
                                        runtime.provider_factory.clone(),
                                        &next,
                                        config.as_ref(),
                                    );
                                }
                                runtime.config = next;
                                agent = candidate.spawn();
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                agent_available = true;
                                observed_tokens = None;
                                snapshot_in_flight = false;
                                compaction_suspended = false;
                                let provider = runtime.config.provider_type.clone();
                                let model = runtime.config.model.clone();
                                resources = Some(runtime);
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Ready),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::ProviderChanged { provider, model },
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
                                match assemble_runtime_resources(&mut runtime) {
                                    Ok(rollback) => {
                                        agent = rollback;
                                        agent_available = true;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Ready),
                                            Ordering::Release,
                                        );
                                    }
                                    Err(rollback_error) => {
                                        agent = noop_agent_handle();
                                        agent_available = false;
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
                        let input = match resolve_reprepare_input(&runtime, target) {
                            Ok(input) => input,
                            Err(error) => {
                                resources = Some(runtime);
                                let _ = done.send(Err(error));
                                continue;
                            }
                        };
                        let operation = input.operation;
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
                        let candidate_parts = prepare_with_plugin_hook_source(
                            &input.config,
                            input.prepare.clone(),
                            runtime.plugin_hooks.as_ref(),
                        )
                        .await;
                        let mut candidate = match candidate_parts {
                            Ok(parts) => RuntimeResources {
                                config: input.config,
                                prepare: input.prepare,
                                provider_factory: runtime.provider_factory.clone(),
                                plugin_hooks: runtime.plugin_hooks.clone(),
                                parts,
                                wakeup_tx: runtime.wakeup_tx.clone(),
                                loop_active: Arc::clone(&runtime.loop_active),
                            },
                            Err(error) => {
                                controls.state.store(
                                    runtime_phase_state(generation, previous_phase),
                                    Ordering::Release,
                                );
                                resources = Some(runtime);
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                    error.to_string(),
                                )));
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
                            &mut snapshot_waiters,
                            &runtime_event_tx,
                        );
                        preserve_sessionless_snapshot(&mut runtime, &stop_report);
                        let candidate_result = assemble_runtime_resources(&mut candidate)
                            .map(|agent| (candidate, agent));

                        match candidate_result {
                            Ok((candidate, replacement)) => {
                                runtime = candidate;
                                agent = replacement;
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                agent_available = true;
                                observed_tokens = None;
                                snapshot_in_flight = false;
                                compaction_suspended = false;
                                if matches!(
                                    operation,
                                    ReconfigureKind::FreshSession
                                        | ReconfigureKind::ChangeDirectory
                                ) {
                                    ai_name_attempted = false;
                                }
                                let changed = session_changed(generation, &runtime);
                                let cwd = runtime.config.working_dir.clone();
                                resources = Some(runtime);
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::Ready),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::SessionChanged(changed.clone()),
                                );
                                if operation == ReconfigureKind::ChangeDirectory {
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::WorkingDirectoryChanged(cwd),
                                    );
                                }
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfigured { operation },
                                );
                                let _ = done.send(Ok(changed));
                            }
                            Err(candidate_error) => {
                                match assemble_runtime_resources(&mut runtime) {
                                    Ok(rollback) => {
                                        agent = rollback;
                                        agent_available = true;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Ready),
                                            Ordering::Release,
                                        );
                                    }
                                    Err(rollback_error) => {
                                        agent = noop_agent_handle();
                                        agent_available = false;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Failed),
                                            Ordering::Release,
                                        );
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::Agent(AgentEvent::Error {
                                                message: format!(
                                                    "reprepare rollback failed: {rollback_error}"
                                                ),
                                                http_status: None,
                                                code: None,
                                            }),
                                        );
                                    }
                                }
                                resources = Some(runtime);
                                let _ = done.send(Err(RuntimeError::ReconfigureFailed(
                                    candidate_error,
                                )));
                            }
                        }
                    }
                    Some(CodingRuntimeControl::ApplyUndo {
                        generation: request_generation,
                        original,
                        truncated,
                        restored_prompt,
                        target_n,
                        prompts_before,
                        done,
                    }) => {
                        if request_generation != generation || compaction_suspended || active_turn.is_some() {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        let Some(mut runtime) = resources.take() else {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        };
                        if let Err(error) = persist_runtime_snapshot(&mut runtime, &truncated) {
                            resources = Some(runtime);
                            let _ = done.send(Err(RuntimeError::ReconfigureFailed(error)));
                            continue;
                        }
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation: ReconfigureKind::Undo,
                        });
                        let _ = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
                        )
                        .await;
                        match assemble_runtime_resources(&mut runtime) {
                            Ok(replacement) => {
                                agent = replacement;
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                agent_available = true;
                                observed_tokens = None;
                                snapshot_in_flight = false;
                                let snapshot = Arc::new(truncated);
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
                                let restore_error = persist_runtime_snapshot(
                                    &mut runtime,
                                    original.as_ref(),
                                )
                                .err();
                                match assemble_runtime_resources(&mut runtime) {
                                    Ok(rollback) => {
                                        agent = rollback;
                                        agent_available = true;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Ready),
                                            Ordering::Release,
                                        );
                                    }
                                    Err(rollback_error) => {
                                        agent = noop_agent_handle();
                                        agent_available = false;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Failed),
                                            Ordering::Release,
                                        );
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::Agent(AgentEvent::Error {
                                                message: format!(
                                                    "undo rollback failed: {rollback_error}"
                                                ),
                                                http_status: None,
                                                code: None,
                                            }),
                                        );
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
                        controls.state.store(
                            runtime_phase_state(generation, RuntimePhase::Reconfiguring),
                            Ordering::Release,
                        );
                        let _ = runtime_event_tx.send(CodingRuntimeEvent::Reconfiguring {
                            operation: ReconfigureKind::RestoreSession,
                        });
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
                        )
                        .await;
                        finish_stopped_native_turn(
                            &stop_report,
                            Some(&runtime),
                            &mut active_turn,
                            &mut terminal_reason,
                            &mut turn_stats,
                            &mut snapshot_waiters,
                            &runtime_event_tx,
                        );
                        let original = current_runtime_snapshot(&runtime)
                            .or_else(|| stop_report.snapshot.clone());
                        preserve_sessionless_snapshot(&mut runtime, &stop_report);
                        let persisted = persist_runtime_snapshot(&mut runtime, &snapshot);
                        let candidate = persisted
                            .as_ref()
                            .map_err(|error| error.clone())
                            .and_then(|_| assemble_runtime_resources(&mut runtime));
                        match candidate {
                            Ok(replacement) => {
                                agent = replacement;
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
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
                                let _ = runtime_event_tx.send(
                                    CodingRuntimeEvent::Reconfigured {
                                        operation: ReconfigureKind::RestoreSession,
                                    },
                                );
                                let _ = done.send(Ok(changed));
                            }
                            Err(candidate_error) => {
                                let restore_error = original
                                    .as_ref()
                                    .and_then(|original| {
                                        persist_runtime_snapshot(&mut runtime, original).err()
                                    });
                                match assemble_runtime_resources(&mut runtime) {
                                    Ok(rollback) => {
                                        agent = rollback;
                                        agent_available = true;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Ready),
                                            Ordering::Release,
                                        );
                                    }
                                    Err(rollback_error) => {
                                        agent = noop_agent_handle();
                                        agent_available = false;
                                        controls.state.store(
                                            runtime_phase_state(generation, RuntimePhase::Failed),
                                            Ordering::Release,
                                        );
                                        let _ = runtime_event_tx.send(
                                            CodingRuntimeEvent::Agent(AgentEvent::Error {
                                                message: format!(
                                                    "conversation restore rollback failed: {rollback_error}"
                                                ),
                                                http_status: None,
                                                code: None,
                                            }),
                                        );
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
                    Some(CodingRuntimeControl::StartGoal { generation: request_generation, condition, done }) => {
                        if !native_protocol || request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        if resources.is_none() {
                            let _ = done.send(Err(RuntimeError::Unavailable));
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
                            let next = GoalState::new(
                                next_controller_id,
                                condition,
                                runtime.config.goal_max_rounds,
                                runtime.config.goal_max_duration_secs,
                            );
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(next.progress()));
                            goal = Some(next);
                            let _ = done.send(Ok(()));
                        }
                    }
                    Some(CodingRuntimeControl::StopGoal { generation: request_generation, done }) => {
                        if !native_protocol || request_generation != generation {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                            continue;
                        }
                        if let Some(mut current) = goal.take() {
                            current.cancel.cancel();
                            current.active = false;
                            current.last_reason = Some("cleared by user".into());
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(current.progress()));
                        }
                        if active_turn.is_some() { let _ = agent.commands.send(AgentCommand::Cancel); }
                        if let Some((turn_id, _, snapshot, stats)) = held_turn.take() {
                            active_turn = None;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                                turn_id, reason: StopReason::Cancelled, snapshot, stats,
                            }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        }
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::StartLoop { generation: request_generation, prompt, done }) => {
                        if !native_protocol || request_generation != generation || compaction_suspended {
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
                        }
                        if resources.is_none() {
                            let _ = done.send(Err(RuntimeError::Unavailable));
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
                            let next = LoopState::new(next_controller_id, prompt, runtime.config.loop_max_rounds);
                            runtime.loop_active.store(true, Ordering::Release);
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(next.progress()));
                            loop_state = Some(next);
                            let _ = done.send(Ok(()));
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
                        if active_turn.is_some() { let _ = agent.commands.send(AgentCommand::Cancel); }
                        if let Some((turn_id, _, snapshot, stats)) = held_turn.take() {
                            active_turn = None;
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                                turn_id, reason: StopReason::Cancelled, snapshot, stats,
                            }));
                            controls.state.store(runtime_phase_state(generation, RuntimePhase::Ready), Ordering::Release);
                        }
                        let _ = done.send(Ok(()));
                    }
                    Some(CodingRuntimeControl::Shutdown { generation: request_generation }) => {
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
                                &mut snapshot_waiters,
                                &runtime_event_tx,
                            );
                        }
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
                        let _ = agent.commands.send(command);
                    }
                    None => break,
                },
                event = agent.events.recv() => match event {
                    Some(event) => match handle_compaction_event(
                        event,
                        &mut compactions,
                        &mut observed_tokens,
                        &runtime_event_tx,
                    ) {
                        Some(event) if native_protocol => match event {
                            AgentEvent::Usage(meta) => {
                                observed_tokens = Some(meta.used_tokens as usize);
                                turn_stats.turn_count = turn_stats.turn_count.saturating_add(1);
                                turn_stats.last_usage = Some(meta.clone());
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                    AgentEvent::Usage(meta),
                                ));
                            }
                            AgentEvent::Request { id, kind, payload } => {
                                pending_requests.insert(id);
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
                            AgentEvent::TurnComplete { reason } => {
                                turn_stats.duration = turn_started_at
                                    .take()
                                    .map(|started| started.elapsed())
                                    .unwrap_or_default();
                                terminal_reason = Some(reason);
                                if !snapshot_in_flight {
                                    if agent.commands.send(AgentCommand::Snapshot).is_ok() {
                                        snapshot_in_flight = true;
                                    } else {
                                        let turn_id = active_turn.take().unwrap_or_default();
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
                                                    reason,
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
                                let snapshot = Arc::new(snapshot);
                                for waiter in snapshot_waiters.drain(..) {
                                    let _ = waiter.send(Ok(snapshot.clone()));
                                }
                                if let Some(reason) = terminal_reason.take() {
                                    pending_requests.clear();
                                    let stats = std::mem::take(&mut turn_stats);
                                    let turn_id = active_turn.unwrap_or_default();
                                    if reason != StopReason::Cancelled && !ai_name_attempted {
                                        if let Some(conversation) =
                                            crate::session_title::first_exchange_text(&snapshot.messages)
                                        {
                                            let enabled = resources
                                                .as_ref()
                                                .and_then(|runtime| {
                                                    runtime.config.subagent_config.as_deref()
                                                })
                                                .map(
                                                    atomcode_config::config::ai_session_naming_enabled,
                                                )
                                                .unwrap_or_else(|| {
                                                    atomcode_config::config::Config::load(
                                                        &atomcode_config::config::Config::default_path(),
                                                    )
                                                    .map(|config| {
                                                        atomcode_config::config::ai_session_naming_enabled(
                                                            &config,
                                                        )
                                                    })
                                                    .unwrap_or(false)
                                                });
                                            if enabled {
                                                ai_name_attempted = true;
                                                let provider = resources.as_ref().and_then(|runtime| {
                                                    let session_id = runtime.parts.session.as_ref()
                                                        .map(|binding| binding.id.as_str());
                                                    runtime.provider_factory
                                                        .build(&runtime.config, session_id)
                                                        .ok()
                                                });
                                                if let Some(provider) = provider {
                                                    let tx = session_name_tx.clone();
                                                    let name_generation = generation;
                                                    tokio::spawn(async move {
                                                        if let Some(name) =
                                                            crate::session_title::generate_session_title(
                                                                provider,
                                                                conversation,
                                                            )
                                                            .await
                                                        {
                                                            let _ = tx.send((name_generation, name));
                                                        }
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    if let Some(state) = goal.as_mut().filter(|state| state.active) {
                                        if let Some(meta) = stats.last_usage.as_ref() {
                                            state.tokens_used = state.tokens_used.saturating_add(
                                                (meta.tokens.prompt + meta.tokens.completion) as u64,
                                            );
                                        }
                                        let stop_reason = state.cap_reached();
                                        let evaluate = matches!(reason, StopReason::Stopped | StopReason::MaxContinuations);
                                        let recoverable = matches!(reason, StopReason::Timeout | StopReason::ProviderError | StopReason::MaxRounds);
                                        if let Some(why) = stop_reason {
                                            state.active = false;
                                            state.last_reason = Some(format!("stopped: {why}"));
                                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                            let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(format!("goal stopped: {why} — goal not met; run /goal again to continue")));
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
                                                runtime.provider_factory.build(&runtime.config, session_id).ok()
                                            });
                                            held_turn = Some((turn_id, reason, snapshot.clone(), stats));
                                            let tx = goal_eval_tx.clone();
                                            if let Some(provider) = provider {
                                                tokio::spawn(async move {
                                                    let outcome = evaluate_goal(generation, controller_id, provider, condition, summary, cancel).await;
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
                                                if agent.commands.send(AgentCommand::SendMessage { text, images: vec![] }).is_ok() {
                                                    let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                                    continue;
                                                }
                                                agent_available = false;
                                                state.cancel.cancel();
                                                state.active = false;
                                                state.last_reason = Some("continuation dispatch failed".into());
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
                                                            reason,
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
                                                state.active = false;
                                                state.last_reason = Some("stopped: too many failed rounds".into());
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning("goal stopped: too many failed rounds".into()));
                                            }
                                        } else {
                                            state.active = false;
                                            state.last_reason = Some(format!("ended: {reason:?}"));
                                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                        }
                                        goal = None;
                                    } else if let Some(state) = loop_state.as_mut().filter(|state| state.active) {
                                        if reason == StopReason::Stopped {
                                            if let Some(wakeup) = pending_wakeup.take() {
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
                                            state.active = false;
                                            state.last_reason = Some(format!("ended: {reason:?}"));
                                        }
                                        state.cancel.cancel();
                                        let _ = runtime_event_tx.send(CodingRuntimeEvent::LoopChanged(state.progress()));
                                        if let Some(runtime) = resources.as_ref() { runtime.loop_active.store(false, Ordering::Release); }
                                        loop_state = None;
                                    }
                                    active_turn = None;
                                    let _ = runtime_event_tx.send(
                                        CodingRuntimeEvent::TurnFinished(
                                            TurnCompletion::Completed {
                                                turn_id,
                                                reason,
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
                            AgentEvent::TurnStarted => {
                                turn_started_at = Some(std::time::Instant::now());
                                controls.state.store(
                                    runtime_phase_state(generation, RuntimePhase::InTurn),
                                    Ordering::Release,
                                );
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(
                                    AgentEvent::TurnStarted,
                                ));
                            }
                            event @ AgentEvent::ToolStarted { .. } => {
                                turn_stats.tool_call_count =
                                    turn_stats.tool_call_count.saturating_add(1);
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::Agent(event));
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
                    },
                    None => {
                        if native_protocol {
                            let message = "kernel event stream closed before snapshot terminal";
                            let unavailable = RuntimeError::SnapshotUnavailable(message.into());
                            for waiter in snapshot_waiters.drain(..) {
                                let _ = waiter.send(Err(unavailable.clone()));
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
            forced_shutdown = stop_current_agent(
                &mut agent,
                &mut compactions,
                &mut observed_tokens,
                &runtime_event_tx,
                CompactionInterruption::RuntimeShutdown,
            )
            .await
            .forced;
        }
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
        match control {
            CodingRuntimeControl::Compact { focus, .. } => emit_compaction_interrupted(
                runtime_event_tx,
                CompactTrigger::Manual { focus },
                reason,
            ),
            CodingRuntimeControl::Shutdown { .. } => {}
            CodingRuntimeControl::Submit { done, .. } => {
                let _ = done.send(Err(RuntimeError::Unavailable));
            }
            CodingRuntimeControl::Respond { done, .. }
            | CodingRuntimeControl::Cancel { done, .. }
            | CodingRuntimeControl::SetMode { done, .. }
            | CodingRuntimeControl::QueueLocalContext { done, .. } => {
                let _ = done.send(Err(RuntimeError::Unavailable));
            }
            CodingRuntimeControl::Snapshot { done, .. } => {
                let _ = done.send(Err(RuntimeError::Unavailable));
            }
            CodingRuntimeControl::ContextStats { done, .. } => {
                let _ = done.send(Err(RuntimeError::Unavailable));
            }
            CodingRuntimeControl::ReassembleProvider { done, .. } => {
                let _ = done.send(Err(RuntimeError::Unavailable));
            }
            CodingRuntimeControl::Reprepare { done, .. } => {
                let _ = done.send(Err(RuntimeError::Unavailable));
            }
            CodingRuntimeControl::ApplyUndo { done, .. } => {
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
}

fn fail_close_pending_requests(
    agent: &AgentHandle,
    pending_requests: &mut BTreeSet<RequestId>,
    cancel_turn: bool,
) {
    for id in pending_requests.iter().copied() {
        let _ = agent.commands.send(AgentCommand::Respond {
            id,
            value: serde_json::Value::Null,
        });
    }
    pending_requests.clear();
    if cancel_turn {
        let _ = agent.commands.send(AgentCommand::Cancel);
    }
}

fn resolve_reprepare_input(
    runtime: &RuntimeResources,
    target: ReprepareTarget,
) -> Result<ReprepareInput, RuntimeError> {
    match target {
        ReprepareTarget::Exact(input) => Ok(input),
        ReprepareTarget::Reload => {
            let mut prepare = runtime.prepare.clone();
            prepare.session = match runtime.parts.session.as_ref() {
                Some(binding) => crate::SessionMode::Resume(binding.id.clone()),
                None => crate::SessionMode::Disabled,
            };
            Ok(ReprepareInput {
                config: runtime.config.clone(),
                prepare,
                operation: ReconfigureKind::Reprepare,
            })
        }
        ReprepareTarget::Fresh => {
            let mut prepare = runtime.prepare.clone();
            prepare.session = crate::SessionMode::Fresh;
            Ok(ReprepareInput {
                config: runtime.config.clone(),
                prepare,
                operation: ReconfigureKind::FreshSession,
            })
        }
        ReprepareTarget::Resume(id) => {
            let mut prepare = runtime.prepare.clone();
            prepare.session = crate::SessionMode::Resume(id);
            Ok(ReprepareInput {
                config: runtime.config.clone(),
                prepare,
                operation: ReconfigureKind::ResumeSession,
            })
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
            let mut config = runtime.config.clone();
            config.working_dir = canonical;
            let mut prepare = runtime.prepare.clone();
            prepare.session = crate::SessionMode::Fresh;
            Ok(ReprepareInput {
                config,
                prepare,
                operation: ReconfigureKind::ChangeDirectory,
            })
        }
    }
}

fn assemble_runtime_resources(runtime: &mut RuntimeResources) -> Result<AgentHandle, String> {
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
    assemble(&mut runtime.parts, &runtime.config, provider)
        .map(|agent| agent.spawn())
        .map_err(|error| error.to_string())
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

fn persist_runtime_snapshot(
    runtime: &mut RuntimeResources,
    snapshot: &SessionSnapshot,
) -> Result<(), String> {
    if let Some(binding) = runtime.parts.session.as_ref() {
        binding
            .manager
            .save_snapshot(&binding.id, snapshot)
            .map_err(|error| error.to_string())
    } else {
        runtime.parts.set_runtime_resume(snapshot.clone());
        Ok(())
    }
}

fn current_runtime_snapshot(runtime: &RuntimeResources) -> Option<SessionSnapshot> {
    let binding = runtime.parts.session.as_ref()?;
    binding.manager.load_snapshot(&binding.id).ok()
}

struct RuntimeUndoPlan {
    truncated: Vec<Message>,
    restored_prompt: String,
    target_n: usize,
    prompts_before: usize,
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
}

async fn stop_current_agent(
    agent: &mut AgentHandle,
    compactions: &mut CompactionTracker,
    observed_tokens: &mut Option<usize>,
    runtime_event_tx: &RuntimeEventEmitter,
    reason: CompactionInterruption,
) -> StopReport {
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
                    match handle_compaction_event(
                        event,
                        compactions,
                        observed_tokens,
                        runtime_event_tx,
                    ) {
                        Some(AgentEvent::Usage(meta)) => {
                            *observed_tokens = Some(meta.used_tokens as usize);
                        }
                        Some(AgentEvent::TurnComplete { reason }) => {
                            report.reason = Some(reason);
                        }
                        Some(AgentEvent::Snapshot { snapshot }) => {
                            report.snapshot = Some(snapshot);
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
        match handle_compaction_event(event, compactions, observed_tokens, runtime_event_tx) {
            Some(AgentEvent::Usage(meta)) => {
                *observed_tokens = Some(meta.used_tokens as usize);
            }
            Some(AgentEvent::TurnComplete { reason }) => report.reason = Some(reason),
            Some(AgentEvent::Snapshot { snapshot }) => report.snapshot = Some(snapshot),
            _ => {}
        }
    }
    compactions.interrupt_all(reason, runtime_event_tx);
    report
}

fn finish_stopped_native_turn(
    report: &StopReport,
    resources: Option<&RuntimeResources>,
    active_turn: &mut Option<u64>,
    terminal_reason: &mut Option<StopReason>,
    turn_stats: &mut RuntimeTurnStats,
    snapshot_waiters: &mut Vec<oneshot::Sender<Result<Arc<SessionSnapshot>, RuntimeError>>>,
    runtime_event_tx: &RuntimeEventEmitter,
) {
    let snapshot = report.snapshot.clone().or_else(|| {
        let binding = resources?.parts.session.as_ref()?;
        binding.manager.load_snapshot(&binding.id).ok()
    });
    if let Some(snapshot) = snapshot {
        let snapshot = Arc::new(snapshot);
        for waiter in snapshot_waiters.drain(..) {
            let _ = waiter.send(Ok(snapshot.clone()));
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
        current.active = false;
        current.last_reason = Some(detail.into());
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TestProviderFactory {
        fail: bool,
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
        host_fast_cell: std::sync::Mutex<Option<Arc<crate::TierProvider>>>,
        fail_model: std::sync::Mutex<Option<String>>,
    }

    #[derive(Default)]
    struct CountingProviderFactory {
        builds: std::sync::atomic::AtomicUsize,
    }

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

    impl CodingProviderFactory for TierRecordingFactory {
        fn build(
            &self,
            config: &CodingAgentConfig,
            _session_id: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            self.models.lock().unwrap().push(config.model.clone());
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
            user_agent: None,
            context_window: 64_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: Some(rank),
        }
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
                session: crate::SessionMode::Disabled,
                skill_dirs: Some(Vec::new()),
                mcp: false,
                memory: false,
                web: false,
                review: false,
                rate_limit_source: None,
            },
            provider_factory: Arc::new(TestProviderFactory {
                fail: fail_provider,
            }),
            plugin_hooks: Arc::new(crate::StaticPluginHookSource::default()),
        }
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

        runtime.handle.start_loop("watch CI").await.unwrap();
        assert!(matches!(
            next_native_event(&mut runtime).await,
            CodingRuntimeEvent::GoalChanged(GoalProgress { active: false, .. })
        ));
        assert!(matches!(
            next_native_event(&mut runtime).await,
            CodingRuntimeEvent::LoopChanged(LoopProgress { active: true, label, .. })
                if label == "watch CI"
        ));

        runtime.handle.stop_loop().await.unwrap();
        assert!(matches!(
            next_native_event(&mut runtime).await,
            CodingRuntimeEvent::LoopChanged(LoopProgress { active: false, .. })
        ));
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

    fn shutdown_reporting_agent(report_started: bool, report_compacted: bool) -> AgentHandle {
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut pending = None;
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
                                    snapshot: None,
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
    async fn native_start_returns_provider_error_without_degraded_handle() {
        assert!(matches!(
            CodingRuntime::start(native_start(true)).await,
            Err(RuntimeStartError::Provider(crate::ProviderBuildError::Adapter(message)))
                if message == "expected failure"
        ));
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

        kernel_events.send(AgentEvent::TurnStarted).unwrap();
        kernel_events
            .send(AgentEvent::TurnComplete {
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Agent(AgentEvent::TurnStarted))
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
    async fn goal_continuation_send_failure_finishes_the_held_snapshot() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
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
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress { active: true, .. }))
        ));
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
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
        .expect("continuation send failure lost the held turn terminal");

        assert!(matches!(
            terminal,
            TurnCompletion::Completed {
                turn_id: 1,
                reason: StopReason::Stopped,
                snapshot,
                ..
            } if snapshot.as_ref() == &expected
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Failed);
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
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        let _ = kernel_commands.recv().await;
        kernel_events
            .send(AgentEvent::TurnComplete {
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
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        } = native_start(false);
        let parts = prepare_with_plugin_hook_source(
            &config,
            prepare.clone(),
            plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            wakeup_tx: wakeup_tx.clone(),
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

        handle.start_loop("watch CI").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress { active: true, .. }))
        ));
        handle.submit(UserInput::from("initial turn")).await.unwrap();
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
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress { active: true, .. }))
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(kernel_commands.recv().await, Some(AgentCommand::Snapshot)));
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
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress { active: false, .. }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                turn_id: 1,
                reason: StopReason::Cancelled,
                ..
            }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress { active: true, .. }))
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);

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
        let parts = prepare_with_plugin_hook_source(
            &config,
            prepare.clone(),
            plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory: Arc::new(PendingProviderFactory),
            plugin_hooks,
            parts,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress { active: true, .. }))
        ));
        handle.submit(UserInput::from("initial turn")).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(kernel_commands.recv().await, Some(AgentCommand::Snapshot)));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("persisted")]),
            })
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match handle.start_loop("watch CI").await {
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
            Some(CodingRuntimeEvent::GoalChanged(GoalProgress { active: false, .. }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                turn_id: 1,
                reason: StopReason::Cancelled,
                ..
            }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress { active: true, .. }))
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reprepare_preflight_failure_does_not_stop_active_agent() {
        let (agent, mut kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
        } = native_start(false);
        let parts = prepare_with_plugin_hook_source(
            &config,
            prepare.clone(),
            plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

        handle.submit(UserInput::from("still running")).await.unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));

        assert!(matches!(
            handle.resume_session("missing-session-id").await,
            Err(RuntimeError::ReconfigureFailed(_))
        ));
        assert_eq!(handle.status().phase, RuntimePhase::InTurn);
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Reconfiguring {
                operation: ReconfigureKind::ResumeSession
            })
        ));
        assert!(kernel_commands.try_recv().is_err());

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fresh_session_clears_held_loop_without_duplicate_terminal() {
        let (agent, mut kernel_commands, kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, mut runtime_events) = mpsc::unbounded_channel();
        let (wakeup_tx, wakeup_rx) = mpsc::unbounded_channel();
        let CodingRuntimeStart {
            agent: config,
            prepare,
            provider_factory,
            plugin_hooks,
        } = native_start(false);
        let parts = prepare_with_plugin_hook_source(
            &config,
            prepare.clone(),
            plugin_hooks.as_ref(),
        )
        .await
        .unwrap();
        let resources = RuntimeResources {
            config,
            prepare,
            provider_factory,
            plugin_hooks,
            parts,
            wakeup_tx: wakeup_tx.clone(),
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

        handle.start_loop("watch CI").await.unwrap();
        let _ = runtime_events.recv().await;
        handle.submit(UserInput::from("initial turn")).await.unwrap();
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
                reason: StopReason::Stopped,
            })
            .unwrap();
        assert!(matches!(kernel_commands.recv().await, Some(AgentCommand::Snapshot)));
        kernel_events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![Message::user("persisted")]),
            })
            .unwrap();

        let changed = handle.fresh_session().await.unwrap();
        assert_eq!(changed.generation, RuntimeGeneration(1));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Reconfiguring {
                operation: ReconfigureKind::FreshSession
            })
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress { active: false, .. }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                turn_id: 1,
                reason: StopReason::Cancelled,
                ..
            }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::SessionChanged(SessionChanged {
                generation: RuntimeGeneration(1),
                ..
            }))
        ));
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::Reconfigured {
                operation: ReconfigureKind::FreshSession
            })
        ));
        assert_eq!(handle.status().phase, RuntimePhase::Ready);

        handle.stop_loop().await.unwrap();
        assert!(runtime_events.try_recv().is_err());
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
    async fn provider_reassemble_commits_one_generation_and_tags_events_at_emit_time() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();
        let mut next = CodingAgentConfig::new(
            "next-key",
            "https://next.example.test/v1",
            "next-model",
            ".",
        );
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
            CodingRuntimeEvent::ProviderChanged { ref model, .. } if model == "next-model"
        ));
        assert_eq!(third.generation, 1);
        assert!(matches!(
            third.event,
            CodingRuntimeEvent::Reconfigured {
                operation: ReconfigureKind::Provider
            }
        ));
        assert!(first.sequence < second.sequence && second.sequence < third.sequence);
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn missing_resume_rolls_back_without_silent_fresh_session() {
        let mut runtime = CodingRuntime::start(native_start(false)).await.unwrap();

        let result = runtime.handle.resume_session("missing-session-id").await;

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
    async fn fresh_session_is_runtime_owned_and_returns_new_identity() {
        let runtime = CodingRuntime::start(native_start(false)).await.unwrap();

        let changed = runtime.handle.fresh_session().await.unwrap();

        assert_eq!(changed.generation, RuntimeGeneration(1));
        assert!(changed.session_id.as_ref().is_some_and(|id| !id.is_empty()));
        assert_eq!(runtime.handle.status().generation, 1);
        runtime.handle.shutdown().await.unwrap();
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
}
