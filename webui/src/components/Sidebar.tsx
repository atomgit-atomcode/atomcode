// Session sidebar (VSCode design system: list rows, violet new-session button).

import { useEffect, useState } from 'preact/hooks';
import { listSessions, SessionMetaWithProject } from '../api';

interface SidebarProps {
  activeSessionId: string | null;
  onSelect: (session: SessionMetaWithProject) => void;
  onNew: () => void;
  /** Mobile drawer open state */
  open?: boolean;
}

function baseName(p: string): string {
  const clean = p.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  return idx >= 0 ? clean.slice(idx + 1) : clean;
}

function shortDir(p: string): string {
  if (p.startsWith('/Users/') || p.startsWith('/home/')) {
    const parts = p.split('/');
    if (parts.length >= 3) {
      return '~/' + parts.slice(3).join('/');
    }
  }
  return p;
}

export function Sidebar({ activeSessionId, onSelect, onNew, open }: SidebarProps) {
  const [sessions, setSessions] = useState<SessionMetaWithProject[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    setLoading(true);
    listSessions()
      .then(setSessions)
      .catch(() => setSessions([]))
      .finally(() => setLoading(false));
  }, []);

  return (
    <aside class={'session-list app-sidebar' + (open ? ' open' : '')}>
      <div class="session-list-header">
        <button class="session-new-btn" onClick={onNew}>
          ＋ 新建会话
        </button>
      </div>

      <div class="session-group-label">最近会话</div>

      <div class="session-list-body">
        {loading && <div class="session-empty">加载中…</div>}
        {!loading && sessions.length === 0 && (
          <div class="session-empty">暂无会话</div>
        )}
        {sessions.map((s) => {
          const active = s.id === activeSessionId;
          const label = s.name || s.id.slice(0, 8);
          const dir = shortDir(s.working_dir);
          const dirBase = baseName(s.working_dir);
          return (
            <button
              key={s.id}
              class={'session-item' + (active ? ' active' : '')}
              onClick={() => onSelect(s)}
              title={dir}
            >
              <span class="session-item-name">{label}</span>
              <span class="session-item-meta">
                {dirBase} · {s.message_count} 条
              </span>
            </button>
          );
        })}
      </div>

      <div class="session-footer">
        {sessions.length > 0 ? `${sessions.length} 个会话` : '无会话记录'}
      </div>
    </aside>
  );
}
