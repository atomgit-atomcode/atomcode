import type { SessionMetaWithProject } from '../api';

export interface SessionReferenceToken { start: number; end: number; label: string; bucket: string; id: string }

export function sessionReferenceToken(session: Pick<SessionMetaWithProject, 'name' | 'project_hash' | 'id'>): string {
  const label = Array.from(session.name.replace(/[\[\]()@\x00-\x1f\x7f-\x9f]/g, '')).slice(0, 60).join('').trim();
  return `@[${label}](atomcode-session:${session.project_hash}/${session.id})`;
}

export function sessionReferenceTokens(text: string): SessionReferenceToken[] {
  const tokens: SessionReferenceToken[] = [];
  const pattern = /(^|\s)@\[([^\]]*)\]\(atomcode-session:([0-9a-fA-F]{16})\/([\w-]{1,128})\)/g;
  for (const m of text.matchAll(pattern)) {
    const start = m.index! + m[1].length;
    tokens.push({ start, end: m.index! + m[0].length, label: m[2], bucket: m[3], id: m[4] });
  }
  return tokens;
}

export function rankSessionReferences(sessions: SessionMetaWithProject[], cwd: string, current: string | null | undefined, currentBucket?: string): SessionMetaWithProject[] {
  const normalized = (path: string) => path.replace(/\\/g, '/').replace(/\/$/, '');
  const seen = new Set<string>();
  return sessions.filter(s => {
    const key = `${s.project_hash}/${s.id}`;
    if ((s.id === current && (!currentBucket || s.project_hash === currentBucket)) || seen.has(key)) return false;
    seen.add(key);
    return true;
  }).sort((a, b) => Number(normalized(b.working_dir) === normalized(cwd)) - Number(normalized(a.working_dir) === normalized(cwd)) || b.updated_at - a.updated_at).slice(0, 20);
}
