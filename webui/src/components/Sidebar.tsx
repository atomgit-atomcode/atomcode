// Session sidebar (Claude-Code-inspired: brand + collapse at top, CC-style
// new-conversation row, live-filtered session list, settings at the bottom;
// collapses to an icon rail).

import { useEffect, useRef, useState } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import { listSessions, getSkills, getMcpStatus, getSession, SkillInfo, McpStatusInfo, SessionMetaWithProject } from '../api';
import { useT, useSettings, SettingsSection, Theme } from '../settings';
import { MsgKey, Lang } from '../i18n';
import { RenameDialog, DeleteDialog } from './SessionDialogs';
import { useAuth } from './LoginButton';
import { mergeOptimisticSession } from '../lib/sessionList';

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
  /** 乐观会话：首条消息发出瞬间即时插入列表（后端空会话不入列表）。
   *  一旦后端列表出现同 id 的真实会话，即被其覆盖（按 id 去重，真实条目优先）。 */
  optimisticSession?: SessionMetaWithProject | null;
  /** 列表（重新）加载后回传当前会话的真实元数据，供顶部标题头同步自动命名后的标题。 */
  onActiveSessionMeta?: (session: SessionMetaWithProject) => void;
  /** Only show sessions whose working_dir matches this path (empty = show all) */
  cwd?: string;
  /** Called after a session is renamed (id + new name). */
  onSessionRenamed?: (id: string, name: string) => void;
  /** Called after a session is deleted. */
  onSessionDeleted?: (id: string) => void;
  /** Pick a skill from the sidebar Skills menu → insert `/name ` into the chat input. */
  onPickSkill?: (name: string) => void;
  /** Open the remote-access dialog (moved from the old top header). */
  onOpenRemote?: () => void;
}

type Translate = (key: MsgKey, params?: Record<string, string | number>) => string;

/** 把时间戳格式化为 YYYY-MM-DD（本地时区），用作日期分组键。 */
function dateKey(ts: number): string {
  const ms = ts < 1e12 ? ts * 1000 : ts;
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** 友好日期分组标题：今天 / 昨天 / 本年内 MM-DD / 跨年 YYYY-MM-DD。 */
function friendlyDateLabel(key: string, t: Translate): string {
  const today = dateKey(Date.now());
  if (key === today) return t('sidebar.dateToday');
  const yMs = Date.now() - 86400000;
  if (key === dateKey(yMs)) return t('sidebar.dateYesterday');
  const [y, m, d] = key.split('-');
  const sameYear = String(new Date().getFullYear()) === y;
  return sameYear ? `${m}-${d}` : key;
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

/** Sparkles glyph for the Skills action. */
function SparklesIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
      <path d="M8 1.3l1.15 3.1a2 2 0 0 0 1.2 1.2L13.4 6.8l-3.05 1.15a2 2 0 0 0-1.2 1.2L8 12.2 6.85 9.15a2 2 0 0 0-1.2-1.2L2.6 6.8l3.05-1.2a2 2 0 0 0 1.2-1.2z" />
      <path d="M12.7 10.4l.5 1.35.5-1.35 1.35-.5-1.35-.5-.5-1.35-.5 1.35-1.35.5z" />
    </svg>
  );
}

/** Chevron-down glyph (for the Skills action's caret). */
function ChevronDownIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M4 6l4 4 4-4" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
    </svg>
  );
}

/** Smartphone glyph for the remote-access button. */
function SmartphoneIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <rect x="4.5" y="1.5" width="7" height="13" rx="1.5" stroke="currentColor" stroke-width="1.2" />
      <line x1="7" y1="12.3" x2="9" y2="12.3" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
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

/** Download / export glyph for the export-markdown menu item. */
function DownloadIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M8 2v8M4.5 7.5L8 10.5l3.5-3" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" />
      <path d="M2.5 12.5h11" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
    </svg>
  );
}

/** MCP / plug glyph (plug-connector for MCP servers). */
function McpIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M6.5 1.5v5a2.5 2.5 0 0 0 5 0v-5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" />
      <line x1="4" y1="9" x2="9" y2="9" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
      <path d="M4 9v4.5a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1V9" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" />
    </svg>
  );
}

