import type { MsgPart } from './toolRows';

/** Display segments, not runtime messages. Splitting never changes the session log. */
export interface TimelineMessage {
  role: 'user' | 'assistant' | 'system';
  parts: MsgPart[];
  ts?: number;
  pendingSteerId?: string;
  archiveKey?: string;
  archiveBytes?: number;
  images?: { data: string }[];
}
export const SEGMENT_PARTS = 8;
export function timelineLimits(compact: boolean) {
  return compact ? { mounted: 12, resident: 36, pageBytes: 1024 * 1024, residentBytes: 2 * 1024 * 1024 }
    : { mounted: 32, resident: 96, pageBytes: 3 * 1024 * 1024, residentBytes: 8 * 1024 * 1024 };
}

/** Keep all unresolved parallel calls in the mutable tail; late results still
 * target the last assistant. Never split the currently streaming final text. */
export function segmentMessages<T extends TimelineMessage>(messages: T[]): T[] {
  let changed = false;
  const result: T[] = [];
  for (const message of messages) {
    if (message.archiveKey || message.role !== 'assistant' || message.parts.length <= SEGMENT_PARTS) {
      result.push(message); continue;
    }
    const pending = message.parts.findIndex(part => part.kind === 'tool'
      && (part.tool.status === 'pending' || part.tool.status === 'waiting_approval'));
    const safeEnd = pending < 0 ? message.parts.length - 1 : pending;
    let start = 0;
    while (start + SEGMENT_PARTS <= safeEnd) {
      result.push({ ...message, ts: undefined, parts: message.parts.slice(start, start + SEGMENT_PARTS) });
      start += SEGMENT_PARTS;
      changed = true;
    }
    result.push(start ? { ...message, parts: message.parts.slice(start) } : message);
  }
  return changed ? result : messages;
}

/** Only small labels remain resident. Full search/copy reads the archived payload. */
export function archivedMessage<T extends TimelineMessage>(message: T, archiveKey: string): T {
  const text = message.parts.filter(part => part.kind === 'text').map(part => part.text.slice(0, 240)).join('').slice(0, 240);
  return { ...message, archiveKey, archiveBytes: timelineWeight(message), parts: text ? [{ kind: 'text', text }] : [],
    ...('images' in message ? { images: undefined } : {}) };
}

export function timelineRange(count: number, size: number, start: number | null) {
  const from = start === null ? Math.max(0, count - size) : Math.max(0, Math.min(start, Math.max(0, count - 1)));
  return { start: from, end: Math.min(count, from + size) };
}

export function assistantTurnRange(messages: readonly TimelineMessage[], index: number) {
  let start = index;
  while (start > 0 && messages[start - 1].role !== 'user') start -= 1;
  let end = index + 1;
  while (end < messages.length && messages[end].role !== 'user') end += 1;
  return { start, end };
}

/** A tiny LRU for inactive sessions. Eviction only removes a display cache;
 * revisiting the session uses its authoritative backend history. */
export function cacheSession<K, V>(cache: Map<K, V>, key: K, value: V, limit = 3) {
  cache.delete(key); cache.set(key, value);
  while (cache.size > limit) cache.delete(cache.keys().next().value!);
}

/** Late archive commits must never replace a newer snapshot or mutable tail. */
export function replaceArchived<T extends TimelineMessage>(current: T[], entries: readonly { key: string; value: T }[],
  summarize: (message: T, key: string) => T = archivedMessage): T[] {
  const replacements = new Map(entries.map(entry => [entry.value, summarize(entry.value, entry.key)]));
  return current.map(message => replacements.get(message) ?? message);
}

/** Approximate retained UTF-16 payload size; no stringify or text copies. */
export function timelineWeight(message: TimelineMessage): number {
  return 256 + message.parts.reduce((bytes, part) => bytes + 96 + 2 * (part.kind === 'tool'
    ? part.tool.args.length + (part.tool.output?.length ?? 0) + (part.tool.progress?.length ?? 0)
    : part.text.length), 0) + (message.images?.reduce((bytes, image) => bytes + image.data.length * 2, 0) ?? 0);
}

/** Always keep the mutable last segment, even for one exceptionally large event. */
export function residentStart(messages: readonly TimelineMessage[], count: number, budget: number): number {
  let start = Math.max(0, messages.length - count);
  let bytes = 0;
  for (let i = messages.length - 1; i >= start; i--) {
    const weight = timelineWeight(messages[i]);
    if (i < messages.length - 1 && bytes + weight > budget) { start = i + 1; break; }
    bytes += weight;
  }
  return start;
}

export function boundedTimelineRange(messages: readonly TimelineMessage[], size: number, start: number | null, budget: number) {
  const range = timelineRange(messages.length, size, start);
  let bytes = 0;
  if (start === null) {
    for (let i = range.end - 1; i >= range.start; i--) {
      const weight = messages[i].archiveBytes ?? timelineWeight(messages[i]);
      if (i < range.end - 1 && bytes + weight > budget) { range.start = i + 1; break; }
      bytes += weight;
    }
  } else {
    for (let i = range.start; i < range.end; i++) {
      const weight = messages[i].archiveBytes ?? timelineWeight(messages[i]);
      if (i > range.start && bytes + weight > budget) { range.end = i; break; }
      bytes += weight;
    }
  }
  return range;
}

/** Bound each structured-clone transaction as well as the resident cache. */
export function archiveCandidates<T extends TimelineMessage>(messages: readonly T[], end: number, budget = 1024 * 1024): T[] {
  const result: T[] = [];
  let bytes = 0;
  for (let index = 0; index < end; index++) {
    const message = messages[index];
    if (message.archiveKey) continue;
    const weight = timelineWeight(message);
    if (result.length && (result.length >= 64 || bytes + weight > budget)) break;
    result.push(message); bytes += weight;
  }
  return result;
}

/** Adjacent byte-bounded windows must never skip a heavy entry or move backwards
 * when fewer than `size` messages remain after the reading position. */
export function adjacentTimelineStart(messages: readonly TimelineMessage[], range: { start: number; end: number },
  direction: number, size: number, budget: number, minimum = 0, overlap = true): number {
  if (direction > 0) return overlap
    ? Math.min(range.end, range.start + Math.max(1, Math.floor((range.end - range.start) / 2))) : range.end;
  const shared = overlap ? Math.floor((range.end - range.start) / 2) : 0;
  let previous = boundedTimelineRange(messages.slice(0, range.start + shared), size, null, budget).start;
  if (previous >= range.start) previous = boundedTimelineRange(messages.slice(0, range.start), size, null, budget).start;
  return Math.max(minimum, previous);
}

/** Include the selected entry even if large payloads exhaust the page budget. */
export function targetTimelineStart(messages: readonly TimelineMessage[], index: number, size: number, budget: number): number {
  return boundedTimelineRange(messages.slice(0, index + 1), Math.max(1, Math.floor(size / 3) + 1), null, budget).start;
}
