import type { ProjectInfo, SessionMetaWithProject } from '../api.ts';

/** Pinning is explicit; current selection only leads the remaining workspaces. */
export function visibleWorkspaces<T extends Pick<ProjectInfo, 'hash' | 'pinned' | 'hidden'>>(projects: readonly T[], selected?: string): T[] {
  return projects.filter(p => !p.hidden).sort((a, b) =>
    Number(!!b.pinned) - Number(!!a.pinned) || Number(b.hash === selected) - Number(a.hash === selected));
}

/** Keep a confirmed mutation authoritative even when its follow-up GET fails. */
export function applyWorkspacePreference<T extends Pick<ProjectInfo, 'hash' | 'pinned' | 'hidden'>>(
  projects: readonly T[], hash: string, patch: { pinned?: boolean; hidden?: boolean }, fallback?: T,
): T[] {
  const known = projects.some(p => p.hash === hash);
  const source = !known && fallback ? [...projects, fallback] : projects;
  return source.map(p => p.hash === hash ? { ...p, ...patch } : p);
}

export class WorkspaceClearError extends Error {
  deleted: number;
  reason: unknown;
  busy: boolean;
  constructor(deleted: number, reason: unknown, busy = false) {
    super(reason instanceof Error ? reason.message : String(reason));
    this.deleted = deleted;
    this.reason = reason;
    this.busy = busy;
  }
}

/** Snapshot only; per-session API keeps all runtime and lease admission checks. */
export async function clearWorkspaceSessions(
  hash: string,
  running: string | null | undefined,
  onDeleted: (session: SessionMetaWithProject) => void,
  api: {
    list: (hash: string) => Promise<SessionMetaWithProject[]>;
    remove: (hash: string, id: string) => Promise<void>;
  },
): Promise<number> {
  const sessions = await api.list(hash);
  if (running && sessions.some(s => s.id === running)) {
    throw new WorkspaceClearError(0, 'This workspace has a running session.', true);
  }
  let deleted = 0;
  for (const session of sessions) {
    try { await api.remove(hash, session.id); }
    catch (cause) { throw new WorkspaceClearError(deleted, cause); }
    deleted++;
    onDeleted(session);
  }
  return deleted;
}
