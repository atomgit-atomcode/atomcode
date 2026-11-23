import type { GitFileChange, GitPathSelection } from '../api';

/** What a file's status row means on the side being shown. */
export type StatusKind =
  | 'added'
  | 'modified'
  | 'deleted'
  | 'renamed'
  | 'copied'
  | 'untracked'
  | 'conflicted';

/** One row of the source-control list, already split for display. */
export type GitEntry = {
  path: string;
  name: string;
  dir: string;
  kind: StatusKind;
  original?: string;
  additions: number;
  deletions: number;
};

const CONFLICT_LETTERS = new Set(['U']);

/** Unmerged rows: either side says U, or both sides say A/D. */
export function isConflicted(change: GitFileChange): boolean {
  if (CONFLICT_LETTERS.has(change.index) || CONFLICT_LETTERS.has(change.worktree)) {
    return true;
  }
  return (
    (change.index === 'A' && change.worktree === 'A') ||
    (change.index === 'D' && change.worktree === 'D')
  );
}

/** `??` is the only status letter that means "not tracked yet". */
export function isUntracked(change: GitFileChange): boolean {
  return change.index === '?';
}

export function statusKind(letter: string, untracked = false): StatusKind {
  if (untracked) return 'untracked';
  switch (letter) {
    case 'A':
      return 'added';
    case 'D':
      return 'deleted';
    case 'R':
      return 'renamed';
    case 'C':
      return 'copied';
    default:
      return 'modified';
  }
}

/** The single letter shown in the status badge. */
export function statusLetter(kind: StatusKind): string {
  switch (kind) {
    case 'added':
      return 'A';
    case 'deleted':
      return 'D';
    case 'renamed':
      return 'R';
    case 'copied':
      return 'C';
    case 'untracked':
      return 'U';
    case 'conflicted':
      return '!';
    default:
      return 'M';
  }
}

export function splitPath(path: string): { dir: string; name: string } {
  const at = path.lastIndexOf('/');
  return at < 0 ? { dir: '', name: path } : { dir: path.slice(0, at), name: path.slice(at + 1) };
}

export function entryOf(change: GitFileChange, side: 'index' | 'worktree'): GitEntry {
  const { dir, name } = splitPath(change.path);
  const letter = side === 'index' ? change.index : change.worktree;
  return {
    path: change.path,
    name,
    dir,
    kind: isConflicted(change)
      ? 'conflicted'
      : statusKind(letter, side === 'worktree' && isUntracked(change)),
    original: change.original ?? undefined,
    additions: change.stats?.additions ?? 0,
    deletions: change.stats?.deletions ?? 0,
  };
}

export type GitPartition = { staged: GitEntry[]; unstaged: GitEntry[] };

/**
 * Split `git status` rows into the two source-control sections. A file can
 * appear in both (`MM`): the index side is what a commit would take, the
 * worktree side is what is still unstaged. Conflicted rows stay in one place
 * so the user never sees the same conflict twice.
 */
export function partitionChanges(changes: GitFileChange[]): GitPartition {
  const staged: GitEntry[] = [];
  const unstaged: GitEntry[] = [];
  for (const change of changes) {
    if (isConflicted(change)) {
      unstaged.push(entryOf(change, 'worktree'));
      continue;
    }
    if (change.index !== ' ' && change.index !== '?') {
      staged.push(entryOf(change, 'index'));
    }
    if (change.worktree !== ' ') {
      unstaged.push(entryOf(change, 'worktree'));
    }
  }
  return { staged, unstaged };
}

export type GitGroup = { id: string; label: string; entries: GitEntry[] };

/**
 * Group rows by their first directory segment, the way VSCode folds a change
 * list into folders. Root-level files come last so a long diff of nested work
 * does not bury them.
 */
export function groupByDirectory(entries: GitEntry[]): GitGroup[] {
  const groups = new Map<string, GitEntry[]>();
  for (const entry of entries) {
    const id = entry.dir === '' ? '' : entry.dir.split('/')[0];
    const bucket = groups.get(id);
    if (bucket) bucket.push(entry);
    else groups.set(id, [entry]);
  }
  return [...groups.entries()]
    .map(([id, rows]) => ({ id, label: id || '.', entries: rows }))
    .sort((a, b) => {
      if (a.id === '') return 1;
      if (b.id === '') return -1;
      return a.id.localeCompare(b.id);
    });
}

export function sumStats(entries: GitEntry[]): { additions: number; deletions: number } {
  return entries.reduce(
    (total, entry) => ({
      additions: total.additions + entry.additions,
      deletions: total.deletions + entry.deletions,
    }),
    { additions: 0, deletions: 0 },
  );
}

/** What a write call must send to address exactly these rows. */
export function selectionsOf(entries: GitEntry[]): GitPathSelection[] {
  return entries.map((entry) => ({
    path: entry.path,
    original: entry.original ?? null,
    untracked: entry.kind === 'untracked',
  }));
}

export const COMMIT_MESSAGE_LIMIT = 4000;

export type CommitMessageProblem = 'empty' | 'too-long';

/** Mirrors the daemon's commit check so the button disables before a round trip. */
export function commitMessageProblem(message: string): CommitMessageProblem | null {
  const text = message.trim();
  if (!text) return 'empty';
  if ([...text].length > COMMIT_MESSAGE_LIMIT) return 'too-long';
  return null;
}

/** Compact relative time for the history list; `now` is passed in so it stays testable. */
export function relativeTime(at: number, now: number, zh: boolean): string {
  const seconds = Math.max(0, Math.round(now - at));
  if (seconds < 60) return zh ? '刚刚' : 'just now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return zh ? `${minutes} 分钟前` : `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return zh ? `${hours} 小时前` : `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days < 30) return zh ? `${days} 天前` : `${days} d ago`;
  const date = new Date(at * 1000);
  const pad = (value: number) => String(value).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** A commit subject is one line: anything the history row can show unwrapped. */
export function commitSubject(subject: string): string {
  const line = subject.split('\n')[0].trim();
  return line || '(no message)';
}