/** Search (magnifying glass) glyph. */
function SearchIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="6.8" cy="6.8" r="4.3" stroke="currentColor" stroke-width="1.3" />
      <line x1="10" y1="10" x2="14" y2="14" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" />
    </svg>
  );
}

/** Gear / settings glyph. */
function GearIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M7.1 1l-.5 2.2a4.8 4.8 0 0 0-1.8 1L2.8 3.2 1.4 5.6l1.7 1.4a4.8 4.8 0 0 0 0 2l-1.7 1.4 1.4 2.4 2-.6a4.8 4.8 0 0 0 1.8 1l.5 2.2h2.8l.5-2.2a4.8 4.8 0 0 0 1.8-1l2 .6 1.4-2.4-1.7-1.4a4.8 4.8 0 0 0 0-2l1.7-1.4-1.4-2.4-2 .6a4.8 4.8 0 0 0-1.8-1L9.9 1z"
        stroke="currentColor"
        stroke-width="1.2"
        stroke-linejoin="round"
      />
      <circle cx="8" cy="8" r="2" stroke="currentColor" stroke-width="1.2" />
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

/** Account glyph (head + shoulders). */
function AccountGlyph() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="8" cy="5.5" r="2.5" stroke="currentColor" stroke-width="1.2" />
      <path d="M3.5 13.5a4.5 4.5 0 0 1 9 0" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
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
  optimisticSession,
  onActiveSessionMeta,
  cwd,
  onSessionRenamed,
  onSessionDeleted,
  onPickSkill,
  onOpenRemote,
}: SidebarProps) {
  const t = useT();
  const { theme, setTheme, lang, setLang } = useSettings();
  const auth = useAuth();
  const [sessions, setSessions] = useState<SessionMetaWithProject[]>([]);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState('');
  // Skills menu: list fetched lazily; the count badge shows once loaded.
  const [skills, setSkills] = useState<SkillInfo[] | null>(null);
  const [skillsLoading, setSkillsLoading] = useState(false);
  const [skillsMenuOpen, setSkillsMenuOpen] = useState(false);
  const [skillsMenuPos, setSkillsMenuPos] = useState<{ top: number; left: number; width: number } | null>(null);
  const skillsMenuRef = useRef<HTMLDivElement | null>(null);
  const skillsBtnRef = useRef<HTMLButtonElement | null>(null);
  // MCP menu: server list fetched lazily; the count badge shows once loaded.
  const [mcpStatus, setMcpStatus] = useState<McpStatusInfo | null>(null);
  const [mcpLoading, setMcpLoading] = useState(false);
  const [mcpMenuOpen, setMcpMenuOpen] = useState(false);
  const [mcpMenuPos, setMcpMenuPos] = useState<{ top: number; left: number; width: number } | null>(null);
  const mcpMenuRef = useRef<HTMLDivElement | null>(null);
  const mcpBtnRef = useRef<HTMLButtonElement | null>(null);
  // Per-session kebab menu: which session, and where to anchor the fixed menu.
  const [menuFor, setMenuFor] = useState<string | null>(null);
  // top XOR bottom: `top` opens the menu downward below the kebab; `bottom`
  // opens it upward above the kebab (used when there isn't room below, so the
  // 2-row menu isn't clipped by the viewport edge).
  const [menuPos, setMenuPos] = useState<
    { top?: number; bottom?: number; right: number } | null
  >(null);
  // Session search: a centered modal dialog (same .modal-overlay/.modal-card
  // family as the rename/delete dialogs), closes on backdrop click / Escape.
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState('');
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const [renameTarget, setRenameTarget] = useState<SessionMetaWithProject | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<SessionMetaWithProject | null>(null);
  // Settings menu (3 entries → each opens its own dialog), fixed-anchored above the button.
  const [settingsMenuOpen, setSettingsMenuOpen] = useState(false);
  const [settingsSub, setSettingsSub] = useState<'theme' | 'language' | null>(null);
  const [settingsMenuPos, setSettingsMenuPos] = useState<{ left: number; bottom: number } | null>(null);
  const itemMenuRef = useRef<HTMLDivElement | null>(null);
  const settingsMenuRef = useRef<HTMLDivElement | null>(null);

  function loadSessions() {
    setLoading(true);
    listSessions()
      .then((list) => {
        setSessions(list);
        // 回合落盘后这次刷新会带出已自动命名的当前会话 → 回传给 App 更新标题头。
        if (activeSessionId) {
          const found = list.find((s) => s.id === activeSessionId);
          if (found) onActiveSessionMeta?.(found);
        }
      })
      .catch(() => setSessions([]))
      .finally(() => setLoading(false));
  }

  useEffect(() => {
    loadSessions();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadKey]);

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

  function openItemMenu(e: MouseEvent, id: string) {
    e.preventDefault();
    e.stopPropagation();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (menuFor === id) {
      setMenuFor(null);
      return;
    }
    const right = window.innerWidth - rect.right;
    // 3 rows (rename + export + delete); estimate generously so we flip up a touch
    // early rather than let the last item clip off the bottom edge.
    const MENU_EST_HEIGHT = 144;
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

  /** Export a session's conversation as a Markdown file and trigger a browser download. */
  async function handleExportMarkdown(session: SessionMetaWithProject) {
    setMenuFor(null);
    try {
      const detail = await getSession(session.project_hash, session.id);
      const title = detail.name || detail.id.slice(0, 8);
      const lines: string[] = [];
      lines.push(`# ${title}`);
      lines.push('');
      for (const msg of detail.messages) {
        if (msg.role === 'system') continue;
        if (msg.role === 'user') {
          lines.push('## User');
          lines.push('');
          lines.push(msg.content || '');
          lines.push('');
        } else if (msg.role === 'assistant') {
          lines.push('## Assistant');
          lines.push('');
          if (msg.content) {
            lines.push(msg.content);
            lines.push('');
          }
          if (msg.tool_calls && msg.tool_calls.length > 0) {
            for (const tc of msg.tool_calls) {
              lines.push(`### Tool: ${tc.name}`);
              lines.push('');
              if (tc.arguments) {
                lines.push('```json');
                lines.push(typeof tc.arguments === 'string' ? tc.arguments : JSON.stringify(tc.arguments, null, 2));
                lines.push('```');
                lines.push('');
              }
            }
          }
        } else if (msg.role === 'tool' && msg.tool_result) {
          const tr = msg.tool_result;
          lines.push(`### Tool Result (${tr.success ? '✓' : '✗'})`);
          lines.push('');
          if (tr.summary) {
            lines.push(tr.summary);
            lines.push('');
          }
        }
      }
      const mdContent = lines.join('\n');
      // Trigger browser download
      const blob = new Blob([mdContent], { type: 'text/markdown;charset=utf-8' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `${title.replace(/[\\/:*?"<>|]/g, '_')}.md`;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
    } catch {
      alert(t('sidebar.exportFailed'));
    }
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

  // Fetch skills once (for the count badge + menu list).
  function ensureSkills() {
    if (skills !== null || skillsLoading) return;
    setSkillsLoading(true);
    getSkills()
      .then(setSkills)
      .catch(() => setSkills([]))
      .finally(() => setSkillsLoading(false));
  }
  useEffect(() => {
    ensureSkills();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Skills menu is fixed-positioned (so the action row's container can't clip
  // it); close on outside click / scroll / resize.
  // Scroll events originating inside the menu itself are ignored so the user
  // can scroll the list without the menu closing.
  useEffect(() => {
    if (!skillsMenuOpen) return;
    const close = () => setSkillsMenuOpen(false);
    const onDown = (e: MouseEvent) => {
      const el = e.target as HTMLElement;
      if (skillsMenuRef.current?.contains(el)) return;
      if (skillsBtnRef.current?.contains(el)) return;
      close();
    };
    const onScroll = (e: Event) => {
      if (skillsMenuRef.current?.contains(e.target as Node)) return;
      close();
    };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('resize', close);
    window.addEventListener('scroll', onScroll, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', onScroll, true);
    };
  }, [skillsMenuOpen]);

  function toggleSkillsMenu(e: MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
    if (skillsMenuOpen) {
      setSkillsMenuOpen(false);
      return;
    }
    ensureSkills();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    setSkillsMenuPos({ top: rect.bottom + 4, left: rect.left, width: rect.width });
    setSkillsMenuOpen(true);
  }

  function chooseSkill(name: string) {
    setSkillsMenuOpen(false);
    onPickSkill?.(name);
  }

  // Fetch MCP status once (for the count badge + menu list).
  function ensureMcpStatus() {
    if (mcpStatus !== null || mcpLoading) return;
    setMcpLoading(true);
    getMcpStatus()
      .then(setMcpStatus)
      .catch(() => setMcpStatus({ servers: [] }))
      .finally(() => setMcpLoading(false));
  }
  useEffect(() => {
    ensureMcpStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // MCP menu is fixed-positioned; close on outside click / scroll / resize.
  // Scroll events originating inside the menu itself are ignored so the user
  // can scroll the list without the menu closing.
  useEffect(() => {
    if (!mcpMenuOpen) return;
    const close = () => setMcpMenuOpen(false);
    const onDown = (e: MouseEvent) => {
      const el = e.target as HTMLElement;
      if (mcpMenuRef.current?.contains(el)) return;
      if (mcpBtnRef.current?.contains(el)) return;
      close();
    };
    const onScroll = (e: Event) => {
      if (mcpMenuRef.current?.contains(e.target as Node)) return;
      close();
    };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('resize', close);
    window.addEventListener('scroll', onScroll, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', onScroll, true);
    };
  }, [mcpMenuOpen]);

  function toggleMcpMenu(e: MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
    if (mcpMenuOpen) {
      setMcpMenuOpen(false);
      return;
    }
    ensureMcpStatus();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    setMcpMenuPos({ top: rect.bottom + 4, left: rect.left, width: rect.width });
    setMcpMenuOpen(true);
  }

  // Close the search dialog on Escape (backdrop click is handled on the overlay).
  useEffect(() => {
    if (!searchOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setSearchOpen(false);
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [searchOpen]);

  // Focus the input each time the dialog opens.
  useEffect(() => {
    if (searchOpen) searchInputRef.current?.focus();
  }, [searchOpen]);

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
        {/* 远程访问入口已移到侧栏底部栏（见下方 sidebar-bottom 的 Remote Btn）。 */}

        {/* 退出登录：放在组最下面，仅登录后显示（头像/登录入口在侧栏底部栏）。 */}
        {auth.loggedIn && (
          <>
            <div class="settings-menu-divider" />
            <button class="item-menu-row" onClick={auth.doLogout}>
              <AccountGlyph />
              <span>{auth.labels.signOut}</span>
            </button>
          </>
        )}
      </div>,
          document.body,
        )
      : null;

  // Skills popover (opens downward below the Skills action). Portaled to <body>
  // so the mobile drawer's transform/overflow can't clip it. Selecting a skill
  // inserts `/name ` into the chat input (via onPickSkill, lifted to App).
  const renderSkillsMenu = () =>
    skillsMenuOpen && skillsMenuPos
      ? createPortal(
          <div
            class="item-menu skills-menu"
            ref={skillsMenuRef}
            style={{
              top: `${skillsMenuPos.top}px`,
              left: `${skillsMenuPos.left}px`,
              minWidth: `${Math.max(skillsMenuPos.width, 200)}px`,
            }}
          >
            {skillsLoading && <div class="group-by-menu-title">{t('sidebar.skillsLoading')}</div>}
            {!skillsLoading && (skills?.length ?? 0) === 0 && (
              <div class="group-by-menu-title">{t('sidebar.skillsEmpty')}</div>
            )}
            {!skillsLoading &&
              (skills ?? []).map((s) => (
                <button key={s.name} class="skills-menu-row" onClick={() => chooseSkill(s.name)} title={s.description || ''}>
                  <span class="skills-menu-name">/{s.name}</span>
                  {s.description && <span class="skills-menu-desc">{s.description}</span>}
                </button>
              ))}
          </div>,
          document.body,
        )
      : null;

  // MCP popover (opens downward below the MCP action). Portaled to <body>
  // so the mobile drawer's transform/overflow can't clip it. Shows each
  // MCP server name, its connection status, and (if connected) tool count.
  const renderMcpMenu = () =>
    mcpMenuOpen && mcpMenuPos
      ? createPortal(
          <div
            class="item-menu mcp-menu"
            ref={mcpMenuRef}
            style={{
              top: `${mcpMenuPos.top}px`,
              left: `${mcpMenuPos.left}px`,
              minWidth: `${Math.max(mcpMenuPos.width, 220)}px`,
            }}
          >
            {mcpLoading && <div class="group-by-menu-title">{t('sidebar.mcpLoading')}</div>}
            {!mcpLoading && (mcpStatus?.servers?.length ?? 0) === 0 && (
              <div class="group-by-menu-title">{t('sidebar.mcpEmpty')}</div>
            )}
            {!mcpLoading &&
              (mcpStatus?.servers ?? []).map((srv) => {
                const statusLabel =
                  srv.status === 'connected' ? t('sidebar.mcpConnected') :
                  srv.status === 'connecting' ? t('sidebar.mcpConnecting') :
                  srv.status === 'error' ? t('sidebar.mcpError') :
                  t('sidebar.mcpDisconnected');
                const statusClass = 'mcp-status-dot ' + srv.status;
                return (
                  <div key={srv.name} class="mcp-menu-row" title={srv.error || ''}>
                    <span class="mcp-menu-name">{srv.name}</span>
                    <span class="mcp-menu-meta">
                      <span class={statusClass} />
                      <span class="mcp-menu-status-text">{statusLabel}</span>
                      {srv.tool_count != null && (
                        <span class="mcp-menu-tool-count">{t('sidebar.mcpTools', { n: srv.tool_count })}</span>
                      )}
                    </span>
                    {srv.error && <span class="mcp-menu-error">{srv.error}</span>}
                  </div>
                );
              })}
          </div>,
          document.body,
        )
      : null;

  // 乐观会话并入列表：仅当后端列表尚无对应条目时置顶插入。后端落盘后列表刷新带出
  // 真实会话，此处便不再插入，真实条目（含自动命名标题）自然取而代之。注意乐观条目
  // 与落盘条目的 id 可能不同（live 快照的会话 id ≠ /sessions 列出的 core .json id），
  // 故 mergeOptimisticSession 在 id 不匹配时按「同目录 + 名字互为前缀」兜底去重，
  // 避免出现两条相同会话（刷新才消失）。
  const merged = mergeOptimisticSession(optimisticSession ?? null, sessions);

  // 先按当前工作目录收窄，再按搜索词过滤。
  const normDir = (p: string) => (p || '').replace(/\/+$/, '');
  const cwdNorm = normDir(cwd || '');
  const inCwd = cwdNorm
    ? merged.filter((s) => normDir(s.working_dir) === cwdNorm)
    : merged;

  const q = query.trim().toLowerCase();
  const filtered = q
    ? inCwd.filter(
        (s) =>
          (s.name || '').toLowerCase().includes(q) ||
          s.id.toLowerCase().startsWith(q),
      )
    : inCwd;

  // 日期分组（始终开启，对齐设计：列表按 updated_at 倒序，同一天天然相邻，按日期切段）。
  const dateGroups: { key: string; items: SessionMetaWithProject[] }[] = [];
  for (const s of filtered) {
    const key = dateKey(s.updated_at || s.created_at);
    const last = dateGroups[dateGroups.length - 1];
    if (last && last.key === key) {
      last.items.push(s);
    } else {
      dateGroups.push({ key, items: [s] });
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
            onClick={onToggleCollapse}
            title={t('sidebar.skills')}
            aria-label={t('sidebar.skills')}
          >
            <SparklesIcon />
          </button>
          <button
            class="rail-btn"
            onClick={onToggleCollapse}
            title={t('sidebar.mcp')}
            aria-label={t('sidebar.mcp')}
          >
            <McpIcon />
          </button>
        </nav>
        <div class="sidebar-rail-bottom">
          {onOpenRemote && (
            <button
              class="rail-btn"
              onClick={onOpenRemote}
              title={t('settings.menuRemote')}
              aria-label={t('settings.menuRemote')}
            >
              <SmartphoneIcon />
            </button>
          )}
          <button
            class="rail-btn rail-btn-settings"
            onClick={(e) => toggleSettingsMenu(e as unknown as MouseEvent)}
            title={t('sidebar.settings')}
            aria-label={t('sidebar.settings')}
          >
            <GearIcon />
          </button>
        </div>
        {renderSettingsMenu()}
      </aside>
    );
  }

  const skillCount = skills?.length ?? 0;
  const mcpCount = mcpStatus?.servers?.length ?? 0;

  return (
    <aside class={'session-list app-sidebar' + (open ? ' open' : '')}>
      <div class="sidebar-brand-row">
        <span class="sidebar-brand">
<span class="sidebar-brand-name">AtomCode</span>
        </span>
        <span class="sidebar-brand-btns">
          <button
            class="sidebar-search-btn"
            onClick={() => { setSearchOpen(true); setSearchQuery(''); }}
            title={t('sidebar.search')}
            aria-label={t('sidebar.search')}
          >
            <SearchIcon />
          </button>
          <button
            class="sidebar-collapse-btn"
            onClick={onToggleCollapse}
            title={t('sidebar.collapse')}
            aria-label={t('sidebar.collapse')}
          >
            <PanelIcon />
          </button>
        </span>
      </div>

      <div class="sidebar-actions">
        <button class="sidebar-action" onClick={onNew}>
          <span class="sidebar-action-icon"><PlusIcon /></span>
          <span class="sidebar-action-label">{t('sidebar.newChat')}</span>
        </button>
        <button
          ref={skillsBtnRef}
          class={'sidebar-action' + (skillsMenuOpen ? ' open' : '')}
          onClick={(e) => toggleSkillsMenu(e as unknown as MouseEvent)}
          aria-haspopup="menu"
          aria-expanded={skillsMenuOpen}
        >
          <span class="sidebar-action-icon"><SparklesIcon /></span>
          <span class="sidebar-action-label">{t('sidebar.skills')}</span>
          {skillCount > 0 && <span class="sidebar-action-badge">{skillCount}</span>}
          <span class="sidebar-action-caret"><ChevronDownIcon /></span>
        </button>
        <button
          ref={mcpBtnRef}
          class={'sidebar-action' + (mcpMenuOpen ? ' open' : '')}
          onClick={(e) => toggleMcpMenu(e as unknown as MouseEvent)}
          aria-haspopup="menu"
          aria-expanded={mcpMenuOpen}
        >
          <span class="sidebar-action-icon"><McpIcon /></span>
          <span class="sidebar-action-label">{t('sidebar.mcp')}</span>
          {mcpCount > 0 && <span class="sidebar-action-badge">{mcpCount}</span>}
          <span class="sidebar-action-caret"><ChevronDownIcon /></span>
        </button>
      </div>

      <div class="session-group-header">
        <span class="session-group-label">{t('sidebar.recent')}</span>
      </div>

      <div class="session-list-body">
        {loading && <div class="session-empty">{t('sidebar.loading')}</div>}
        {!loading && inCwd.length === 0 && (
          <div class="session-empty">{t('sidebar.emptyInCwd')}</div>
        )}
        {!loading && inCwd.length > 0 && filtered.length === 0 && (
          <div class="session-empty">{t('sidebar.noMatch')}</div>
        )}
        {!loading &&
          dateGroups.map((g) => (
            <div key={g.key} class="session-date-group">
              <div class="session-date-label">{friendlyDateLabel(g.key, t)}</div>
              {g.items.map(renderItem)}
            </div>
          ))}
      </div>

      <div class="sidebar-bottom">
        {auth.loggedIn ? (
          <div
            class="sidebar-account"
            title={auth.user?.name || auth.user?.username || 'account'}
          >
            <span class="login-avatar">
              {auth.user?.avatar_url ? (
                <img
                  class="login-avatar-img"
                  src={auth.user.avatar_url}
                  alt=""
                  referrerpolicy="no-referrer"
                  onError={(e) => {
                    (e.currentTarget as HTMLImageElement).style.display = 'none';
                  }}
                />
              ) : (
                (auth.user?.name || auth.user?.username || 'A').slice(0, 1).toUpperCase()
              )}
            </span>
            <span class="login-name">{auth.user?.name || auth.user?.username || 'account'}</span>
          </div>
        ) : (
          <button
            class="sidebar-account-btn"
            onClick={auth.startLogin}
            disabled={auth.busy}
            title={auth.busy ? auth.labels.hint : auth.labels.signIn}
          >
            <AccountGlyph />
            <span class="login-name">{auth.busy ? auth.labels.signingIn : auth.labels.signIn}</span>
          </button>
        )}
        <span class="sidebar-bottom-spacer" />
        {onOpenRemote && (
          <button
            class="sidebar-icon-btn"
            onClick={onOpenRemote}
            title={t('settings.menuRemote')}
            aria-label={t('settings.menuRemote')}
          >
            <SmartphoneIcon />
          </button>
        )}
        <button
          class="sidebar-icon-btn sidebar-settings-btn"
          onClick={(e) => toggleSettingsMenu(e as unknown as MouseEvent)}
          title={t('sidebar.settings')}
          aria-label={t('sidebar.settings')}
        >
          <GearIcon />
        </button>
      </div>
      {renderSettingsMenu()}
      {renderSkillsMenu()}
      {renderMcpMenu()}

      {/* Session search dialog (centered modal) */}
      {searchOpen && createPortal(
        <div class="modal-overlay" onClick={(e) => { if (e.target === e.currentTarget) setSearchOpen(false); }}>
          <div class="modal-card search-modal-card">
            <div class="search-input-row">
              <SearchIcon />
              <input
                ref={searchInputRef}
                class="search-input"
                type="text"
                placeholder={t('sidebar.searchPlaceholder')}
                value={searchQuery}
                onInput={(e) => setSearchQuery((e.target as HTMLInputElement).value)}
                autofocus
              />
            </div>
            <div class="search-results">
              {(() => {
                const q = searchQuery.trim().toLowerCase();
                const results = q
                  ? sessions.filter(
                      (s) =>
                        (s.name || '').toLowerCase().includes(q) ||
                        s.id.toLowerCase().startsWith(q) ||
                        (s.working_dir || '').toLowerCase().includes(q),
                    )
                  : sessions;
                if (results.length === 0) {
                  return <div class="search-empty">{t('sidebar.noMatch')}</div>;
                }
                // 日期分组
                const groups: { key: string; items: typeof results }[] = [];
                for (const s of results) {
                  const key = dateKey(s.updated_at || s.created_at);
                  const last = groups[groups.length - 1];
                  if (last && last.key === key) {
                    last.items.push(s);
                  } else {
                    groups.push({ key, items: [s] });
                  }
                }
                return groups.map((g) => (
                  <div key={g.key} class="search-date-group">
                    <div class="search-date-label">{friendlyDateLabel(g.key, t)}</div>
                    {g.items.map((s) => {
                      const label = s.name || s.id.slice(0, 8);
                      const dir = shortDir(s.working_dir);
                      return (
                        <button
                          key={s.id}
                          class={'search-result-item' + (s.id === activeSessionId ? ' active' : '')}
                          onClick={() => { onSelect(s); setSearchOpen(false); }}
                        >
                          <span class="search-result-name">{label}</span>
                          <span class="search-result-dir">{dir}</span>
                          <span class="search-result-time">{formatTime(s.updated_at || s.created_at, t)}</span>
                        </button>
                      );
                    })}
                  </div>
                ));
              })()}
            </div>
          </div>
        </div>,
        document.body,
      )}

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
            class="item-menu-row"
            onClick={() => handleExportMarkdown(menuSession)}
          >
            <DownloadIcon />
            <span>{t('sidebar.exportMarkdown')}</span>
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
