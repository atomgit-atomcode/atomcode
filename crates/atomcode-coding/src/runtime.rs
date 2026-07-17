//! Stable driver control plane and kernel-agent owner for a coding runtime.
//!
//! The runtime owns the replaceable kernel [`AgentHandle`] so native controls and
//! events never need to traverse a legacy driver adapter. During incremental
//! migration the adapter returned by [`spawn_runtime_owner`] still exposes all
//! non-compaction kernel traffic to `atomcode-bridge`.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::checkpoint::CompactionCheckpointError;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::message::{
    CompactionStrategy, CompactionView, Conversation, Message, SessionSnapshot,
};
pub use atomcode_kernel::message::CompactTrigger;
use atomcode_kernel::provider::LlmProvider;
use tokio::sync::{mpsc, oneshot};

/// Runtime facts emitted by the coding engine without depending on the legacy
/// `atomcode-core` driver protocol.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum CodingRuntimeEvent {
    /// A potentially slow compaction strategy has started.
    CompactionStarted { trigger: CompactTrigger },
    /// A compaction attempt reached exactly one terminal state.
    CompactionFinished { completion: CompactionCompletion },
}

/// Terminal state of a compaction accepted by the coding runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactionCompletion {
    /// The kernel returned a normal compaction result.
    Completed(CompactionOutcome),
    /// The prepared result could not be durably checkpointed, so it was not committed.
    Failed { trigger: CompactTrigger, error: CompactionCheckpointError },
    /// The owning runtime was replaced or stopped before the kernel returned a result.
    Interrupted { trigger: CompactTrigger, reason: CompactionInterruption },
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
    Replace { old_start: usize, old_end: usize, new_end: usize },
}

/// Apply the same v2 manual-compaction policy and kernel invariants to a
/// persisted message list.
pub async fn compact_snapshot(
    messages: Vec<Message>,
    provider: Arc<dyn LlmProvider>,
    focus: Option<String>,
) -> SnapshotCompaction {
    let mut conversation = Conversation { messages, cache_epoch: 0 };
    let floor = conversation.sacred_floor();
    let (recorded_window, used_tokens, _) = conversation.last_pressure();
    let live_window = provider.context_window();
    let ctx_window = if live_window > 0 { live_window } else { recorded_window };
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
    SnapshotCompaction { messages: conversation.messages, outcome, mutation }
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
        SnapshotCompactionMutation::Replace { old_start, old_end, new_end }
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
        let estimated_tokens_after = estimate_after_tokens(
            estimated_tokens_before,
            bytes_before,
            bytes_after,
        );

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

fn estimate_after_tokens(
    tokens_before: usize,
    bytes_before: usize,
    bytes_after: usize,
) -> usize {
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
}

impl CodingRuntimeHandle {
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
}

/// The runtime owner side of [`CodingRuntimeHandle`].
///
/// This type intentionally hides the Tokio receiver so ownership stays singular.
#[derive(Debug)]
pub struct CodingRuntimeControlReceiver {
    rx: mpsc::UnboundedReceiver<CodingRuntimeControl>,
    state: Arc<AtomicU64>,
}

impl CodingRuntimeControlReceiver {
    pub async fn recv(&mut self) -> Option<CodingRuntimeControl> {
        self.rx.recv().await
    }
}

/// Internal control envelope consumed by the current runtime owner.
///
/// It is public only because the temporary owner lives in `atomcode-bridge`, a
/// separate crate. Drivers should use capability methods on [`CodingRuntimeHandle`].
#[doc(hidden)]
#[derive(Debug)]
pub enum CodingRuntimeControl {
    Compact { generation: u64, focus: Option<String> },
}

/// Build the two ends of the stable runtime control channel.
#[doc(hidden)]
pub fn coding_runtime_control_channel() -> (CodingRuntimeHandle, CodingRuntimeControlReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    // A standalone channel is immediately usable. The runtime owner overrides
    // this flag at spawn time when startup produced only a degraded placeholder.
    let state = Arc::new(AtomicU64::new(runtime_state(0, true)));
    (
        CodingRuntimeHandle { tx, state: Arc::clone(&state) },
        CodingRuntimeControlReceiver { rx, state },
    )
}

const RUNTIME_AVAILABLE: u64 = 1;

fn runtime_state(generation: u64, available: bool) -> u64 {
    (generation << 1) | u64::from(available)
}

fn runtime_state_generation(state: u64) -> u64 {
    state >> 1
}

fn runtime_state_available(state: u64) -> bool {
    state & RUNTIME_AVAILABLE != 0
}

/// Temporary kernel-facing adapter used by legacy coordinators while native
/// commands migrate one by one. Compaction traffic is deliberately absent.
pub struct KernelRuntimeAdapter {
    pub commands: mpsc::UnboundedSender<AgentCommand>,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    owner_tx: mpsc::UnboundedSender<OwnerControl>,
}

impl KernelRuntimeAdapter {
    /// Reject new native compaction controls while a coordinator rebuilds the
    /// underlying agent. Accepted controls from the prior generation terminate as
    /// interrupted rather than crossing into the replacement agent.
    pub async fn suspend_compaction(&self) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::SuspendCompaction { done }).await
    }

    /// Resume delivery of native compaction controls after a replacement agent
    /// has been installed successfully.
    pub async fn resume_compaction(&self) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::ResumeCompaction { done }).await
    }

    /// Stop the current agent and install an inert placeholder. Used before a
    /// session/provider rebuild whose prepare phase must run after persistence.
    pub async fn stop_agent(&self) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::Stop { done }).await
    }

    /// Atomically replace the current agent after shutting the previous one down.
    pub async fn replace_agent(&self, agent: AgentHandle) -> Result<(), RuntimeUnavailable> {
        self.manage(|done| OwnerControl::Replace { agent, done }).await
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
        self.owner_tx.send(build(done_tx)).map_err(|_| RuntimeUnavailable)?;
        done_rx.await.map_err(|_| RuntimeUnavailable)
    }
}

