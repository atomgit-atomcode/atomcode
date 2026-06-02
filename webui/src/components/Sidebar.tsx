// Session sidebar (Claude-Code-inspired: brand + collapse at top, CC-style
// new-conversation row, live-filtered session list, settings at the bottom;
// collapses to an icon rail).

import { useEffect, useRef, useState } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import { listSessions, SessionMetaWithProject } from '../api';
import { useT, useSettings, SettingsSection, Theme } from '../settings';
import { MsgKey, Lang } from '../i18n';
import { RenameDialog, DeleteDialog } from './SessionDialogs';
import { LoginButton } from './LoginButton';

interface SidebarProps {
  activeSessionId: string | null;
  onSelect: (session: SessionMetaWithProject) => void;
  onNew: () => void;
  /** Open a specific settings dialog (theme / language / model). */
  onOpenSettings: (section: SettingsSection) => void;
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
  /** Called after a session is renamed (id + new name). */
  onSessionRenamed?: (id: string, name: string) => void;
  /** Called after a session is deleted. */
  onSessionDeleted?: (id: string) => void;
}

type Translate = (key: MsgKey, params?: Record<string, string | number>) => string;

type GroupBy = 'none' | 'date';

const GROUP_KEY = 'atomcode.sidebarGroupBy';

function readGroupBy(): GroupBy {
  try {
    const v = localStorage.getItem(GROUP_KEY);
    if (v === 'date' || v === 'none') return v;
  } catch {
    /* ignore */
  }
  return 'none';
}

/** 把时间戳格式化为 YYYY-MM-DD（本地时区），用作日期分组标题。 */
function dateKey(ts: number): string {
  const ms = ts < 1e12 ? ts * 1000 : ts;
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

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

/** Sliders / "group by" glyph (vertical faders). */
function SlidersIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <line x1="5" y1="2.5" x2="5" y2="13.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
      <line x1="11" y1="2.5" x2="11" y2="13.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
      <circle cx="5" cy="5.5" r="1.9" fill="var(--app-primary-background)" stroke="currentColor" stroke-width="1.2" />
      <circle cx="11" cy="10.5" r="1.9" fill="var(--app-primary-background)" stroke="currentColor" stroke-width="1.2" />
    </svg>
  );
}

/** Checkmark glyph for the active menu item. */
function CheckIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M3.5 8.5l3 3 6-7" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" />
    </svg>
  );
}

/** Kebab (vertical three-dot) glyph for the per-session menu. */
function KebabIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
      <circle cx="8" cy="3" r="1.4" />
      <circle cx="8" cy="8" r="1.4" />
      <circle cx="8" cy="13" r="1.4" />
    </svg>
  );
}

function PencilIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M10.8 2.6l2.6 2.6-7.3 7.3-3 .7.7-3z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
    </svg>
  );
}

function TrashIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M3 4.5h10M6.2 4.5V3h3.6v1.5M4.8 4.5l.6 8.5h5.2l.6-8.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" />
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

/** Theme glyph (half-filled circle = light/dark contrast). */
function ThemeGlyph() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="8" cy="8" r="5.5" stroke="currentColor" stroke-width="1.2" />
      <path d="M8 2.5a5.5 5.5 0 0 0 0 11z" fill="currentColor" />
    </svg>
  );
}

/** Language glyph (globe with meridians). */
function LangGlyph() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="8" cy="8" r="5.5" stroke="currentColor" stroke-width="1.2" />
      <path d="M2.6 8h10.8M8 2.5c2 2 2 9 0 11M8 2.5c-2 2-2 9 0 11" stroke="currentColor" stroke-width="1.2" />
    </svg>
  );
}

