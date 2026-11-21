import type { DisplayHistoryCursor } from '../api';

/** Keep already fetched pages only when the server certifies the same omitted
 * prefix and the fresh tail has a verified native user-event boundary. */
export function preservedHistoryPrefix<T extends { role: string; sourceSeq?: number }>(
  previous: readonly T[], incoming: readonly T[],
  baseline?: DisplayHistoryCursor, fresh?: DisplayHistoryCursor,
): T[] {
  if (!baseline || !fresh || baseline.session !== fresh.session
    || baseline.before !== fresh.before || baseline.digest !== fresh.digest) return [];
  const first = incoming.find(message => message.role === 'user');
  if (first?.sourceSeq == null) return [];
  const index = previous.findIndex(message => message.role === 'user' && message.sourceSeq === first.sourceSeq);
  return index > 0 ? previous.slice(0, index) : [];
}
