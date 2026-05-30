// Session sidebar (Claude-Code-inspired: brand + collapse at top, CC-style
// new-conversation row, live-filtered session list, settings at the bottom;
// collapses to an icon rail).

import { useEffect, useRef, useState } from 'preact/hooks';
import { listSessions, SessionMetaWithProject } from '../api';
import { useT } from '../settings';
import { MsgKey } from '../i18n';

interface SidebarProps {
  activeSessionId: string | null;
  onSelect: (session: SessionMetaWithProject) => void;
  onNew: () => void;
  /** Open the settings modal */
  onOpenSettings: () => void;
  /** Mobile drawer open state */
  open?: boolean;
  /** Desktop collapsed (icon rail) state */
  collapsed?: boolean;
  /** Toggle the desktop collapsed/expanded state */
  onToggleCollapse?: () => void;
  /** Bump to force a session-list reload (e.g. after a new session is created) */
  reloadKey?: number;
  /** Only show sessions whose working_dir matches this path (empty = show all) */
  cwd?: string;
}

function baseName(p: string): string {
  const clean = p.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  return idx >= 0 ? clean.slice(idx + 1) : clean;
}

type Translate = (key: MsgKey, params?: Record<string, string | number>) => string;

/** 把 unix 时间戳（秒或毫秒）格式化为相对时间。 */
function formatTime(ts: number, t: Translate): string {
  if (!ts) return '';
  const ms = ts < 1e12 ? ts * 1000 : ts;
  const now = Date.now();
  const diff = now - ms;
  const MIN = 60000, HOUR = 3600000, DAY = 86400000;
  if (diff < MIN) return t('time.justNow');
  if (diff < HOUR) return t('time.minutesAgo', { n: Math.floor(diff / MIN) });
  if (diff < DAY) return t('time.hoursAgo', { n: Math.floor(diff / HOUR) });
  if (diff < 2 * DAY) return t('time.yesterday');
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

/** Gear / settings glyph. */
function GearIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="8" cy="8" r="2.2" stroke="currentColor" stroke-width="1.2" />
      <path
        d="M8 1.5v1.6M8 12.9v1.6M14.5 8h-1.6M3.1 8H1.5M12.6 3.4l-1.1 1.1M4.5 11.5l-1.1 1.1M12.6 12.6l-1.1-1.1M4.5 4.5L3.4 3.4"
        stroke="currentColor"
        stroke-width="1.2"
        stroke-linecap="round"
      />
    </svg>
  );
}

export function Sidebar({
  activeSessionId,
  onSelect,
  onNew,
  onOpenSettings,
  open,
  collapsed,
  onToggleCollapse,
  reloadKey,
  cwd,
}: SidebarProps) {
  const t = useT();
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

  // 先按当前工作目录收窄，再按搜索词过滤。
  const normDir = (p: string) => (p || '').replace(/\/+$/, '');
  const cwdNorm = normDir(cwd || '');
  const inCwd = cwdNorm
    ? sessions.filter((s) => normDir(s.working_dir) === cwdNorm)
    : sessions;

  const q = query.trim().toLowerCase();
  const filtered = q
    ? inCwd.filter(
        (s) =>
          (s.name || '').toLowerCase().includes(q) ||
          s.id.toLowerCase().startsWith(q),
      )
    : inCwd;

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
            title={t('sidebar.expand')}
            aria-label={t('sidebar.expand')}
          >
            <PanelIcon />
          </button>
          <button
            class="rail-btn"
            onClick={onNew}
            title={t('sidebar.newChat')}
            aria-label={t('sidebar.newChat')}
          >
            <PlusIcon />
          </button>
          <button
            class="rail-btn"
            onClick={expandAndSearch}
            title={t('sidebar.search')}
            aria-label={t('sidebar.search')}
          >
            <SearchIcon />
          </button>
        </nav>
        <button
          class="rail-btn rail-btn-settings"
          onClick={onOpenSettings}
          title={t('sidebar.settings')}
          aria-label={t('sidebar.settings')}
        >
          <GearIcon />
        </button>
      </aside>
    );
  }

  return (
    <aside class={'session-list app-sidebar' + (open ? ' open' : '')}>
      <div class="sidebar-brand-row">
        <span class="sidebar-brand">atomcode</span>
        <button
          class="sidebar-collapse-btn"
          onClick={onToggleCollapse}
          title={t('sidebar.collapse')}
          aria-label={t('sidebar.collapse')}
        >
          <PanelIcon />
        </button>
      </div>

      <div class="sidebar-new-row">
        <button class="sidebar-new-btn" onClick={onNew}>
          <PlusIcon />
          <span>{t('sidebar.newChat')}</span>
        </button>
      </div>

      <div class="sidebar-search-row">
        <input
          ref={searchRef}
          class="sidebar-search"
          type="text"
          placeholder={t('sidebar.searchPlaceholder')}
          value={query}
          onInput={(e) => setQuery((e.target as HTMLInputElement).value)}
        />
      </div>

      <div class="session-group-label">{t('sidebar.recent')}</div>

      <div class="session-list-body">
        {loading && <div class="session-empty">{t('sidebar.loading')}</div>}
        {!loading && inCwd.length === 0 && (
          <div class="session-empty">{t('sidebar.emptyInCwd')}</div>
        )}
        {!loading && inCwd.length > 0 && filtered.length === 0 && (
          <div class="session-empty">{t('sidebar.noMatch')}</div>
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
                {formatTime(s.updated_at || s.created_at, t)} ·{' '}
                {t('sidebar.msgCount', { n: s.message_count })} · {dirBase}
              </span>
            </button>
          );
        })}
      </div>

      <div class="sidebar-bottom">
        <button
          class="sidebar-settings-btn"
          onClick={onOpenSettings}
          title={t('sidebar.settings')}
          aria-label={t('sidebar.settings')}
        >
          <GearIcon />
          <span>{t('sidebar.settings')}</span>
        </button>
        <span class="session-footer">
          {inCwd.length > 0
            ? q
              ? t('sidebar.sessionCountFiltered', { f: filtered.length, t: inCwd.length })
              : t('sidebar.sessionCount', { n: inCwd.length })
            : t('sidebar.noRecords')}
        </span>
      </div>
    </aside>
  );
}
