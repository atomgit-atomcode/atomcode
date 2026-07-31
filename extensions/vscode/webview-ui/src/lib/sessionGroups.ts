import type { SessionMeta } from '../state/types';

export function sessionIdentity(session: SessionMeta): string {
  return `${session.project_hash || session.working_dir || 'unknown'}:${session.id}`;
}

export function filterSessionsForQuery(sessions: SessionMeta[], query: string): SessionMeta[] {
  const q = query.trim().toLowerCase();
  if (!q) return sessions;
  return sessions.filter((s) => {
    const fields = [
      s.name,
      s.title,
      s.id,
      s.project_hash,
      s.working_dir,
    ];
    return fields.some((field) => (field ?? '').toLowerCase().includes(q));
  });
}
