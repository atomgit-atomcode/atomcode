export interface ComposerEnterEvent {
  key: string;
  shiftKey: boolean;
  isComposing: boolean;
  /** Safari/WebKit and some desktop IMEs report composition as legacy 229. */
  keyCode?: number;
}

/** Mobile soft keyboards have no Shift+Enter chord, so Enter must remain a newline. */
export function shouldSendComposerOnEnter(
  event: ComposerEnterEvent,
  coarsePointer: boolean,
): boolean {
  return event.key === 'Enter'
    && !event.shiftKey
    && !event.isComposing
    && event.keyCode !== 229
    && !coarsePointer;
}

export function hasCoarsePointer(): boolean {
  return typeof window !== 'undefined'
    && typeof window.matchMedia === 'function'
    && window.matchMedia('(pointer: coarse)').matches;
}
