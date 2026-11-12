import type { ImageData, TranscriptTurn } from '../api';
import type { MsgPart } from './toolRows';

import { displayHistoryUser } from './historyMessages.ts';

export const HISTORY_BATCH = 6;
export interface HistoryMessage {
  role: 'user' | 'assistant' | 'system';
  parts: MsgPart[];
  ts?: number;
  retainedHistory?: boolean;
  images?: ImageData[];
}

/** Only join on an unambiguous identity, never on a repeated prompt alone. */
export function historyBoundary(turns: readonly TranscriptTurn[], messages: readonly HistoryMessage[]): { end: number; liveStart: number; joined: boolean } {
  const liveStart = messages.findIndex(m => m.role === 'user' && !m.retainedHistory);
  if (liveStart < 0) return { end: turns.length, liveStart: 0, joined: false };
  const first = messages[liveStart];
  const text = first.parts.filter(p => p.kind === 'text').map(p => p.text).join('');
  const matches = turns.map((t, index) => ({ t, index })).filter(({ t }) => !t.undone &&
    (first.ts != null && t.started_at != null ? first.ts === t.started_at : !t.user_truncated && displayHistoryUser(t.user, t.user_images).text === text));
  if (matches.length === 1) return { end: matches[0].index, liveStart, joined: true };
  const last = turns[turns.length - 1];
  if (first.ts != null && (!last || (last.ts != null && first.ts > last.ts))) {
    return { end: turns.length, liveStart, joined: true };
  }
  return { end: turns.length, liveStart: 0, joined: false };
}

export function historyWindow(end: number, target?: number, batch = HISTORY_BATCH) {
  const start = target == null ? Math.max(0, end - batch) : Math.max(0, Math.min(target, end - 1));
  return { start, end: Math.min(end, start + batch) };
}

/** Stop at known history edges before starting a request or showing a spinner. */
export function hasHistoryPage(current: { start: number; end: number } | null, end: number,
  direction: 'before' | 'after'): boolean {
  return direction === 'before' ? (current ? current.start > 0 : end > 0)
    : current != null && current.end < end;
}

export function historyMessages(turn: TranscriptTurn): HistoryMessage[] {
  const parts: MsgPart[] = [];
  if (turn.steps) {
    for (const step of turn.steps) {
      if (step.text) parts.push({ kind: 'text', text: step.text });
      for (const tool of step.tools) parts.push({ kind: 'tool', tool: {
        id: tool.id, name: tool.name, args: tool.args,
        status: tool.result === null ? 'incomplete' : tool.is_error ? 'error' : 'done',
        output: tool.result ?? undefined,
      } });
    }
  } else {
    if (turn.assistant) parts.push({ kind: 'text', text: turn.assistant });
    for (const [index, tool] of (turn.tools ?? []).entries()) parts.push({ kind: 'tool', tool: {
      id: `history-${turn.turn_id}-${index}`, name: tool.name, args: tool.args,
      status: tool.is_error ? 'error' : 'done', output: tool.result,
    } });
  }
  const user = displayHistoryUser(turn.user, turn.user_images);
  return [
    { role: 'user', parts: [{ kind: 'text', text: user.text }], images: user.images, ts: turn.started_at },
    { role: 'assistant', parts, ts: turn.ts },
  ];
}