/** Model glyph (chip). */
function ModelGlyph() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <rect x="4" y="4" width="8" height="8" rx="1" stroke="currentColor" stroke-width="1.2" />
      <path d="M6.5 1.5v1.8M9.5 1.5v1.8M6.5 12.7v1.8M9.5 12.7v1.8M1.5 6.5h1.8M1.5 9.5h1.8M12.7 6.5h1.8M12.7 9.5h1.8" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" />
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
  onSessionRenamed,
  onSessionDeleted,
}: SidebarProps) {
  const t = useT();
  const { theme, setTheme, lang, setLang } = useSettings();
  const [sessions, setSessions] = useState<SessionMetaWithProject[]>([]);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState('');
  const [groupBy, setGroupBy] = useState<GroupBy>(readGroupBy);
  const [groupMenuOpen, setGroupMenuOpen] = useState(false);
  // Per-session kebab menu: which session, and where to anchor the fixed menu.
  const [menuFor, setMenuFor] = useState<string | null>(null);
  // top XOR bottom: `top` opens the menu downward below the kebab; `bottom`
  // opens it upward above the kebab (used when there isn't room below, so the
  // 2-row menu isn't clipped by the viewport edge).
  const [menuPos, setMenuPos] = useState<
    { top?: number; bottom?: number; right: number } | null
  >(null);
  const [renameTarget, setRenameTarget] = useState<SessionMetaWithProject | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<SessionMetaWithProject | null>(null);
  // Settings menu (3 entries → each opens its own dialog), fixed-anchored above the button.
  const [settingsMenuOpen, setSettingsMenuOpen] = useState(false);
  const [settingsSub, setSettingsSub] = useState<'theme' | 'language' | null>(null);
  const [settingsMenuPos, setSettingsMenuPos] = useState<{ left: number; bottom: number } | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);
  const groupRef = useRef<HTMLDivElement | null>(null);
  const itemMenuRef = useRef<HTMLDivElement | null>(null);
  const settingsMenuRef = useRef<HTMLDivElement | null>(null);

  function loadSessions() {
    setLoading(true);
    listSessions()
      .then(setSessions)
      .catch(() => setSessions([]))
      .finally(() => setLoading(false));
  }

  useEffect(() => {
    loadSessions();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadKey]);

  // Close the group-by menu on outside click.
  useEffect(() => {
    if (!groupMenuOpen) return;
    const h = (e: MouseEvent) => {
      if (groupRef.current && !groupRef.current.contains(e.target as Node)) {
        setGroupMenuOpen(false);
      }
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [groupMenuOpen]);

  // The kebab menu is fixed-positioned (so the list's overflow can't clip it);
  // close it on outside click, scroll, or resize since it won't track anchors.
  useEffect(() => {
    if (!menuFor) return;
    const close = () => setMenuFor(null);
    const onDown = (e: MouseEvent) => {
      const el = e.target as HTMLElement;
      if (itemMenuRef.current?.contains(el)) return;
      // Clicks on a kebab button are handled by its own toggle.
      if (el.closest?.('.session-item-kebab')) return;
      close();
    };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('resize', close);
    window.addEventListener('scroll', close, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', close, true);
    };
  }, [menuFor]);

  function selectGroupBy(g: GroupBy) {
    setGroupBy(g);
    try {
      localStorage.setItem(GROUP_KEY, g);
    } catch {
      /* ignore */
    }
    setGroupMenuOpen(false);
  }

  function openItemMenu(e: MouseEvent, id: string) {
    e.preventDefault();
    e.stopPropagation();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (menuFor === id) {
      setMenuFor(null);
      return;
    }
    const right = window.innerWidth - rect.right;
    // 2 rows (rename + delete); estimate generously so we flip up a touch
    // early rather than let the last item clip off the bottom edge.
    const MENU_EST_HEIGHT = 96;
    const spaceBelow = window.innerHeight - rect.bottom;
    if (spaceBelow < MENU_EST_HEIGHT + 8) {
      // Not enough room below → open upward, anchored above the kebab.
      setMenuPos({ bottom: window.innerHeight - rect.top + 4, right });
    } else {
      setMenuPos({ top: rect.bottom + 4, right });
    }
    setMenuFor(id);
  }

  function handleRenamed(id: string, name: string) {
    loadSessions();
    onSessionRenamed?.(id, name);
  }

  function handleDeleted(id: string) {
    loadSessions();
    onSessionDeleted?.(id);
  }

  // Settings menu: fixed-anchored above its button; close on outside/scroll/resize.
  useEffect(() => {
    if (!settingsMenuOpen) return;
    const close = () => setSettingsMenuOpen(false);
    const onDown = (e: MouseEvent) => {
      const el = e.target as HTMLElement;
      if (settingsMenuRef.current?.contains(el)) return;
      // Clicks on a settings trigger are handled by its own toggle.
      if (el.closest?.('.sidebar-settings-btn, .rail-btn-settings')) return;
      close();
    };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('resize', close);
    window.addEventListener('scroll', close, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', close, true);
    };
  }, [settingsMenuOpen]);

  function toggleSettingsMenu(e: MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
    if (settingsMenuOpen) {
      setSettingsMenuOpen(false);
      return;
    }
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    // Anchor the menu's bottom just above the button (it opens upward).
    setSettingsMenuPos({ left: rect.left, bottom: window.innerHeight - rect.top + 4 });
    setSettingsSub(null);
    setSettingsMenuOpen(true);
  }

  function chooseSettings(section: SettingsSection) {
    setSettingsMenuOpen(false);
    onOpenSettings(section);
  }

  // The settings popup (3 entries). Fixed-positioned, but portaled to <body>:
  // on mobile the sidebar becomes a `transform`ed off-canvas drawer, which
  // would make it the containing block for `position: fixed` descendants and
  // clip them with its `overflow: hidden`. Portaling escapes the drawer so the
  // menu is positioned against the viewport (its getBoundingClientRect coords)
  // and never clipped. Rendered inside both the rail and expanded layouts.
  const renderSettingsMenu = () =>
    settingsMenuOpen && settingsMenuPos
      ? createPortal(
      <div
        class="item-menu settings-menu"
        ref={settingsMenuRef}
        style={{ left: `${settingsMenuPos.left}px`, bottom: `${settingsMenuPos.bottom}px` }}
      >
        <div class="group-by-menu-title">{t('sidebar.settings')}</div>

        {/* 主题：选项少，直接内联展开二级菜单 */}
        <button
          class="item-menu-row"
          onClick={() => setSettingsSub((s) => (s === 'theme' ? null : 'theme'))}
        >
          <ThemeGlyph />
          <span>{t('settings.menuTheme')}</span>
          <span class="submenu-caret">{settingsSub === 'theme' ? '▾' : '▸'}</span>
        </button>
        {settingsSub === 'theme' && (
          <div class="settings-submenu">
            {(
              [
                ['light', t('settings.theme.light')],
                ['dark', t('settings.theme.dark')],
                ['system', t('settings.theme.system')],
              ] as [Theme, string][]
            ).map(([v, label]) => (
              <button key={v} class="item-menu-row sub" onClick={() => setTheme(v)}>
                <span>{label}</span>
                {theme === v && <CheckIcon />}
              </button>
            ))}
          </div>
        )}

        {/* 语言：同样内联展开 */}
        <button
          class="item-menu-row"
          onClick={() => setSettingsSub((s) => (s === 'language' ? null : 'language'))}
        >
          <LangGlyph />
          <span>{t('settings.menuLang')}</span>
          <span class="submenu-caret">{settingsSub === 'language' ? '▾' : '▸'}</span>
        </button>
        {settingsSub === 'language' && (
          <div class="settings-submenu">
            {(
              [
                ['zh', '中文'],
                ['en', 'English'],
              ] as [Lang, string][]
            ).map(([v, label]) => (
              <button key={v} class="item-menu-row sub" onClick={() => setLang(v)}>
                <span>{label}</span>
                {lang === v && <CheckIcon />}
              </button>
            ))}
          </div>
        )}

        {/* 模型配置：内容多，仍用弹窗 */}
        <button class="item-menu-row" onClick={() => chooseSettings('model')}>
          <ModelGlyph />
          <span>{t('settings.menuModel')}</span>
        </button>
        {/* 远程访问入口已移到顶栏右上角（见 app.tsx header-remote-btn）。 */}
      </div>,
          document.body,
        )
      : null;

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

  // 日期分组：列表本就按 updated_at 倒序，同一天的会话天然相邻，按日期切段即可。
  const dateGroups: { key: string; items: SessionMetaWithProject[] }[] = [];
  if (groupBy === 'date') {
    for (const s of filtered) {
      const key = dateKey(s.updated_at || s.created_at);
      const last = dateGroups[dateGroups.length - 1];
      if (last && last.key === key) {
        last.items.push(s);
      } else {
        dateGroups.push({ key, items: [s] });
      }
    }
  }

  const renderItem = (s: SessionMetaWithProject) => {
    const active = s.id === activeSessionId;
    const label = s.name || s.id.slice(0, 8);
    const dir = shortDir(s.working_dir);
    return (
      <div
        key={s.id}
        class={
          'session-item' +
          (active ? ' active' : '') +
          (menuFor === s.id ? ' menu-open' : '')
        }
      >
        <button class="session-item-main" onClick={() => onSelect(s)} title={dir}>
          <span class="session-item-name">{label}</span>
          <span class="session-item-meta">
            {formatTime(s.updated_at || s.created_at, t)}
          </span>
        </button>
        <button
          class="session-item-kebab"
          onClick={(e) => openItemMenu(e as unknown as MouseEvent, s.id)}
          title={t('sidebar.itemMenu')}
          aria-label={t('sidebar.itemMenu')}
        >
          <KebabIcon />
        </button>
      </div>
    );
  };

  const menuSession = menuFor
    ? filtered.find((s) => s.id === menuFor) ?? sessions.find((s) => s.id === menuFor) ?? null
    : null;

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
          onClick={(e) => toggleSettingsMenu(e as unknown as MouseEvent)}
          title={t('sidebar.settings')}
          aria-label={t('sidebar.settings')}
        >
          <GearIcon />
        </button>
        {renderSettingsMenu()}
      </aside>
    );
  }

  return (
    <aside class={'session-list app-sidebar' + (open ? ' open' : '')}>
      <div class="sidebar-brand-row">
        <span class="sidebar-brand">AtomCode</span>
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

      <div class="session-group-header">
        <span class="session-group-label">{t('sidebar.recent')}</span>
        <div class="group-by-control" ref={groupRef}>
          <button
            class={'group-by-btn' + (groupBy !== 'none' ? ' on' : '')}
            onClick={() => setGroupMenuOpen((o) => !o)}
            title={t('sidebar.group')}
            aria-label={t('sidebar.group')}
          >
            <SlidersIcon />
          </button>
          {groupMenuOpen && (
            <div class="group-by-menu">
              <div class="group-by-menu-title">{t('sidebar.groupBy')}</div>
              <button
                class={'group-by-item' + (groupBy === 'none' ? ' active' : '')}
                onClick={() => selectGroupBy('none')}
              >
                <span>{t('sidebar.groupNone')}</span>
                {groupBy === 'none' && <CheckIcon />}
              </button>
              <button
                class={'group-by-item' + (groupBy === 'date' ? ' active' : '')}
                onClick={() => selectGroupBy('date')}
              >
                <span>{t('sidebar.groupDate')}</span>
                {groupBy === 'date' && <CheckIcon />}
              </button>
            </div>
          )}
        </div>
      </div>

      <div class="session-list-body">
        {loading && <div class="session-empty">{t('sidebar.loading')}</div>}
        {!loading && inCwd.length === 0 && (
          <div class="session-empty">{t('sidebar.emptyInCwd')}</div>
        )}
        {!loading && inCwd.length > 0 && filtered.length === 0 && (
          <div class="session-empty">{t('sidebar.noMatch')}</div>
        )}
        {!loading && groupBy === 'date'
          ? dateGroups.map((g) => (
              <div key={g.key} class="session-date-group">
                <div class="session-date-label">{g.key}</div>
                {g.items.map(renderItem)}
              </div>
            ))
          : filtered.map(renderItem)}
      </div>

      <div class="sidebar-bottom">
        <LoginButton />
        <button
          class="sidebar-settings-btn"
          onClick={(e) => toggleSettingsMenu(e as unknown as MouseEvent)}
          title={t('sidebar.settings')}
          aria-label={t('sidebar.settings')}
        >
          <GearIcon />
          <span>{t('sidebar.settings')}</span>
        </button>
      </div>
      {renderSettingsMenu()}

      {/* Per-session actions menu. Fixed + portaled to <body> so neither the
          scroll container nor the mobile drawer's transform/overflow clips it. */}
      {menuSession && menuPos && createPortal(
        <div
          class="item-menu"
          ref={itemMenuRef}
          style={{
            top: menuPos.top != null ? `${menuPos.top}px` : undefined,
            bottom: menuPos.bottom != null ? `${menuPos.bottom}px` : undefined,
            right: `${menuPos.right}px`,
          }}
        >
          <button
            class="item-menu-row"
            onClick={() => {
              setRenameTarget(menuSession);
              setMenuFor(null);
            }}
          >
            <PencilIcon />
            <span>{t('sidebar.rename')}</span>
          </button>
          <button
            class="item-menu-row danger"
            onClick={() => {
              setDeleteTarget(menuSession);
              setMenuFor(null);
            }}
          >
            <TrashIcon />
            <span>{t('sidebar.delete')}</span>
          </button>
        </div>,
        document.body,
      )}

      {renameTarget && createPortal(
        <RenameDialog
          session={renameTarget}
          onClose={() => setRenameTarget(null)}
          onDone={(name) => handleRenamed(renameTarget.id, name)}
        />,
        document.body,
      )}
      {deleteTarget && createPortal(
        <DeleteDialog
          session={deleteTarget}
          onClose={() => setDeleteTarget(null)}
          onDone={() => handleDeleted(deleteTarget.id)}
        />,
        document.body,
      )}
    </aside>
  );
}
