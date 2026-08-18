// Session sidebar (Claude-Code-inspired: brand + collapse at top, CC-style
// new-conversation row, live-filtered session list, settings at the bottom;
// collapses to an icon rail).

import { useEffect, useRef, useState } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import { listSessions, listProjectSessions, searchSessions, getSkills, getMcpStatus, postLiveMcpTrust, getSession, getProjects, SkillInfo, McpStatusInfo, SessionMetaWithProject, ProjectInfo } from '../api';
import { useT, useSettings, SettingsSection, Theme } from '../settings';
import { MsgKey, Lang } from '../i18n';
import { RenameDialog, DeleteDialog } from './SessionDialogs';
import { useAuth } from './LoginButton';
import { mergeOptimisticSession } from '../lib/sessionList';
import { sessionMessagesToMarkdownLines } from '../lib/historyMessages';
import { sidebarRelativeTime } from '../lib/sidebarTime';
import {
  loadSidebarViewMode,
  saveSidebarViewMode,
  sidebarProjectScopes,
  SidebarViewMode,
} from '../lib/sidebarView';

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
  /** Current working directory, for display + as the fallback session scope. */
  cwd?: string;
  /** Physical session-bucket hash of the current project. When set, the list is
   *  scoped to sessions in THIS bucket (robust against `working_dir` restamping);
   *  sessions lacking a hash fall back to matching `cwd`. Empty = scope by cwd. */
  projectHash?: string;
  /** Called after a session is renamed (id + new name). */
  onSessionRenamed?: (id: string, name: string) => void;
  /** Called after a session is deleted. */
  onSessionDeleted?: (id: string) => void;
  /** Pick a skill from the sidebar Skills menu → insert `/name ` into the chat input. */
  onPickSkill?: (name: string) => void;
  /** Open the remote-access dialog (moved from the old top header). */
  onOpenRemote?: () => void;
  /** Open the working-directory picker (to switch to / start a project in any dir). */
  onOpenCwd?: () => void;
}

type Translate = (key: MsgKey, params?: Record<string, string | number>) => string;

