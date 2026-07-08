export interface IdleNoticeState {
  isGenerating: boolean;
  lastEventAt: number;
  now: number;
  thresholdMs: number;
  alreadyShown: boolean;
}

export function shouldShowIdleNotice(state: IdleNoticeState): boolean {
  return state.isGenerating
    && !state.alreadyShown
    && state.now - state.lastEventAt >= state.thresholdMs;
}
