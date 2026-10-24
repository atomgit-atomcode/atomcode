/** Navigation is a projection of visible messages, never a second conversation. */
export interface NavigationMessage {
  role: string;
  parts: readonly { kind: string; text?: string }[];
  retainedHistory?: boolean;
  images?: readonly unknown[];
  pendingSteerId?: string;
}
export interface TurnNavigationItem {
  key: string;
  messageIndex: number;
  endIndex: number;
  user: string;
  assistant: string;
  retained: boolean;
  pending: boolean;
}
export function navigationSummary(text: string, limit = 160): string {
  const plain = text.trimStart().slice(0, Math.max(512, limit * 8)).replace(/\s+/g, ' ').trim();
  return plain.length > limit ? `${plain.slice(0, limit)}…` : plain;
}
function textOf(message: NavigationMessage): string {
  let result = '';
  for (const part of message.parts) {
    if (part.kind === 'text' && part.text) result += part.text.slice(0, 240);
    if (result.length >= 240) break;
  }
  return navigationSummary(result);
}
export function deriveTurnNavigation(messages: readonly NavigationMessage[], pendingIds: ReadonlySet<string> = new Set()): TurnNavigationItem[] {
  const turns: TurnNavigationItem[] = [];
  messages.forEach((message, index) => {
    if (message.role === 'user') {
      turns.push({ key: `message-${index}`, messageIndex: index, endIndex: index,
        user: textOf(message), assistant: '', retained: !!message.retainedHistory,
        pending: message.pendingSteerId !== undefined && pendingIds.has(message.pendingSteerId) });
    } else if (turns.length) {
      const turn = turns[turns.length - 1];
      turn.endIndex = index;
      if (message.role === 'assistant' && turn.assistant.length < 160) {
        const text = textOf(message);
        if (text) turn.assistant = navigationSummary(`${turn.assistant} ${text}`);
      }
    }
  });
  return turns;
}
/** Fixed-height rail rows keep DOM size independent of session length. */
export function turnWindow(count: number, scrollTop: number, height: number, pitch: number) {
  const start = Math.max(0, Math.min(count, Math.floor(Math.max(0, scrollTop) / pitch) - 6));
  const end = Math.min(count, Math.ceil((Math.max(0, scrollTop) + height) / pitch) + 6);
  return { start, end };
}
/** Last anchor at/before the reading line; O(log turns) DOM measurements. */
export function turnAtReadingLine(count: number, top: (index: number) => number, line: number): number {
  let lo = 0, hi = count;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (top(mid) <= line) lo = mid + 1; else hi = mid;
  }
  return Math.max(0, lo - 1);
}

/** Smooth navigation must not mistake intermediate scroll frames for user intent. */
export function createTurnNavigationIntent() {
  let target: number | null = null;
  let follow = false;
  return {
    jumpTo(top: number) { target = top; follow = false; },
    followBottom() { target = null; follow = true; },
    interact() { target = null; follow = false; },
    onScroll(viewport: { scrollTop: number; scrollHeight: number; clientHeight: number }): boolean | null {
      if (follow) {
        if (viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight <= 2) follow = false;
        return true;
      }
      if (target !== null) {
        if (!Number.isFinite(target) || Math.abs(viewport.scrollTop - target) > 2) return false;
        target = null;
      }
      return null; // Delegate ordinary scroll and browser restoration to their existing owner.
    },
  };
}
