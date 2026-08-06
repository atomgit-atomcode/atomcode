export interface ChatDoneTerminal {
  stopReason?: string;
  message?: string;
}

export type ChatDoneDisposition =
  | { kind: 'completed'; discardQueued: false }
  | { kind: 'incomplete'; discardQueued: true; detail: string };

/**
 * Classify the daemon's authoritative turn terminal.
 *
 * Older daemons did not include `stop_reason`, so an absent reason remains a
 * natural completion for wire compatibility. Once a reason is present, only
 * kernel `Stopped` (`stopped` on the wire) means the model finished normally;
 * every other current or future reason is incomplete and must fail closed.
 */
export function classifyChatDone(terminal: ChatDoneTerminal): ChatDoneDisposition {
  if (terminal.stopReason === undefined || terminal.stopReason === 'stopped') {
    return { kind: 'completed', discardQueued: false };
  }

  const message = terminal.message?.trim();
  return {
    kind: 'incomplete',
    discardQueued: true,
    detail: message || terminal.stopReason || 'unknown_terminal',
  };
}

export interface LiveLifecycleState {
  running: boolean;
  terminalConsumed: boolean;
}

export type LiveLifecycleEvent =
  | { type: 'snapshot' }
  | { type: 'input_accepted' }
  | { type: 'state'; running: boolean; stopReason?: string; message?: string }
  | { type: 'error'; message: string };

export interface LiveLifecycleTransition {
  state: LiveLifecycleState;
  terminal?: ChatDoneDisposition;
  diagnostic?: string;
}

export function createLiveLifecycleState(): LiveLifecycleState {
  return { running: false, terminalConsumed: false };
}

/**
 * Track only authoritative `/live` lifecycle observations.
 *
 * Kernel error events are diagnostics and cannot finish a turn. The following
 * `state { running: false }` owns the terminal, and duplicate idle states are
 * ignored so queue/persistence side effects happen once.
 */
export function reduceLiveLifecycle(
  current: LiveLifecycleState,
  event: LiveLifecycleEvent,
): LiveLifecycleTransition {
  switch (event.type) {
    case 'snapshot':
      return { state: createLiveLifecycleState() };
    case 'input_accepted':
      return { state: { running: true, terminalConsumed: false } };
    case 'error':
      return { state: current, diagnostic: event.message };
    case 'state':
      if (event.running) {
        return { state: { running: true, terminalConsumed: false } };
      }
      if (current.terminalConsumed) {
        return { state: { running: false, terminalConsumed: true } };
      }
      return {
        state: { running: false, terminalConsumed: true },
        terminal: classifyChatDone({
          stopReason: event.stopReason,
          message: event.message,
        }),
      };
  }
}

/** A snapshot is persisted history, not evidence that a turn is active. */
export function restoreLiveSnapshot<T>(messages: T[]): { messages: T[]; running: false } {
  return { messages, running: false };
}

export type LiveSnapshotQueueDisposition =
  | { discardQueued: false }
  | { discardQueued: true; reason: 'terminal_unknown' };

/**
 * A reconnect snapshot contains persisted history but no terminal reason. If
 * the previous connection observed an active turn, the daemon may have already
 * cleared that turn's replay window while the client was disconnected. Queued
 * input therefore cannot be drained safely until a fresh authoritative turn
 * lifecycle is observed.
 */
export function liveSnapshotQueueDisposition(
  wasRunning: boolean,
  queuedCount: number,
): LiveSnapshotQueueDisposition {
  return wasRunning && queuedCount > 0
    ? { discardQueued: true, reason: 'terminal_unknown' }
    : { discardQueued: false };
}

export type ChatRecoveryState =
  | 'ready'
  | 'checking'
  | 'detached_active'
  | 'terminal_unknown';

export type ChatRecoveryEvent =
  | { type: 'session_switch'; hasSession: boolean }
  | { type: 'active_check_succeeded'; active: boolean }
  | { type: 'active_check_failed' }
  | { type: 'transport_lost' }
  | { type: 'stop_succeeded' }
  | { type: 'stop_failed' }
  | { type: 'authoritative_terminal' };

export interface ChatRecoveryPolicy {
  allowSend: boolean;
  allowQueueDrain: boolean;
  allowStop: boolean;
}

/**
 * `/chat` has cancellation and active-operation discovery, but no stream
 * reattach protocol. Keep that recovery boundary explicit: until discovery or
 * cancellation proves the old operation is gone, neither direct sends nor
 * queued auto-sends are safe.
 */
export function reduceChatRecovery(
  _current: ChatRecoveryState,
  event: ChatRecoveryEvent,
): ChatRecoveryState {
  switch (event.type) {
    case 'session_switch':
      return event.hasSession ? 'checking' : 'ready';
    case 'active_check_succeeded':
      return event.active ? 'detached_active' : 'ready';
    case 'active_check_failed':
    case 'transport_lost':
    case 'stop_failed':
      return 'terminal_unknown';
    case 'stop_succeeded':
    case 'authoritative_terminal':
      return 'ready';
  }
}

export function chatRecoveryPolicy(state: ChatRecoveryState): ChatRecoveryPolicy {
  if (state === 'ready') {
    return { allowSend: true, allowQueueDrain: true, allowStop: false };
  }
  return { allowSend: false, allowQueueDrain: false, allowStop: true };
}

export type SyncAttachDisposition =
  | { allowed: true }
  | { allowed: false; reason: 'active_chat' | 'unresolved_chat' };

export function syncAttachDisposition(
  chatRunning: boolean,
  recoveryState: ChatRecoveryState,
): SyncAttachDisposition {
  if (chatRunning) return { allowed: false, reason: 'active_chat' };
  if (recoveryState !== 'ready') {
    return { allowed: false, reason: 'unresolved_chat' };
  }
  return { allowed: true };
}

/** Gate every `/chat` event, not only the promise terminal, against replacement. */
export function isCurrentChatStream(
  requestId: string,
  requestGeneration: number,
  currentRequestId: string | null,
  currentGeneration: number,
  aborted: boolean,
): boolean {
  return (
    !aborted &&
    requestId === currentRequestId &&
    requestGeneration === currentGeneration
  );
}

export type LiveDetachDisposition =
  | { allowed: true }
  | { allowed: false; reason: 'active_turn' };

export function liveDetachDisposition(running: boolean): LiveDetachDisposition {
  return running ? { allowed: false, reason: 'active_turn' } : { allowed: true };
}

/** A shared live runtime has one foreground session owner. Switching that
 * owner while its turn is active would reconfigure the runtime and cancel the
 * turn, so navigation must wait for an authoritative terminal. */
export function liveSessionSwitchDisposition(running: boolean): LiveDetachDisposition {
  return running ? { allowed: false, reason: 'active_turn' } : { allowed: true };
}

/** Clear only the structured-input prompt whose native request was resolved.
 * A late terminal for an older request must never dismiss a newer prompt. */
export function resolveUserInputRequest<T extends { request_id: number }>(
  current: T | null,
  resolvedRequestId: number,
): T | null {
  return current?.request_id === resolvedRequestId ? null : current;
}
