//! Stable driver control plane and kernel-agent owner for a coding runtime.
//!
//! The runtime owns the replaceable kernel [`AgentHandle`] so native controls and
//! events never need to traverse a legacy driver adapter.

use std::collections::{BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use atomcode_capabilities::session::snapshot::SnapshotPersistenceStatus;
use atomcode_capabilities::session::{
    DisplayAnchor, PresentationEntry, RewindPoint, RewindTransactionReceipt, SessionLease,
    SessionStoreError, TurnStat,
};
#[cfg(test)]
use atomcode_capabilities::session::{PresentationFile, SessionMeta, StorageOwner};
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
    GoalResult, GoalState, GoalTerminal, LoopProgress, LoopState, ScheduleWakeupTool,
    WakeupRequest, MAX_UNPRODUCTIVE,
};
use crate::parts::prepare_with_plugin_hook_source_reusing_lease;
#[cfg(test)]
use crate::prepare_with_plugin_hook_source;
use crate::{assemble, CodingAgentConfig, CodingProviderFactory, PluginHookSource, PrepareOptions};

/// Runtime facts emitted by the coding engine without depending on the legacy
/// `atomcode-core` driver protocol.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum CodingRuntimeEvent {
    /// A kernel observation that is not owned as a runtime terminal/request.
    Agent(AgentEvent),
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
    ProviderUnavailable {
        reason: ProviderUnavailableReason,
        forced: bool,
    },
    SessionNameSuggested {
        name: String,
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

#[derive(Clone, Debug, PartialEq)]
pub struct McpToolsSnapshot {
    pub generation: RuntimeGeneration,
    pub server: String,
    pub status: Option<atomcode_capabilities::mcp::ServerStatus>,
    pub tools: Vec<String>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindCatalog {
    pub generation: RuntimeGeneration,
    pub revision: u64,
    pub points: Vec<RewindPoint>,
    pub code_unavailable: Option<String>,
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
    SessionInUse { id: String },
    StaleRequest { id: RequestId },
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
            Self::SessionInUse { id } => {
                write!(f, "session {id:?} is already in use by another runtime")
            }
            Self::StaleRequest { id } => write!(f, "runtime request {id} is stale"),
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
    /// `active_model` is the runtime's resolved main-turn model name (honours
    /// a `--provider` / `/model` selection), used to decide vision support —
    /// authoritative, unlike re-reading a config default. `session_id` is the
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
        active_model: String,
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
    wakeup_tx: mpsc::UnboundedSender<WakeupRequest>,
    loop_active: Arc<std::sync::atomic::AtomicBool>,
    image_preprocessor: Option<Arc<dyn ImagePreprocessor>>,
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

    /// Remove every MCP tool from the model-facing catalog without reading
    /// mutable config, trust, or auth state. Security-reducing mutations must
    /// await this terminal before changing those inputs.
    pub async fn withdraw_mcp_tools(&self) -> Result<(), RuntimeError> {
        let state = self.state.load(Ordering::Acquire);
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::WithdrawMcpTools {
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

    pub async fn reprepare(&self, input: ReprepareInput) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Exact(input)).await
    }

    pub async fn fresh_session(&self) -> Result<SessionChanged, RuntimeError> {
        self.reprepare_target(ReprepareTarget::Fresh).await
    }

    pub async fn reload_capabilities(&self) -> Result<SessionChanged, RuntimeError> {
        self.reload_capabilities_with_plugin_skills(None).await
    }

    /// Reload the capability graph, optionally replacing the plugin skill
    /// directories captured at runtime startup. Drivers call this after plugin
    /// install/update/uninstall so the replacement generation sees current
    /// disk state rather than the stale startup snapshot.
    pub async fn reload_capabilities_with_plugin_skills(
        &self,
        plugin_skill_dirs: Option<Vec<(std::path::PathBuf, String)>>,
    ) -> Result<SessionChanged, RuntimeError> {
        self.withdraw_mcp_tools().await?;
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
        self.apply_undo(generation, original.revision, original.undo_snapshot, undo)
            .await
    }

    async fn apply_undo(
        &self,
        generation: u64,
        expected_revision: u64,
        original: Arc<SessionSnapshot>,
        undo: SnapshotUndoResult,
    ) -> Result<UndoResult, RuntimeError> {
        let (done, result) = oneshot::channel();
        self.tx
            .send(CodingRuntimeControl::ApplyUndo {
                generation,
                expected_revision,
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
                return Err(RuntimeError::CodeRewindUnavailable(reason));
            }
        }
        let original = self.snapshot_with_revision().await?;
        if original.revision != catalog.revision {
            return Err(RuntimeError::Busy);
        }
        let undo = if scope.restores_conversation() {
            Some(undo_snapshot_to_prompt(
                &original.undo_snapshot,
                Some(point.prompt_number),
            )?)
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
        let session_id = parts.session.as_ref().map(|binding| binding.id.as_str());
        let session = parts.session.as_ref().map(|binding| RuntimeSessionInfo {
            id: binding.id.clone(),
            resumed: binding.resume.is_some(),
        });
        let (kernel_agent, unavailable_reason) = match bootstrap {
            ProviderBootstrap::Unavailable(reason) => (None, Some(reason)),
            ProviderBootstrap::Required | ProviderBootstrap::RecoverAuthentication => {
                match provider_factory.build(&agent, session_id) {
                    Ok(provider) => (
                        Some(
                            assemble(&mut parts, &agent, provider)
                                .map_err(RuntimeStartError::Assemble)?
                                .spawn(),
                        ),
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
                    Err(crate::ProviderBuildError::SourceBuildGatewayUnsupported { .. })
                        if bootstrap == ProviderBootstrap::RecoverAuthentication =>
                    {
                        (None, Some(ProviderUnavailableReason::UnsupportedBuild))
                    }
                    Err(error) => return Err(RuntimeStartError::Provider(error)),
                }
            }
        };
        parts
            .publish_staged_session()
            .map_err(runtime_start_prepare_error)?;

        let (handle, controls) = coding_runtime_control_channel();
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
        done: oneshot::Sender<Result<RuntimeSnapshotReceipt, RuntimeError>>,
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
    WithdrawMcpTools {
        generation: u64,
        done: oneshot::Sender<Result<(), RuntimeError>>,
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
    ApplyUndo {
        generation: u64,
        expected_revision: u64,
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
    Exact(ReprepareInput),
    Reload {
        plugin_skill_dirs: Option<Vec<(std::path::PathBuf, String)>>,
    },
    Fresh,
    Resume(String),
    ResumeWithLease {
        id: String,
        working_dir: std::path::PathBuf,
        lease: SessionLease,
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
    (
        CodingRuntimeHandle {
            tx,
            state: Arc::clone(&state),
            provider_unavailable_reason: Arc::clone(&provider_unavailable_reason),
            terminal,
        },
        CodingRuntimeControlReceiver {
            rx,
            state,
            provider_unavailable_reason,
            terminal_tx,
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
    match phase {
        RuntimePhase::Ready | RuntimePhase::InTurn | RuntimePhase::WaitingApproval => true,
        RuntimePhase::AwaitingProvider | RuntimePhase::Failed => matches!(
            command,
            DriverCommand::ReloadProvider(_)
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
    let (session_name_tx, mut session_name_rx) = mpsc::unbounded_channel::<(u64, String)>();
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
        let mut pending_requests = BTreeSet::new();
        let mut snapshot_waiters: Vec<RuntimeSnapshotWaiter> = Vec::new();
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
        let mut persistence_failure = None;
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
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
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
                    let mut finish_reason = None;
                    let mut continuation = None;
                    match outcome.result {
                        GoalResult::Met(verdict) => {
                            if let Some(state) = goal.as_mut() {
                                state.finish(GoalTerminal::Met, verdict);
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
                            finish_reason = Some(StopReason::Stopped);
                        }
                        GoalResult::NotMet(verdict) => {
                            if let Some(state) = goal.as_mut() {
                                state.round = state.round.saturating_add(1);
                                state.last_reason = Some(verdict.clone());
                                continuation = Some(goal_continuation_message(&verdict, &state.condition));
                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                            }
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
                                let active_model = resources
                                    .as_ref()
                                    .map(|r| r.config.model.clone())
                                    .unwrap_or_default();
                                let session_id = resources
                                    .as_ref()
                                    .and_then(|r| r.parts.session.as_ref())
                                    .map(|b| b.id.clone());
                                let (new_input, notice) = pp
                                    .preprocess(
                                        std::mem::take(&mut input.text),
                                        std::mem::take(&mut input.images),
                                        active_model,
                                        session_id,
                                    )
                                    .await;
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
                        if send_agent_command(
                            &agent,
                            AgentCommand::SendMessage {
                                text: input.text,
                                images: input.images,
                            },
                        )
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
                        } else if !send_agent_command(&agent, AgentCommand::Respond { id, value }) {
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
                                code_unavailable: Some(
                                    "rewind requires a persistent session".into(),
                                ),
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
                            points: hook.rewind_points(),
                            code_unavailable: hook.code_rewind_unavailable(),
                        }));
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
                        if !native_protocol || request_generation != generation || !agent_available {
                            let _ = done.send(Err(RuntimeError::Unavailable));
                        } else if let Some((turn_id, _, snapshot, stats)) = held_turn.take() {
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
                            for id in pending_requests.iter().copied() {
                                let _ = send_agent_command(&agent, AgentCommand::Respond {
                                    id,
                                    value: serde_json::Value::Null,
                                });
                            }
                            pending_requests.clear();
                            if send_agent_command(&agent, AgentCommand::Cancel) {
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
                        let status = runtime
                            .parts
                            .mcp_statuses()
                            .await
                            .into_iter()
                            .find_map(|(name, status)| (name == server).then_some(status));
                        let _ = done.send(Ok(McpToolsSnapshot {
                            generation: RuntimeGeneration(generation),
                            server,
                            status,
                            tools,
                        }));
                    }
                    Some(CodingRuntimeControl::WithdrawMcpTools {
                        generation: request_generation,
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
                        let had_active_agent = agent_available;
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
                                agent = Some(candidate.spawn());
                                generation = generation.wrapping_add(1);
                                event_generation.store(generation, Ordering::Release);
                                agent_available = true;
                                provider_unavailable_reason = None;
                                controls.provider_unavailable_reason.store(0, Ordering::Release);
                                observed_tokens = None;
                                snapshot_in_flight = false;
                                compaction_suspended = false;
                                let provider = runtime.config.provider_name.clone();
                                let model = runtime.config.model.clone();
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
                                match had_active_agent
                                    .then(|| assemble_runtime_resources(&mut runtime))
                                    .transpose()
                                {
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
                            let _ = done.send(Ok(RuntimeGeneration(generation)));
                            continue;
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
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
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
                        let withdraws_mcp = matches!(&target, ReprepareTarget::Reload { .. });
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
                        let Some((input, prepared_lease)) = resolved else {
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
                        if active_turn.is_some() && reuses_current_session {
                            resources = Some(runtime);
                            let _ = done.send(Err(RuntimeError::Busy));
                            continue;
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
                        let candidate_parts = prepare_with_plugin_hook_source_reusing_lease(
                            &input.config,
                            input.prepare.clone(),
                            runtime.plugin_hooks.as_ref(),
                            reuse_lease,
                            true,
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
                                // Preserve the injected VL hook across reprepare
                                // (/model swap, reconfigure).
                                image_preprocessor: runtime.image_preprocessor.clone(),
                            },
                            Err(error) => {
                                let error = runtime_prepare_error(error);
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
                        let replacement = match assemble_runtime_resources(&mut candidate) {
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
                        agent_available = true;
                        observed_tokens = None;
                        snapshot_in_flight = false;
                        compaction_suspended = false;
                        if matches!(
                            operation,
                            ReconfigureKind::FreshSession | ReconfigureKind::ChangeDirectory
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
                        let _ = runtime_event_tx
                            .send(CodingRuntimeEvent::SessionChanged(changed.clone()));
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
                        let undo_sidecars = match persist_runtime_undo(
                            &mut runtime,
                            Some(original.as_ref()),
                            &truncated,
                        ) {
                            Ok(sidecars) => sidecars,
                            Err(error) => {
                                if error.is_snapshot_conflict() {
                                    resources = Some(runtime);
                                    let _ = done.send(Err(RuntimeError::Busy));
                                    continue;
                                }
                                let health_error = native_session_health_error(&runtime);
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
                        let stop_report = stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeReconfigured,
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
                        match assemble_runtime_resources(&mut runtime) {
                            Ok(replacement) => {
                                agent = Some(replacement);
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
                                        },
                                    ));
                                } else {
                                    match assemble_runtime_resources(&mut runtime) {
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
                        );
                        let candidate = match persisted.as_ref() {
                            Ok(_) => assemble_runtime_resources(&mut runtime)
                                .map_err(NativePersistenceError::certain),
                            Err(error) => Err(error.clone()),
                        };
                        match candidate {
                            Ok(replacement) => {
                                agent = Some(replacement);
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
                                        },
                                    ));
                                } else {
                                    match assemble_runtime_resources(&mut runtime) {
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
                            current.finish(GoalTerminal::Cancelled, "cleared by user");
                            let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(current.progress()));
                        }
                        if active_turn.is_some() { let _ = send_agent_command(&agent, AgentCommand::Cancel); }
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
                        if active_turn.is_some() { let _ = send_agent_command(&agent, AgentCommand::Cancel); }
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
                                if let Some(warning) =
                                    resources.as_ref().and_then(|runtime| {
                                        runtime.parts.take_cost_persistence_warning()
                                    })
                                {
                                    let _ = runtime_event_tx
                                        .send(CodingRuntimeEvent::ControllerWarning(warning));
                                }
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
                                if terminal_reason.is_some() {
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
                                if let Some(reason) = terminal_reason.take() {
                                    pending_requests.clear();
                                    let stats = std::mem::take(&mut turn_stats);
                                    let turn_id = active_turn.unwrap_or_default();
                                    let mut completion_reason = reason;
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
                                        if let Some(why) = stop_reason {
                                            state.finish(
                                                GoalTerminal::Stopped,
                                                format!("stopped: {why}"),
                                            );
                                            completion_reason = match why {
                                                "round limit" => StopReason::MaxRounds,
                                                "time limit" => StopReason::Timeout,
                                                _ => StopReason::ProviderError,
                                            };
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
                                                state.finish(
                                                    GoalTerminal::Failed,
                                                    "stopped: too many failed rounds",
                                                );
                                                completion_reason = StopReason::ProviderError;
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::GoalChanged(state.progress()));
                                                let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning("goal stopped: too many failed rounds".into()));
                                            }
                                        } else {
                                            state.finish(
                                                GoalTerminal::Failed,
                                                format!("ended: {reason:?}"),
                                            );
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
        CodingRuntimeControl::Submit { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::Respond { done, .. }
        | CodingRuntimeControl::Cancel { done, .. }
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
        CodingRuntimeControl::McpStatus { done, .. } => {
            let _ = done.send(Err(RuntimeError::Unavailable));
        }
        CodingRuntimeControl::McpTools { done, .. } => {
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

fn fail_close_pending_requests(
    agent: &Option<AgentHandle>,
    pending_requests: &mut BTreeSet<RequestId>,
    cancel_turn: bool,
) {
    if let Some(agent) = agent.as_ref() {
        for id in pending_requests.iter().copied() {
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
) -> Result<Option<(ReprepareInput, Option<SessionLease>)>, RuntimeError> {
    match target {
        ReprepareTarget::Exact(input) => Ok(Some((input, None))),
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
            )))
        }
        ReprepareTarget::ResumeWithLease {
            id,
            working_dir,
            lease,
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

struct NativeUndoSidecars {
    message_count: u32,
    turn_count: u32,
    turn_stats: Vec<TurnStat>,
    archived_turn_stats: Vec<TurnStat>,
    removed_presentation: Vec<(usize, PresentationEntry)>,
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

fn persist_runtime_undo(
    runtime: &mut RuntimeResources,
    expected_snapshot: Option<&SessionSnapshot>,
    snapshot: &SessionSnapshot,
) -> Result<Option<NativeUndoSidecars>, NativePersistenceError> {
    let Some(binding) = runtime.parts.session.as_ref() else {
        runtime.parts.set_runtime_resume(snapshot.clone());
        return Ok(None);
    };
    let message_count = u32::try_from(snapshot.messages.len()).map_err(|_| {
        NativePersistenceError::certain("snapshot message count exceeds native metadata")
    })?;
    let mut snapshot_conflict = false;
    let sidecars = binding
        .manager
        .commit_native_runtime_mutation(
            &binding.lease,
            snapshot,
            |current_snapshot, meta, presentation| {
                if expected_snapshot.is_some_and(|expected| current_snapshot != expected) {
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
                };
                sidecars.archived_turn_stats = meta.archive_turn_stats_where(|stat| {
                    stat.position_valid && stat.after_message > snapshot.messages.len()
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
    } = sidecars;
    let mut snapshot_conflict = false;
    binding
        .manager
        .commit_native_runtime_mutation(
            &binding.lease,
            snapshot,
            |current_snapshot, meta, presentation| {
                if current_snapshot != expected_current_snapshot {
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
        binding
            .manager
            .commit_native_runtime_mutation(
                &binding.lease,
                snapshot,
                |_current_snapshot, _meta, _presentation| Ok(()),
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

fn native_session_health_error(runtime: &RuntimeResources) -> Option<String> {
    let binding = runtime.parts.session.as_ref()?;
    binding
        .manager
        .load_native_session(&binding.id)
        .err()
        .map(|error| error.to_string())
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
    conversation_changed: bool,
    persistence_failure: Option<String>,
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

async fn stop_current_agent(
    agent: &mut Option<AgentHandle>,
    compactions: &mut CompactionTracker,
    observed_tokens: &mut Option<usize>,
    runtime_event_tx: &RuntimeEventEmitter,
    reason: CompactionInterruption,
    persistence_status: Option<SnapshotPersistenceStatus>,
) -> StopReport {
    let Some(mut agent) = agent.take() else {
        compactions.interrupt_all(reason, runtime_event_tx);
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
        record_stopped_conversation_event(&mut report, &event);
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
    report.persistence_failure =
        persistence_status.and_then(|status| status.take_uncertain_commit());
    report
}

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
    }));
    Some(RuntimeError::ReconfigureFailed(message))
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

#[cfg(test)]
mod tests {
    use super::*;

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

    struct DeletePresentationAndFailSecondBuildFactory {
        builds: std::sync::atomic::AtomicUsize,
        presentation_path: std::path::PathBuf,
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

    impl CodingProviderFactory for DeletePresentationAndFailSecondBuildFactory {
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
            std::fs::remove_file(&self.presentation_path).map_err(|error| {
                crate::ProviderBuildError::Adapter(format!(
                    "could not arrange rollback persistence failure: {error}"
                ))
            })?;
            Err(crate::ProviderBuildError::Adapter(
                "candidate provider failed after presentation removal".into(),
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
            pricing: None,
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
                skill_dirs: Some(Vec::new()),
                plugin_skill_dirs: Vec::new(),
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
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        let _ = kernel_commands.recv().await;

        for attempt in 0..2 {
            kernel_events
                .send(AgentEvent::TurnComplete {
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

        handle.start_loop("watch CI").await.unwrap();
        let _ = runtime_events.recv().await;
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
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

        handle.start_loop("watch CI").await.unwrap();
        let _ = runtime_events.recv().await;
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
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
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        let _ = kernel_commands.recv().await;
        drop(kernel_commands);
        kernel_events
            .send(AgentEvent::TurnComplete {
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

        handle.start_loop("watch CI").await.unwrap();
        let _ = runtime_events.recv().await;
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
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
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "initial turn"
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
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "initial turn"
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
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
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "initial turn"
        ));
        kernel_events
            .send(AgentEvent::TurnComplete {
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

        handle.start_loop("watch CI").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: true,
                ..
            }))
        ));
        assert!(loop_active.load(Ordering::Acquire));
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::SendMessage { text, .. }) if text == "initial turn"
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
            _active_model: String,
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
            _active_model: String,
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

    async fn spawn_with_preprocessor(
        pp: Option<Arc<dyn ImagePreprocessor>>,
    ) -> (
        CodingRuntimeHandle,
        mpsc::UnboundedReceiver<AgentCommand>,
        mpsc::UnboundedReceiver<CodingRuntimeEvent>,
        KernelRuntimeAdapter,
    ) {
        let (agent, kernel_commands, _kernel_events) = fake_agent();
        let (handle, controls) = coding_runtime_control_channel();
        let (runtime_tx, runtime_events) = mpsc::unbounded_channel();
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
        (handle, kernel_commands, runtime_events, adapter)
    }

    // The installed preprocessor runs on an image-carrying submit, and its
    // rewritten `(text, images)` — not the raw input — is what reaches the
    // kernel. This is the seam that restores TUI VL image recognition.
    #[tokio::test]
    async fn image_submit_runs_installed_preprocessor_before_kernel() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, mut kernel_commands, mut runtime_events, _adapter) =
            spawn_with_preprocessor(Some(Arc::new(RecordingPreprocessor {
                called: called.clone(),
            })))
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

    // Guard: a text-only submit skips the preprocessor entirely (no images),
    // so the original text passes through untouched.
    #[tokio::test]
    async fn text_only_submit_skips_preprocessor() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, mut kernel_commands, _runtime_events, _adapter) =
            spawn_with_preprocessor(Some(Arc::new(RecordingPreprocessor {
                called: called.clone(),
            })))
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
        let (handle, mut kernel_commands, mut runtime_events, _adapter) =
            spawn_with_preprocessor(Some(Arc::new(FailingPreprocessor))).await;

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

        handle.start_loop("watch CI").await.unwrap();
        assert!(matches!(
            runtime_events.recv().await,
            Some(CodingRuntimeEvent::LoopChanged(LoopProgress {
                active: true,
                ..
            }))
        ));
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
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
        assert!(matches!(
            handle.resume_session(missing_session_id).await,
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
    #[serial_test::serial(atomcode_home)]
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

        handle.start_loop("watch CI").await.unwrap();
        let _ = runtime_events.recv().await;
        handle
            .submit(UserInput::from("initial turn"))
            .await
            .unwrap();
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
        assert!(matches!(
            kernel_commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
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
            CodingRuntimeEvent::Reconfigured {
                operation: ReconfigureKind::Provider
            }
        ));
        assert!(first.sequence < second.sequence && second.sequence < third.sequence);
        runtime.handle.shutdown().await.unwrap();
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

        let mut model_b =
            CodingAgentConfig::new("key", "https://example.test/v1", "model-b", project.path());
        model_b.provider_name = "provider-b".into();
        runtime
            .handle
            .reassemble_provider(model_b.clone())
            .await
            .unwrap();
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
        assert_eq!(catalog.code_unavailable, None);

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
        assert_eq!(visible(&before), visible(&after_second_reassemble));
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
        manager.save_snapshot(id, &newer).unwrap();
        let (done, result) = oneshot::channel();
        runtime
            .handle
            .tx
            .send(CodingRuntimeControl::ApplyUndo {
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
        assert_eq!(manager.load_snapshot(id).unwrap(), newer);
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
        let presentation_path = manager.presentation_path(id).unwrap();
        std::fs::remove_file(&presentation_path).unwrap();

        let error = runtime.handle.undo_to_prompt(None).await.unwrap_err();

        let RuntimeError::ReconfigureFailed(message) = &error else {
            panic!("expected presentation persistence error, got {error:?}");
        };
        assert!(
            message.contains(presentation_path.to_string_lossy().as_ref()),
            "expected missing presentation path in error, got {error:?}"
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Failed);
        assert_eq!(
            runtime.handle.submit(UserInput::from("must fail")).await,
            Err(RuntimeError::Unavailable)
        );
        runtime.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn native_undo_rollback_persistence_failure_is_sticky() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let id = "undo-rollback-persistence-failure";
        let manager = atomcode_capabilities::session::SessionManager::for_project(project.path());
        let snapshot = SessionSnapshot::new(vec![
            Message::user("first prompt"),
            Message::assistant("answer", Vec::new()),
        ]);
        persist_native_session(&manager, id, project.path(), &snapshot);
        let mut start = native_start(false);
        start.agent.working_dir = project.path().to_path_buf();
        start.prepare.session = crate::SessionMode::Resume(id.into());
        start.provider_factory = Arc::new(DeletePresentationAndFailSecondBuildFactory {
            builds: std::sync::atomic::AtomicUsize::new(0),
            presentation_path: manager.presentation_path(id).unwrap(),
        });
        let runtime = CodingRuntime::start(start).await.unwrap();

        // Resume may normalize the live snapshot (for example, refreshing the
        // current persona) before a turn persists it. Align the canonical CAS
        // preimage so this test reaches the intended rollback-failure branch.
        let live_snapshot = runtime.handle.snapshot().await.unwrap();
        manager.save_snapshot(id, &live_snapshot).unwrap();
        assert_eq!(
            live_snapshot.as_ref(),
            &manager.load_snapshot(id).unwrap(),
            "live and canonical snapshots must agree before undo"
        );

        let undo = runtime.handle.undo_to_prompt(None).await;
        assert!(
            matches!(
                &undo,
                Err(RuntimeError::ReconfigureFailed(message))
                    if message.contains("snapshot restore failed")
            ),
            "unexpected undo result: {undo:?}"
        );
        assert_eq!(runtime.handle.status().phase, RuntimePhase::Failed);
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
        start.provider_factory = Arc::new(DeletePresentationAndFailSecondBuildFactory {
            builds: std::sync::atomic::AtomicUsize::new(0),
            presentation_path: manager.presentation_path(id).unwrap(),
        });
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        let live_snapshot = runtime.handle.snapshot().await.unwrap();
        manager.save_snapshot(id, &live_snapshot).unwrap();
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
            wakeup_tx,
            loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            image_preprocessor,
        };

        let mut truncated = original_snapshot.clone();
        truncated.messages.truncate(2);
        let receipt = persist_runtime_undo(&mut resources, Some(&original_snapshot), &truncated)
            .unwrap()
            .expect("native undo must retain a sidecar rollback receipt");
        assert_eq!(manager.load_snapshot(id).unwrap(), truncated);
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

        assert_eq!(manager.load_snapshot(id).unwrap(), original_snapshot);
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
        let second_receipt =
            persist_runtime_undo(&mut resources, Some(&original_snapshot), &second_truncated)
                .unwrap()
                .expect("second native undo must retain a rollback receipt");
        let concurrently_advanced = SessionSnapshot::new(vec![
            Message::user("concurrent"),
            Message::assistant("newer answer", Vec::new()),
        ]);
        manager.save_snapshot(id, &concurrently_advanced).unwrap();

        let error = restore_runtime_undo(
            &mut resources,
            &second_truncated,
            &original_snapshot,
            Some(second_receipt),
        )
        .unwrap_err();
        assert!(error.is_snapshot_conflict());
        assert_eq!(manager.load_snapshot(id).unwrap(), concurrently_advanced);
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
}
