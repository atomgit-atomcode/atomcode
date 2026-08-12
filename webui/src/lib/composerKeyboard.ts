export interface ComposerEnterEvent {
  key: string;
  shiftKey: boolean;
  isComposing: boolean;
}

/** Mobile soft keyboards have no Shift+Enter chord, so Enter must remain a newline. */
export function shouldSendComposerOnEnter(
  event: ComposerEnterEvent,
  coarsePointer: boolean,
): boolean {
  return event.key === 'Enter'
    && !event.shiftKey
    && !event.isComposing
    && !coarsePointer;
}

export function hasCoarsePointer(): boolean {
  return typeof window !== 'undefined'
    && typeof window.matchMedia === 'function'
    && window.matchMedia('(pointer: coarse)').matches;
}
