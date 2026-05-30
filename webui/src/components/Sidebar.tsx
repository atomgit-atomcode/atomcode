// Session sidebar (VSCode design system: list rows, violet new-session button).

import { useEffect, useState } from 'preact/hooks';
import { listSessions, SessionMetaWithProject } from '../api';

interface SidebarProps {
  activeSessionId: string | null;
  onSelect: (session: SessionMetaWithProject) => void;
  onNew: () => void;
  /** Mobile drawer open state */
  open?: boolean;
  /** Desktop collapsed (hidden) state */
  collapsed?: boolean;
}

function baseName(p: string): string {
  const clean = p.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  return idx >= 0 ? clean.slice(idx + 1) : clean;
}

/** 把 unix 时间戳（秒或毫秒）格式化为相对时间。 */
function formatTime(ts: number): string {
  if (!ts) return '';
  const ms = ts < 1e12 ? ts * 1000 : ts;
  const now = Date.now();
  const diff = now - ms;
  const MIN = 60000, HOUR = 3600000, DAY = 86400000;
  if (diff < MIN) return '刚刚';
  if (diff < HOUR) return `${Math.floor(diff / MIN)}分钟前`;
  if (diff < DAY) return `${Math.floor(diff / HOUR)}小时前`;
  if (diff < 2 * DAY) return '昨天';
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  const sameYear = d.getFullYear() === new Date(now).getFullYear();
  const md = `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  return sameYear ? md : `${d.getFullYear()}-${md}`;
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

export function Sidebar({ activeSessionId, onSelect, onNew, open, collapsed }: SidebarProps) {
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
    <aside class={'session-list app-sidebar' + (open ? ' open' : '') + (collapsed ? ' collapsed' : '')}>
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
                {formatTime(s.updated_at || s.created_at)} · {s.message_count} 条 · {dirBase}
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
