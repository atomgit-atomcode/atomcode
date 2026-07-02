// Compose the sidebar session list from the optimistic (client-only) entry and
// the server-fetched list. See sessionList.test.ts for the bug this guards.

export interface SessionLike {
  id: string;
  name: string;
  working_dir: string;
}

const normDir = (p: string): string => (p || '').replace(/\/+$/, '');

// Below this the optimistic title is too weak a signal to match on (short common
// openers like "你好"/"在吗" would suppress unrelated same-dir sessions). Such
// sessions fall back to id-only dedup.
const MIN_PREFIX_MATCH_LEN = 4;

// Does the persisted session `s` represent the same conversation as the
// `optimistic` entry? The two can carry DIFFERENT ids: the /live snapshot
// realigns the optimistic id to the LIVE session id (v2 capabilities store),
// while /sessions lists the core `.json` under its own id — so id-only dedup
// leaves a duplicate row. Fall back to a content match: same working dir AND the
// persisted auto-name EXTENDS the optimistic title (its first ~10 chars). The
// prefix check is ONE-DIRECTIONAL (persisted starts with optimistic) so a
// genuinely new session whose short title merely prefixes an existing longer
// name is not wrongly hidden.
function isSameSession(s: SessionLike, optimistic: SessionLike): boolean {
  if (s.id === optimistic.id) return true;
  if (normDir(s.working_dir) !== normDir(optimistic.working_dir)) return false;
  if (optimistic.name.length < MIN_PREFIX_MATCH_LEN) return false;
  return s.name.startsWith(optimistic.name);
}

// Merge the optimistic (client-only) entry into the server list: pin it to the
// top only while no persisted session already represents it.
export function mergeOptimisticSession<T extends SessionLike>(
  optimistic: T | null | undefined,
  sessions: T[],
): T[] {
  if (!optimistic) return sessions;
  return sessions.some((s) => isSameSession(s, optimistic))
    ? sessions
    : [optimistic, ...sessions];
}