/** 把时间戳格式化为 YYYY-MM-DD（本地时区），供跨工作区搜索结果分组。 */
function dateKey(ts: number): string {
  const ms = ts < 1e12 ? ts * 1000 : ts;
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** 跨工作区搜索弹窗的友好日期标题。 */
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
function formatTime(ts: number, t: Translate, now = Date.now()): string {
  if (!ts) return '';
  const { unit, n } = sidebarRelativeTime(ts, now);
  if (unit === 'now') return t('sidebar.timeNow');
  const key = `sidebar.time${unit[0].toUpperCase()}${unit.slice(1)}` as MsgKey;
  return t(key, { n });
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

function dirName(p: string): string {
  const trimmed = (p || '').replace(/[\\/]+$/, '');
  return trimmed.split(/[\\/]/).pop() || trimmed;
}

function sidebarStorage(): Storage | null {
  try {
    return typeof window === 'undefined' ? null : window.localStorage;
  } catch {
    return null;
  }
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

/** Folder glyph for the project selector. */
function FolderIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M1.75 4c0-.55.45-1 1-1h3l1.5 1.5h5c.55 0 1 .45 1 1V12c0 .55-.45 1-1 1h-10c-.55 0-1-.45-1-1V4z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
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

/** Compact sliders glyph for choosing the sidebar list layout. */
function SlidersIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M2.5 4h11M2.5 8h11M2.5 12h11" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
      <circle cx="5" cy="4" r="1.35" fill="var(--app-sidebar-background)" stroke="currentColor" stroke-width="1.2" />
      <circle cx="10.5" cy="8" r="1.35" fill="var(--app-sidebar-background)" stroke="currentColor" stroke-width="1.2" />
      <circle cx="7" cy="12" r="1.35" fill="var(--app-sidebar-background)" stroke="currentColor" stroke-width="1.2" />
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

/** Bell glyph (completion notifications). */
function BellGlyph() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path d="M8 2.5c-2.2 0-3.8 1.7-3.8 3.8v2.2l-1.2 1.7h10l-1.2-1.7V6.3c0-2.1-1.6-3.8-3.8-3.8z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
      <path d="M6.4 11.8a1.7 1.7 0 0 0 3.2 0" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" />
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
  projectHash,
  onSessionRenamed,
  onSessionDeleted,
  onPickSkill,
  onOpenRemote,
  onOpenCwd,
}: SidebarProps) {
  const t = useT();
  const { theme, setTheme, lang, setLang } = useSettings();
  const auth = useAuth();
  // Bottom-bar login button's idle label: distinct copy when credentials are
  // present-but-dead vs signed-out. Shared by the tooltip and the visible text
  // (they only diverge while a login is in flight).
  const loginIdleLabel = auth.expired ? auth.labels.expired : auth.labels.signIn;
  const [sessions, setSessions] = useState<SessionMetaWithProject[]>([]);
  const [loading, setLoading] = useState(true);
  const sessionListBodyRef = useRef<HTMLDivElement | null>(null);
  const activeSessionItemRef = useRef<HTMLDivElement | null>(null);
  // Selection is an explicit navigation intent, unlike a background reload.
  // Keep it pending across a cross-project fetch until the destination row
  // actually exists; ordinary reloads must not steal the user's scroll.
  const pendingCenterSessionIdRef = useRef<string | null>(activeSessionId);
  const previousActiveSessionIdRef = useRef<string | null>(activeSessionId);
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [viewMode, setViewMode] = useState<SidebarViewMode>(() =>
    loadSidebarViewMode(sidebarStorage()),
  );
  const [expandedProjects, setExpandedProjects] = useState<Set<string>>(
    () => new Set(projectHash ? [projectHash] : []),
  );
  const [loadedProjects, setLoadedProjects] = useState<Set<string>>(new Set());
  const [loadingProjects, setLoadingProjects] = useState<Set<string>>(new Set());
  const [failedProjects, setFailedProjects] = useState<Set<string>>(new Set());
  const [viewMenuOpen, setViewMenuOpen] = useState(false);
  const viewMenuRef = useRef<HTMLDivElement | null>(null);
  // Relative labels (for example, 59min -> 1h) update without refetching sessions.
  const [timeNow, setTimeNow] = useState(() => Date.now());
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
  // Bounds the connecting-state poll loop (see the poll effect below).
  const mcpPollAttemptsRef = useRef(0);
  const [mcpMenuOpen, setMcpMenuOpen] = useState(false);
  const [mcpMenuPos, setMcpMenuPos] = useState<{ top: number; left: number; width: number } | null>(null);
  const mcpMenuRef = useRef<HTMLDivElement | null>(null);
  const mcpBtnRef = useRef<HTMLButtonElement | null>(null);
  // Trust flow: in-flight POST + error for the "Trust this project" button.
  const [trusting, setTrusting] = useState(false);
  const [trustError, setTrustError] = useState<string | null>(null);
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
  // Cross-project search results (the sidebar list itself is per-project, so the
  // search modal hits the dedicated all-projects endpoint instead of filtering
  // the loaded list — which would only ever see the current project).
  const [searchResults, setSearchResults] = useState<SessionMetaWithProject[]>([]);
  const [searchBusy, setSearchBusy] = useState(false);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const [renameTarget, setRenameTarget] = useState<SessionMetaWithProject | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<SessionMetaWithProject | null>(null);
  // Settings menu (3 entries → each opens its own dialog), fixed-anchored above the button.
  const [settingsMenuOpen, setSettingsMenuOpen] = useState(false);
  const [settingsSub, setSettingsSub] = useState<'theme' | 'language' | null>(null);
  const [settingsMenuPos, setSettingsMenuPos] = useState<{ left: number; bottom: number } | null>(null);
  const itemMenuRef = useRef<HTMLDivElement | null>(null);
  const settingsMenuRef = useRef<HTMLDivElement | null>(null);

  // Guards against out-of-order reloads and lazy bucket responses after a mode
  // switch. Workspace mode loads only expanded project buckets.
  const loadEpochRef = useRef(0);
  const projectRequestRef = useRef(new Map<string, number>());

  function replaceProjectSessions(hash: string, list: SessionMetaWithProject[]) {
    setSessions((current) => [
      ...current.filter((session) => session.project_hash !== hash),
      ...list,
    ]);
  }

  async function loadProjectBucket(hash: string, epoch = loadEpochRef.current) {
    const request = (projectRequestRef.current.get(hash) ?? 0) + 1;
    projectRequestRef.current.set(hash, request);
    setLoadingProjects((current) => new Set(current).add(hash));
    setFailedProjects((current) => {
      const next = new Set(current);
      next.delete(hash);
      return next;
    });
    try {
      const list = await listProjectSessions(hash);
      if (epoch !== loadEpochRef.current || projectRequestRef.current.get(hash) !== request) return;
      replaceProjectSessions(hash, list);
      setLoadedProjects((current) => new Set(current).add(hash));
      if (activeSessionId) {
        const found = list.find((session) => session.id === activeSessionId);
        if (found) onActiveSessionMeta?.(found);
      }
    } catch {
      if (epoch !== loadEpochRef.current || projectRequestRef.current.get(hash) !== request) return;
      setFailedProjects((current) => new Set(current).add(hash));
    } finally {
      if (epoch === loadEpochRef.current && projectRequestRef.current.get(hash) === request) {
        setLoadingProjects((current) => {
          const next = new Set(current);
          next.delete(hash);
          return next;
        });
      }
    }
  }

  function loadSessions() {
    const epoch = ++loadEpochRef.current;
    setLoading(true);
    setLoadingProjects(new Set());
    const load = viewMode === 'workspace'
      ? getProjects().then(async (nextProjects) => {
          if (epoch !== loadEpochRef.current) return [];
          setProjects(nextProjects);
          const normalizedCwd = (cwd || '').replace(/\/+$/, '');
          const currentHash = projectHash || nextProjects.find(
            (project) => project.working_dir.replace(/\/+$/, '') === normalizedCwd,
          )?.hash || '';
          const retainedExpanded = projectHash && !expandedProjects.has(projectHash)
            ? []
            : [...expandedProjects];
          const projectScopes = sidebarProjectScopes(
            'workspace',
            [currentHash, ...retainedExpanded],
          );
          setSessions([]);
          setLoadedProjects(new Set());
          setFailedProjects(new Set());
          await Promise.all(projectScopes.map((hash) => loadProjectBucket(hash, epoch)));
          return null;
        })
      : listSessions();
    load
      .then((list) => {
        if (epoch !== loadEpochRef.current) return; // superseded by a newer load
        if (list === null) return;
        setSessions(list);
        // 回合落盘后这次刷新会带出已自动命名的当前会话 → 回传给 App 更新标题头。
        if (activeSessionId) {
          const found = list.find((s) => s.id === activeSessionId);
          if (found) onActiveSessionMeta?.(found);
        }
      })
      .catch(() => {
        if (epoch !== loadEpochRef.current) return;
        setSessions([]);
        if (viewMode === 'workspace') {
          setProjects([]);
          setLoadedProjects(new Set());
        }
      })
      .finally(() => {
        if (epoch === loadEpochRef.current) setLoading(false);
      });
  }

  useEffect(() => {
    loadSessions();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reloadKey, viewMode, projectHash]);

  // A real project switch makes the new working directory the sole expanded
  // group. Other projects remain visible as collapsed peer rows.
  useEffect(() => {
    setExpandedProjects(new Set(projectHash ? [projectHash] : []));
  }, [projectHash]);

  useEffect(() => {
    if (activeSessionId !== previousActiveSessionIdRef.current) {
      pendingCenterSessionIdRef.current = activeSessionId;
      previousActiveSessionIdRef.current = activeSessionId;
    }
  }, [activeSessionId]);

  useEffect(() => {
    const pendingId = pendingCenterSessionIdRef.current;
    if (loading || collapsed || !pendingId || pendingId !== activeSessionId) return;
    const container = sessionListBodyRef.current;
    const item = activeSessionItemRef.current;
    if (!container || !item || !container.contains(item)) return;
    item.scrollIntoView({ behavior: 'smooth', block: 'center', inline: 'nearest' });
    pendingCenterSessionIdRef.current = null;
  }, [activeSessionId, collapsed, loading, sessions, expandedProjects]);

  // Flat mode does not need project buckets for rendering, but retaining the
  // project catalog keeps switching back to workspace immediate.
  useEffect(() => {
    if (viewMode === 'workspace') return;
    getProjects()
      .then(setProjects)
      .catch(() => setProjects([]));
  }, [reloadKey, viewMode]);

  useEffect(() => {
    if (!viewMenuOpen) return;
    const onDown = (e: MouseEvent) => {
      if (!viewMenuRef.current?.contains(e.target as Node)) setViewMenuOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    return () => document.removeEventListener('mousedown', onDown);
  }, [viewMenuOpen]);

  useEffect(() => {
    const timer = window.setInterval(() => setTimeNow(Date.now()), 60_000);
    return () => window.clearInterval(timer);
  }, []);

  function chooseViewMode(mode: SidebarViewMode) {
    setViewMode(mode);
    saveSidebarViewMode(mode, sidebarStorage());
    setViewMenuOpen(false);
  }

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
    // The DELETE response is authoritative for this row. Removing it locally
    // avoids an O(N) catalog reload after every deletion, which made clearing a
    // large history progressively slow. Invalidate any older in-flight list
    // response so it cannot resurrect the deleted row; normal reload triggers
    // still reconcile the complete list later.
    loadEpochRef.current += 1;
    setLoading(false);
    setSessions((current) => current.filter((session) => session.id !== id));
    onSessionDeleted?.(id);
  }

  /** Export a session's conversation as a Markdown file and trigger a browser download. */
  async function handleExportMarkdown(session: SessionMetaWithProject) {
    setMenuFor(null);
    try {
      const detail = await getSession(session.project_hash, session.id);
      const title = detail.name || detail.id.slice(0, 8);
      const lines = sessionMessagesToMarkdownLines(detail.messages, title);
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

  // Fetch MCP status (for the count badge + menu list). Unlike a one-shot cache,
  // this can re-run: remote servers (especially HTTP) connect asynchronously, so
  // an early snapshot can show them as `connecting` and must be refreshed until
  // they settle. Only the in-flight fetch is deduped (mcpLoading), not the result.
  function refreshMcpStatus() {
    if (mcpLoading) return;
    setMcpLoading(true);
    getMcpStatus()
      .then(setMcpStatus)
      // On a transient error keep whatever we had; only fall back to empty if we
      // never got a result, so a blip doesn't wipe a populated panel.
      .catch(() => setMcpStatus((cur) => cur ?? { servers: [] }))
      .finally(() => setMcpLoading(false));
  }
  useEffect(() => {
    refreshMcpStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  // Poll while any server is still `connecting` so the panel converges to the
  // settled state (connected / error) without the user reopening it. Servers
  // resolve to connected/failed within their timeout, so this stops on its own;
  // an attempt cap guards against a server that never settles.
  useEffect(() => {
    const anyConnecting = (mcpStatus?.servers ?? []).some((s) => s.status === 'connecting');
    if (!anyConnecting) {
      mcpPollAttemptsRef.current = 0;
      return;
    }
    if (mcpPollAttemptsRef.current >= 20) return; // ~30s ceiling
    const id = window.setTimeout(() => {
      mcpPollAttemptsRef.current += 1;
      refreshMcpStatus();
    }, 1500);
    return () => window.clearTimeout(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mcpStatus]);

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

  async function onTrust() {
    if (trusting) return;
    setTrusting(true);
    setTrustError(null);
    try {
      const result = await postLiveMcpTrust();
      if (result.ok) {
        // Refresh MCP status so the blocked notice disappears and new servers appear.
        mcpPollAttemptsRef.current = 0;
        refreshMcpStatus();
      } else {
        setTrustError(result.error ?? 'Trust failed');
      }
    } catch (e) {
      setTrustError(e instanceof Error ? e.message : 'Trust failed');
    } finally {
      setTrusting(false);
    }
  }

  function toggleMcpMenu(e: MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
    if (mcpMenuOpen) {
      setMcpMenuOpen(false);
      return;
    }
    // Reopening forces a fresh fetch (and restarts the connecting poll) so a
    // panel that was empty during the connect window picks up newly-ready servers.
    mcpPollAttemptsRef.current = 0;
    refreshMcpStatus();
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

  // Focus the input on open; reset query/results on close so a reopen starts
  // fresh instead of flashing the previous search's stale results.
  useEffect(() => {
    if (searchOpen) {
      searchInputRef.current?.focus();
    } else {
      setSearchQuery('');
      setSearchResults([]);
      setSearchBusy(false);
    }
  }, [searchOpen]);

  // Cross-project search: debounce the query and hit the all-projects endpoint.
  // Empty query → clear (the modal then shows the current project's list).
  useEffect(() => {
    if (!searchOpen) return;
    const q = searchQuery.trim();
    if (!q) {
      setSearchResults([]);
      setSearchBusy(false);
      return;
    }
    setSearchBusy(true);
    let cancelled = false;
    const timer = setTimeout(() => {
      searchSessions(q)
        .then((r) => {
          if (!cancelled) setSearchResults(r);
        })
        .catch(() => {
          if (!cancelled) setSearchResults([]);
        })
        .finally(() => {
          if (!cancelled) setSearchBusy(false);
        });
    }, 200);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [searchQuery, searchOpen]);

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
        {/* 完成通知：浏览器提醒设置，用弹窗 */}
        <button class="item-menu-row" onClick={() => chooseSettings('notifications')}>
          <BellGlyph />
          <span>{t('settings.menuNotifications')}</span>
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
            {!mcpLoading && (mcpStatus?.servers?.length ?? 0) === 0 && !(mcpStatus?.blocked?.length) && (
              <div class="group-by-menu-title">{t('sidebar.mcpEmpty')}</div>
            )}
            {!mcpLoading && (mcpStatus?.blocked?.length ?? 0) > 0 && (
              <div class="mcp-blocked-notice">
                <span>{t('mcp.blockedUntrusted').replace('{n}', String(mcpStatus!.blocked!.length))}</span>
                {trustError && <span class="mcp-blocked-error">{trustError}</span>}
                <button class="btn btn-primary" disabled={trusting} onClick={onTrust}>
                  {trusting ? '…' : t('mcp.trustProject')}
                </button>
              </div>
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

  // Workspace mode groups every uncapped project bucket. A just-created optimistic
  // row may not have its hash yet, so assign it to the current project by cwd/hash.
  const normDir = (p: string) => (p || '').replace(/\/+$/, '');
  const cwdNorm = normDir(cwd || '');
  const optimisticId = optimisticSession?.id;
  const sessionProjectHash = (s: SessionMetaWithProject): string => {
    if (s.project_hash) return s.project_hash;
    if (s.id === optimisticId && projectHash) return projectHash;
    const dir = normDir(s.working_dir);
    return projects.find((project) => normDir(project.working_dir) === dir)?.hash
      || (dir === cwdNorm ? projectHash || '' : '');
  };

  const filtered = merged;

  const sessionsByProject = new Map<string, SessionMetaWithProject[]>();
  for (const session of filtered) {
    const hash = sessionProjectHash(session);
    if (!hash) continue;
    const bucket = sessionsByProject.get(hash) ?? [];
    bucket.push(session);
    sessionsByProject.set(hash, bucket);
  }
  const workspaceProjects = projectHash && !projects.some((project) => project.hash === projectHash)
    ? [{
        hash: projectHash,
        name: dirName(cwd || ''),
        working_dir: cwd || '',
        session_count: sessionsByProject.get(projectHash)?.length ?? 0,
        created_at: 0,
        last_updated: 0,
      } as ProjectInfo, ...projects]
    : projects;
  const currentWorkspaceHash = projectHash || projects.find(
    (project) => normDir(project.working_dir) === cwdNorm,
  )?.hash || '';
  const currentProjectSessions = sessionsByProject.get(currentWorkspaceHash) ?? [];

  const toggleProject = (hash: string) => {
    setExpandedProjects((current) => {
      const next = new Set(current);
      if (next.has(hash)) next.delete(hash);
      else {
        next.add(hash);
        if (!loadedProjects.has(hash) && !loadingProjects.has(hash)) void loadProjectBucket(hash);
      }
      return next;
    });
  };

  const renderItem = (s: SessionMetaWithProject) => {
    const active = s.id === activeSessionId;
    const label = s.name || s.id.slice(0, 8);
    const dir = shortDir(s.working_dir);
    return (
      <div
        key={s.id}
        ref={active ? activeSessionItemRef : undefined}
        class={
          'session-item' +
          (active ? ' active' : '') +
          (menuFor === s.id ? ' menu-open' : '')
        }
      >
        <button class="session-item-main" onClick={() => onSelect(s)} title={dir}>
          <span class="session-item-name">{label}</span>
          <span class="session-item-meta">
            {formatTime(s.updated_at || s.created_at, t, timeNow)}
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
        <span class="session-group-label">
          {t(viewMode === 'workspace' ? 'sidebar.workspace' : 'sidebar.sessions')}
        </span>
        <span class="session-group-spacer" />
        <div class="group-by-control" ref={viewMenuRef}>
          <button
            class={'group-by-btn' + (viewMenuOpen ? ' on' : '')}
            onClick={() => setViewMenuOpen((open) => !open)}
            title={t('sidebar.groupBy')}
            aria-label={t('sidebar.groupBy')}
            aria-haspopup="menu"
            aria-expanded={viewMenuOpen}
          >
            <SlidersIcon />
          </button>
          {viewMenuOpen && (
            <div class="group-by-menu" role="menu">
              <div class="group-by-menu-title">{t('sidebar.groupBy')}</div>
              <button class="group-by-item" role="menuitemradio" aria-checked={viewMode === 'workspace'} onClick={() => chooseViewMode('workspace')}>
                <span>{t('sidebar.groupWorkspace')}</span>
                {viewMode === 'workspace' && <CheckIcon />}
              </button>
              <button class="group-by-item" role="menuitemradio" aria-checked={viewMode === 'flat'} onClick={() => chooseViewMode('flat')}>
                <span>{t('sidebar.groupFlat')}</span>
                {viewMode === 'flat' && <CheckIcon />}
              </button>
            </div>
          )}
        </div>
        {onOpenCwd && (
          <button
            class="group-by-btn"
            onClick={() => {
              setViewMenuOpen(false);
              onOpenCwd();
            }}
            title={t('sidebar.addWorkspace')}
            aria-label={t('sidebar.addWorkspace')}
          >
            <PlusIcon />
          </button>
        )}
      </div>

      <div
        class="session-list-body"
        ref={sessionListBodyRef}
        aria-busy={loading}
      >
        {loading && filtered.length === 0 && (viewMode === 'flat' || workspaceProjects.length === 0) && (
          <div class="session-empty">{t('sidebar.loading')}</div>
        )}
        {!loading && merged.length === 0 && (viewMode === 'flat' || workspaceProjects.length === 0) && (
          <div class="session-empty">{t(viewMode === 'flat' ? 'sidebar.empty' : 'sidebar.emptyInCwd')}</div>
        )}
        {viewMode === 'flat' && filtered.map(renderItem)}
        {viewMode === 'workspace' && workspaceProjects.map((project) => {
          const projectSessions = sessionsByProject.get(project.hash) ?? [];
          const expanded = expandedProjects.has(project.hash);
          const projectLoading = loadingProjects.has(project.hash);
          const projectFailed = failedProjects.has(project.hash);
          return (
            <section class="sidebar-project" key={project.hash}>
              <div class={'sidebar-project-row' + (project.hash === projectHash ? ' current' : '')}>
                <button
                  class="sidebar-project-expand"
                  onClick={() => toggleProject(project.hash)}
                  title={t(expanded ? 'sidebar.collapseWorkspace' : 'sidebar.expandWorkspace')}
                  aria-label={t(expanded ? 'sidebar.collapseWorkspace' : 'sidebar.expandWorkspace')}
                  aria-expanded={expanded}
                >
                  <span class={'sidebar-project-expand-icon' + (expanded ? ' open' : '')}>
                    <ChevronDownIcon />
                  </span>
                </button>
                <button
                  class="sidebar-project-btn"
                  onClick={() => toggleProject(project.hash)}
                  title={project.working_dir}
                  aria-expanded={expanded}
                >
                  <span class="sidebar-project-icon"><FolderIcon /></span>
                  <span class="sidebar-project-name">{project.name || dirName(project.working_dir)}</span>
                  <span class="sidebar-project-count">{project.session_count}</span>
                </button>
              </div>
              {expanded && (
                <div class="sidebar-project-sessions">
                  {projectLoading && projectSessions.length === 0 && (
                    <div class="session-empty">{t('sidebar.loading')}</div>
                  )}
                  {projectFailed && !projectLoading && (
                    <button class="sidebar-project-retry" onClick={() => void loadProjectBucket(project.hash)}>
                      {t('sidebar.loadFailedRetry')}
                    </button>
                  )}
                  {!projectLoading && !projectFailed && loadedProjects.has(project.hash) && projectSessions.length === 0 && (
                    <div class="session-empty">{t('sidebar.emptyInCwd')}</div>
                  )}
                  {projectSessions.map(renderItem)}
                </div>
              )}
            </section>
          );
        })}
      </div>

      <div class="sidebar-bottom">
        {auth.loggedIn && !auth.expired ? (
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
            class={auth.expired ? 'sidebar-account-btn sidebar-account-btn--expired' : 'sidebar-account-btn'}
            onClick={auth.startLogin}
            disabled={auth.busy}
            title={auth.busy ? auth.labels.hint : loginIdleLabel}
          >
            <AccountGlyph />
            <span class="login-name">{auth.busy ? auth.labels.signingIn : loginIdleLabel}</span>
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
                const q = searchQuery.trim();
                // Non-empty query → cross-project results from the endpoint;
                // empty → only the current project's already-loaded history.
                const results = q ? searchResults : currentProjectSessions;
                const currentProjectLoading = !q && loadingProjects.has(currentWorkspaceHash);
                if ((q && searchBusy && results.length === 0) || currentProjectLoading) {
                  return <div class="search-empty">{t('sidebar.searching')}</div>;
                }
                if (!q && failedProjects.has(currentWorkspaceHash)) {
                  return (
                    <button
                      class="sidebar-project-retry"
                      onClick={() => void loadProjectBucket(currentWorkspaceHash)}
                    >
                      {t('sidebar.loadFailedRetry')}
                    </button>
                  );
                }
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
                          <span class="search-result-time">{formatTime(s.updated_at || s.created_at, t, timeNow)}</span>
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
