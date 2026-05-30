// Session sidebar (Claude-Code-inspired: toolbar with collapse+search, CC-style
// new-conversation row, live-filtered session list; collapses to an icon rail).

import { useEffect, useRef, useState } from 'preact/hooks';
import { listSessions, SessionMetaWithProject } from '../api';

interface SidebarProps {
  activeSessionId: string | null;
  onSelect: (session: SessionMetaWithProject) => void;
  onNew: () => void;
  /** Mobile drawer open state */
  open?: boolean;
  /** Desktop collapsed (icon rail) state */
  collapsed?: boolean;
  /** Toggle the desktop collapsed/expanded state */
  onToggleCollapse?: () => void;
  /** Bump to force a session-list reload (e.g. after a new session is created) */
  reloadKey?: number;
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

/** Inline panel-collapse glyph (monochrome, uses currentColor). */
function PanelIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <rect x="1.5" y="2.5" width="13" height="11" rx="1.5" stroke="currentColor" stroke-width="1.2" />
      <line x1="6" y1="2.5" x2="6" y2="13.5" stroke="currentColor" stroke-width="1.2" />
    </svg>
  );
}

function PlusIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <line x1="8" y1="3" x2="8" y2="13" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
      <line x1="3" y1="8" x2="13" y2="8" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
    </svg>
  );
}

function SearchIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="7" cy="7" r="4.5" stroke="currentColor" stroke-width="1.3" />
      <line x1="10.5" y1="10.5" x2="14" y2="14" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" />
    </svg>
  );
}

export function Sidebar({
  activeSessionId,
  onSelect,
  onNew,
  open,
  collapsed,
  onToggleCollapse,
  reloadKey,
}: SidebarProps) {
  const [sessions, setSessions] = useState<SessionMetaWithProject[]>([]);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState('');
  const searchRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    setLoading(true);
    listSessions()
      .then(setSessions)
      .catch(() => setSessions([]))
      .finally(() => setLoading(false));
  }, [reloadKey]);

  const q = query.trim().toLowerCase();
  const filtered = q
    ? sessions.filter(
        (s) =>
          (s.name || '').toLowerCase().includes(q) ||
          s.id.toLowerCase().startsWith(q),
      )
    : sessions;

  // Rail (collapsed desktop): a narrow icon rail instead of hiding the sidebar.
  if (collapsed) {
    const expandAndSearch = () => {
      onToggleCollapse?.();
      // Focus the search input once it has rendered (next frame).
      requestAnimationFrame(() => searchRef.current?.focus());
    };
    return (
      <aside class="session-list app-sidebar collapsed">
        <nav class="sidebar-rail">
          <button
            class="rail-btn"
            onClick={onToggleCollapse}
            title="展开侧栏"
            aria-label="展开侧栏"
          >
            <PanelIcon />
          </button>
          <button class="rail-btn" onClick={onNew} title="新建对话" aria-label="新建对话">
            <PlusIcon />
          </button>
          <button
            class="rail-btn"
            onClick={expandAndSearch}
            title="搜索会话"
            aria-label="搜索会话"
          >
            <SearchIcon />
          </button>
        </nav>
      </aside>
    );
  }

  return (
    <aside class={'session-list app-sidebar' + (open ? ' open' : '')}>
      <div class="sidebar-toolbar">
        <button
          class="sidebar-collapse-btn"
          onClick={onToggleCollapse}
          title="收起侧栏"
          aria-label="收起侧栏"
        >
          <PanelIcon />
        </button>
        <input
          ref={searchRef}
          class="sidebar-search"
          type="text"
          placeholder="搜索会话…"
          value={query}
          onInput={(e) => setQuery((e.target as HTMLInputElement).value)}
        />
      </div>

      <div class="sidebar-new-row">
        <button class="sidebar-new-btn" onClick={onNew}>
          <PlusIcon />
          <span>新建对话</span>
        </button>
      </div>

      <div class="session-group-label">最近会话</div>

      <div class="session-list-body">
        {loading && <div class="session-empty">加载中…</div>}
        {!loading && sessions.length === 0 && (
          <div class="session-empty">暂无会话</div>
        )}
        {!loading && sessions.length > 0 && filtered.length === 0 && (
          <div class="session-empty">无匹配会话</div>
        )}
        {filtered.map((s) => {
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
        {sessions.length > 0
          ? q
            ? `${filtered.length} / ${sessions.length} 个会话`
            : `${sessions.length} 个会话`
          : '无会话记录'}
      </div>
    </aside>
  );
}
