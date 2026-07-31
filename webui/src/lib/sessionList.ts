// Compose the sidebar session list from the optimistic (client-only) entry and
// the server-fetched list. See sessionList.test.ts for the bug this guards.

export interface SessionLike {
  id: string;
  name: string;
  working_dir: string;
  // Physical session-bucket hash. Preferred over `working_dir` for the
  // same-project check: `working_dir` is a mutable field the daemon can
  // restamp, whereas the bucket a session lives in is fixed.
  project_hash?: string;
}

const normDir = (p: string): string => (p || '').replace(/\/+$/, '');

// Below this an INEXACT (prefix) title match is too weak a signal (short common
// openers like "你好"/"在吗" would prefix-suppress unrelated sessions in the same
// project). Short titles must match EXACTLY instead; see isSameSession.
const MIN_PREFIX_MATCH_LEN = 4;

// Two entries belong to the same project when their physical buckets match. Fall
// back to comparing `working_dir` only when a hash is missing (e.g. a transitional
// optimistic entry created before the project hash was known).
function sameProject(a: SessionLike, b: SessionLike): boolean {
  if (a.project_hash && b.project_hash) return a.project_hash === b.project_hash;
  return normDir(a.working_dir) === normDir(b.working_dir);
}

// Does the persisted session `s` represent the same conversation as the
// `optimistic` entry? The two can carry DIFFERENT ids: the /live snapshot
// can realign the optimistic id before the persisted session list catches up,
// so id-only dedup can leave a duplicate row. Fall back to a content match
// within the SAME project:
//   - exact name → same session (this is what closes the short-name dup, e.g.
//     "你好", which was never merged before because it is under MIN_PREFIX_MATCH_LEN);
//   - otherwise the persisted auto-name must EXTEND the optimistic title (its
//     first ~10 chars). The prefix check is ONE-DIRECTIONAL (persisted starts
//     with optimistic) so a genuinely new session whose short title merely
//     prefixes an existing longer name is not wrongly hidden, and only applies
//     to titles long enough to be a distinctive signal.
function isSameSession(s: SessionLike, optimistic: SessionLike): boolean {
  if (s.id === optimistic.id) return true;
  if (!sameProject(s, optimistic)) return false;
  if (s.name === optimistic.name) return true;
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
