export type ApprovalMode = 'build' | 'plan' | 'bypass' | 'accept_edits';

export interface ApprovalModeState {
  confirmedMode: ApprovalMode;
  displayMode: ApprovalMode;
  pendingMode?: ApprovalMode;
}

export function initApprovalModeState(mode: ApprovalMode): ApprovalModeState {
  return {
    confirmedMode: mode,
    displayMode: mode,
  };
}

export function beginApprovalModeSwitch(
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

export function completeApprovalModeSwitch(
  state: ApprovalModeState,
  confirmed: ApprovalMode,
): ApprovalModeState {
  if (!state.pendingMode) return state;
  return {
    confirmedMode: confirmed,
    displayMode: confirmed,
  };
}

export function failApprovalModeSwitch(state: ApprovalModeState): ApprovalModeState {
  if (!state.pendingMode) return state;
  return {
    confirmedMode: state.confirmedMode,
    displayMode: state.confirmedMode,
  };
}