enum OwnerControl {
    SuspendCompaction { done: oneshot::Sender<()> },
    ResumeCompaction { done: oneshot::Sender<()> },
    Stop { done: oneshot::Sender<()> },
    Replace { agent: AgentHandle, done: oneshot::Sender<()> },
    Shutdown { done: oneshot::Sender<()> },
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
        self.manual.push_back(ManualCompactionFlight { trigger, started: false });
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
        runtime_event_tx: &mpsc::UnboundedSender<CodingRuntimeEvent>,
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
    mut controls: CodingRuntimeControlReceiver,
    runtime_event_tx: mpsc::UnboundedSender<CodingRuntimeEvent>,
    initial_agent_available: bool,
) -> KernelRuntimeAdapter {
    let (kernel_command_tx, mut kernel_command_rx) = mpsc::unbounded_channel();
    let (kernel_event_tx, kernel_event_rx) = mpsc::unbounded_channel();
    let (owner_tx, mut owner_rx) = mpsc::unbounded_channel();
    let mut generation = 0;
    controls
        .state
        .store(runtime_state(generation, initial_agent_available), Ordering::Release);

    tokio::spawn(async move {
        let mut agent = initial;
        let mut observed_tokens = None;
        let mut controls_open = true;
        let mut compaction_suspended = false;
        let mut agent_available = initial_agent_available;
        let mut compactions = CompactionTracker::default();
        let mut shutdown_was_handled = false;
        loop {
            tokio::select! {
                biased;
                management = owner_rx.recv() => match management {
                    Some(OwnerControl::SuspendCompaction { done }) => {
                        if !compaction_suspended {
                            generation = generation.wrapping_add(1);
                            compaction_suspended = true;
                            controls
                                .state
                                .store(runtime_state(generation, false), Ordering::Release);
                        }
                        let _ = done.send(());
                    }
                    Some(OwnerControl::ResumeCompaction { done }) => {
                        compaction_suspended = false;
                        controls.state.store(
                            runtime_state(generation, agent_available),
                            Ordering::Release,
                        );
                        let _ = done.send(());
                    }
                    Some(OwnerControl::Stop { done }) => {
                        if !compaction_suspended {
                            generation = generation.wrapping_add(1);
                        }
                        compaction_suspended = true;
                        controls
                            .state
                            .store(runtime_state(generation, false), Ordering::Release);
                        stop_current_agent(
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
                            compaction_suspended = true;
                            controls
                                .state
                                .store(runtime_state(generation, false), Ordering::Release);
                        }
                        stop_current_agent(
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
                                .store(runtime_state(generation, true), Ordering::Release);
                        }
                        let _ = done.send(());
                    }
                    Some(OwnerControl::Shutdown { done }) => {
                        if !compaction_suspended {
                            generation = generation.wrapping_add(1);
                        }
                        controls
                            .state
                            .store(runtime_state(generation, false), Ordering::Release);
                        stop_current_agent(
                            &mut agent,
                            &mut compactions,
                            &mut observed_tokens,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeShutdown,
                        )
                        .await;
                        interrupt_queued_controls(
                            &mut controls,
                            &runtime_event_tx,
                            CompactionInterruption::RuntimeShutdown,
                        );
                        let _ = done.send(());
                        shutdown_was_handled = true;
                        break;
                    }
                    None => break,
                },
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
                                .store(runtime_state(generation, false), Ordering::Release);
                            emit_compaction_interrupted(
                                &runtime_event_tx,
                                trigger,
                                CompactionInterruption::RuntimeUnavailable,
                            );
                        }
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
                    None => break,
                },
            }
        }
        controls
            .state
            .store(runtime_state(generation, false), Ordering::Release);
        interrupt_queued_controls(
            &mut controls,
            &runtime_event_tx,
            CompactionInterruption::RuntimeShutdown,
        );
        if !shutdown_was_handled {
            stop_current_agent(
                &mut agent,
                &mut compactions,
                &mut observed_tokens,
                &runtime_event_tx,
                CompactionInterruption::RuntimeShutdown,
            )
            .await;
        }
    });

    KernelRuntimeAdapter {
        commands: kernel_command_tx,
        events: kernel_event_rx,
        owner_tx,
    }
}

