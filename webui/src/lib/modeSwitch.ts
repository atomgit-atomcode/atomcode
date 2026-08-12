export type ApprovalMode = 'build' | 'plan' | 'bypass' | 'accept_edits';

export interface ApprovalModeState {
  confirmedMode: ApprovalMode;
  displayMode: ApprovalMode;
  pendingMode?: ApprovalMode;
}

export function initModeState(mode: ApprovalMode): ApprovalModeState {
  return {
    confirmedMode: mode,
    displayMode: mode,
  };
}

export function beginModeSwitch(
  state: ApprovalModeState,
  requested: ApprovalMode,
): ApprovalModeState {
  if (state.pendingMode || requested === state.displayMode) return state;
  return {
    confirmedMode: state.confirmedMode,
    displayMode: requested,
    pendingMode: requested,
  };
}

export function completeModeSwitch(
  state: ApprovalModeState,
  confirmed: ApprovalMode,
): ApprovalModeState {
  if (!state.pendingMode) return state;
  return {
    confirmedMode: confirmed,
    displayMode: confirmed,
  };
}

export function failModeSwitch(state: ApprovalModeState): ApprovalModeState {
  if (!state.pendingMode) return state;
  return {
    confirmedMode: state.confirmedMode,
    displayMode: state.confirmedMode,
  };
}

/**
 * Freeze the approval mode for a queued prompt only when it is authoritative.
 * A pending selection is merely optimistic UI state; returning undefined lets
 * queue draining use the daemon-confirmed (or rolled-back) mode at that time.
 */
export function modeForQueuedPrompt(state: ApprovalModeState): ApprovalMode | undefined {
  return state.pendingMode ? undefined : state.confirmedMode;
}