fn emit_compaction_interrupted(
    runtime_event_tx: &mpsc::UnboundedSender<CodingRuntimeEvent>,
    trigger: CompactTrigger,
    reason: CompactionInterruption,
) {
    let _ = runtime_event_tx.send(CodingRuntimeEvent::CompactionFinished {
        completion: CompactionCompletion::Interrupted { trigger, reason },
    });
}

fn interrupt_queued_controls(
    controls: &mut CodingRuntimeControlReceiver,
    runtime_event_tx: &mpsc::UnboundedSender<CodingRuntimeEvent>,
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
        }
    }
}

fn handle_compaction_event(
    event: AgentEvent,
    compactions: &mut CompactionTracker,
    observed_tokens: &mut Option<usize>,
    runtime_event_tx: &mpsc::UnboundedSender<CodingRuntimeEvent>,
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

async fn stop_current_agent(
    agent: &mut AgentHandle,
    compactions: &mut CompactionTracker,
    observed_tokens: &mut Option<usize>,
    runtime_event_tx: &mpsc::UnboundedSender<CodingRuntimeEvent>,
    reason: CompactionInterruption,
) {
    let _ = agent.commands.send(AgentCommand::Shutdown);
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(timeout);
    let mut events_open = true;
    loop {
        tokio::select! {
            result = &mut agent.task => {
                let _ = result;
                break;
            }
            event = agent.events.recv(), if events_open => match event {
                Some(event) => {
                    if let Some(AgentEvent::Usage(meta)) = handle_compaction_event(
                        event,
                        compactions,
                        observed_tokens,
                        runtime_event_tx,
                    ) {
                        *observed_tokens = Some(meta.used_tokens as usize);
                    }
                }
                None => events_open = false,
            },
            () = &mut timeout => {
                agent.task.abort();
                let _ = (&mut agent.task).await;
                break;
            }
        }
    }

    while let Ok(event) = agent.events.try_recv() {
        if let Some(AgentEvent::Usage(meta)) = handle_compaction_event(
            event,
            compactions,
            observed_tokens,
            runtime_event_tx,
        ) {
            *observed_tokens = Some(meta.used_tokens as usize);
        }
    }
    compactions.interrupt_all(reason, runtime_event_tx);
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
    AgentHandle { commands, events, task }
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

    fn fake_agent() -> (
        AgentHandle,
        mpsc::UnboundedReceiver<AgentCommand>,
        mpsc::UnboundedSender<AgentEvent>,
    ) {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async {});
        (AgentHandle { commands, events, task }, command_rx, event_tx)
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
        AgentHandle { commands, events, task }
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
        (AgentHandle { commands, events, task }, delivered_rx)
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
            (outcome.estimated_tokens_before, outcome.estimated_tokens_after),
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
                trigger: CompactTrigger::Manual { focus: Some("files".into()) },
            })
            .unwrap();
        let committed_snapshot = SessionSnapshot::new(vec![Message::user("after compact")]);
        kernel_events
            .send(AgentEvent::Compacted {
                trigger: CompactTrigger::Manual { focus: Some("files".into()) },
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

        handle.compact(Some("queued before shutdown".into())).unwrap();
        adapter.shutdown().await.unwrap();

        assert!(matches!(kernel_commands.try_recv(), Ok(AgentCommand::Shutdown)));
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
        let provider = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("anchored summary".into()),
            StreamEvent::Done { truncated: false },
        ]]).with_ctx_window(128_000));

        let result = compact_snapshot(messages, provider, None).await;

        assert!(result.outcome.committed);
        assert!(matches!(
            result.mutation,
            SnapshotCompactionMutation::Replace { .. }
        ));
        assert_eq!(result.messages[0].text, "persona");
        assert_eq!(result.messages[1].text, "original task");
        assert!(result.messages.iter().any(|message| message.text.contains("anchored summary")));
    }
}
