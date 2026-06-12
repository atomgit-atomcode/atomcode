// Two-column layout: sidebar + chat, header with cwd breadcrumb + config.
// VSCode design system: timeline messages, violet brand, floating input.

import { useEffect, useRef, useState } from 'preact/hooks';
import { Chat } from './components/Chat';
import { Sidebar } from './components/Sidebar';
import { ThemeDialog, LanguageDialog, ModelConfigDialog, RemoteAccessDialog } from './components/SettingsDialogs';
import { RenameDialog, DeleteDialog } from './components/SessionDialogs';
import { CwdPicker } from './components/CwdPicker';
import { PermissionCard } from './components/PermissionCard';
import { resolvePendingAfterDecision } from './lib/pendingPermission';
import { getProject, listSessions, SessionMetaWithProject } from './api';
import { useT, SettingsSection } from './settings';

function cwdDisplay(cwd: string): { prefix: string; name: string } {
  const clean = cwd.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  if (idx < 0) return { prefix: '', name: cwd };
  return { prefix: clean.slice(0, idx + 1), name: clean.slice(idx + 1) };
}

// 从 URL (?session=<id>) 读取要打开的会话 id（短 id），用于刷新后恢复。
function readSessionIdFromUrl(): string | null {
  try {
    return new URLSearchParams(window.location.search).get('session');
  } catch {
    return null;
  }
}

// URL 里只放 UUID 前 8 位以缩短地址；刷新时按前缀在会话列表里还原成完整 id。
function shortSessionId(id: string): string {
  return id.slice(0, 8);
}

export function App() {
  const t = useT();
  // URL 里是短 id，不能直接当完整 id 用（后端需要完整 id）；先置空，挂载后再还原。
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [activeSession, setActiveSession] = useState<SessionMetaWithProject | null>(null);
  const [cwd, setCwd] = useState('');
  const [pending, setPending] = useState<any | null>(null);
  const [showCwd, setShowCwd] = useState(false);
  const [settingsSection, setSettingsSection] = useState<SettingsSection | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [sessionListVersion, setSessionListVersion] = useState(0);
  const [headerMenuOpen, setHeaderMenuOpen] = useState(false);
  // 表头会话菜单改用 fixed 定位，避免被祖先 overflow/层叠裁剪；记录锚点坐标。
  const [headerMenuPos, setHeaderMenuPos] = useState<{ top: number; left: number } | null>(null);
  const [headerDialog, setHeaderDialog] = useState<'rename' | 'delete' | null>(null);
  const headerMenuRef = useRef<HTMLDivElement>(null);
  // 挂载时 URL 里的（短）session id；用 ref 暂存，避免被 URL 同步 effect 清掉。
  const urlSessionRef = useRef<string | null>(readSessionIdFromUrl());
  // 跳过首次 URL 同步：此时可能正等待把短 id 还原成完整会话，别先清掉参数。
  const firstUrlSync = useRef(true);
  // 刷新时若 URL 带 session id，先进入「恢复中」态：在按短 id 还原出会话前，
  // 抑制 Chat 的新建落地页，避免「先闪一下新建页再跳到历史」的体验。
  const [restoring, setRestoring] = useState<boolean>(() => readSessionIdFromUrl() != null);

  // 关闭表头会话菜单：外部点击 / 滚动 / 缩放（fixed 菜单不跟随锚点，故一并关闭）。
  useEffect(() => {
    if (!headerMenuOpen) return;
    const close = () => setHeaderMenuOpen(false);
    const onDown = (e: MouseEvent) => {
      if (headerMenuRef.current && !headerMenuRef.current.contains(e.target as Node)) {
        setHeaderMenuOpen(false);
      }
    };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('resize', close);
    window.addEventListener('scroll', close, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', close, true);
    };
  }, [headerMenuOpen]);

  // Chat 完成首条消息后会回传它创建的 session id；若是新 id，刷新侧栏列表。
  function handleSessionAssigned(id: string) {
    if (id !== sessionId) {
      setSessionId(id);
      setSessionListVersion((v) => v + 1);
    }
  }

  // ☰：移动端专用，开/关抽屉。桌面端的收起/展开由侧栏自身的按钮处理。
  function toggleSidebar() {
    setSidebarOpen((o) => !o);
  }

  // Seed cwd from /project on mount（恢复会话时以会话目录为准，故只在仍为空时填充）
  useEffect(() => {
    let cancelled = false;
    getProject()
      .then((p) => {
        if (!cancelled && p.working_dir) setCwd((c) => c || p.working_dir);
      })
      .catch(() => {
        // Ignore; cwd stays empty
      });
    return () => { cancelled = true; };
  }, []);

  // 把当前 session id（取前 8 位）同步进 URL，刷新后可恢复。
  useEffect(() => {
    // 跳过首次：挂载时若 URL 已带短 id 而 sessionId 尚未还原，别先把参数清掉。
    if (firstUrlSync.current) {
      firstUrlSync.current = false;
      return;
    }
    const url = new URL(window.location.href);
    if (sessionId) {
      url.searchParams.set('session', shortSessionId(sessionId));
    } else {
      url.searchParams.delete('session');
    }
    window.history.replaceState(null, '', url.pathname + url.search + url.hash);
  }, [sessionId]);

  // 刷新后用 URL 里的短 id 去会话列表按前缀还原成完整记录（完整 id + project_hash
  // + working_dir），再交给 Chat 加载历史。仅在挂载时执行一次。
  useEffect(() => {
    const urlSid = urlSessionRef.current;
    if (!urlSid) return;
    let cancelled = false;
    listSessions()
      .then((list) => {
        if (cancelled) return;
        // 兼容历史上写入的完整 id：优先精确匹配，否则按前缀匹配。
        const found =
          list.find((s) => s.id === urlSid) ??
          list.find((s) => s.id.startsWith(urlSid));
        if (found) {
          setSessionId(found.id);
          setActiveSession(found);
          if (found.working_dir) setCwd(found.working_dir);
        }
      })
      .catch(() => {
        /* 找不到就维持现状（回到新建页） */
      })
      .finally(() => {
        // 无论找没找到，恢复流程结束：解除抑制（找到→历史页，没找到→新建页）。
        if (!cancelled) setRestoring(false);
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function handleNewSession() {
    setSessionId(null);
    setActiveSession(null);
    setSidebarOpen(false);
  }

  function handleSelectSession(session: SessionMetaWithProject) {
    setSessionId(session.id);
    setActiveSession(session);
    if (session.working_dir) {
      setCwd(session.working_dir);
    }
    setSidebarOpen(false);
  }

  // 切换工作目录：侧栏按新目录过滤会话，并自动进入该目录的新对话。
  function handlePickCwd(path: string) {
    setCwd(path);
    setSessionId(null);
    setActiveSession(null);
  }

  // 删除会话：若删的是当前打开的会话，回到空白新对话。
  function handleSessionDeleted(id: string) {
    if (id === sessionId) {
      setSessionId(null);
      setActiveSession(null);
    }
  }

  // 重命名会话：若是当前会话，同步更新标题。
  function handleSessionRenamed(id: string, name: string) {
    if (id === sessionId) {
      setActiveSession((prev) => (prev ? { ...prev, name } : prev));
    }
  }

  const { prefix, name } = cwdDisplay(cwd);

  return (
    <div class="app">
      {/* ===== Full-height sidebar (通栏)：品牌 + 收起按钮在其顶部 ===== */}
      <Sidebar
        activeSessionId={sessionId}
        onSelect={handleSelectSession}
        onNew={handleNewSession}
        open={sidebarOpen}
        collapsed={sidebarCollapsed}
        onToggleCollapse={() => setSidebarCollapsed((c) => !c)}
        onOpenSettings={(section) => setSettingsSection(section)}
        reloadKey={sessionListVersion}
        cwd={cwd}
        onSessionRenamed={handleSessionRenamed}
        onSessionDeleted={handleSessionDeleted}
      />
      <div
        class={'sidebar-backdrop' + (sidebarOpen ? ' show' : '')}
        onClick={() => setSidebarOpen(false)}
      />

      {/* ===== Main column: header (cwd breadcrumb) + chat ===== */}
      <div class="main-column">
        <header class="header">
          <button
            class="ghost-btn hamburger-btn"
            onClick={toggleSidebar}
            aria-label={t('header.menu')}
            title={t('header.sessionList')}
          >
            ☰
          </button>

          {activeSession?.name && (
            <div class="header-session" ref={headerMenuRef}>
              <button
                class="header-session-name"
                title={activeSession.name}
                onClick={(e) => {
                  if (headerMenuOpen) {
                    setHeaderMenuOpen(false);
                    return;
                  }
                  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
                  setHeaderMenuPos({ top: r.bottom + 4, left: r.left });
                  setHeaderMenuOpen(true);
                }}
              >
                <span class="header-session-text">{activeSession.name}</span>
                <svg
                  class="header-session-chevron"
                  width="11"
                  height="11"
                  viewBox="0 0 16 16"
                  fill="none"
                  aria-hidden="true"
                >
                  <path
                    d="M4 6l4 4 4-4"
                    stroke="currentColor"
                    stroke-width="1.5"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                  />
                </svg>
              </button>
              {headerMenuOpen && headerMenuPos && (
                <div
                  class="item-menu"
                  style={{ top: `${headerMenuPos.top}px`, left: `${headerMenuPos.left}px` }}
                >
                  <button
                    class="item-menu-row"
                    onClick={() => {
                      setHeaderMenuOpen(false);
                      setHeaderDialog('rename');
                    }}
                  >
                    <span>{t('sidebar.rename')}</span>
                  </button>
                  <button
                    class="item-menu-row danger"
                    onClick={() => {
                      setHeaderMenuOpen(false);
                      setHeaderDialog('delete');
                    }}
                  >
                    <span>{t('sidebar.delete')}</span>
                  </button>
                </div>
              )}
            </div>
          )}

          <span class="header-spacer" />

          <button
            class="cwd-breadcrumb"
            onClick={() => setShowCwd(true)}
            title={t('header.switchCwd')}
          >
            {cwd ? (
              <>
                <span class="cwd-prefix">{prefix}</span>
                <span class="cwd-name">{name || cwd}</span>
              </>
            ) : (
              <span class="cwd-prefix">{t('header.noCwd')}</span>
            )}
            <span class="cwd-chevron">▾</span>
          </button>

          <button
            class="ghost-btn header-remote-btn"
            onClick={() => setSettingsSection('remote')}
            aria-label={t('settings.menuRemote')}
            title={t('settings.menuRemote')}
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <rect x="4.5" y="1.5" width="7" height="13" rx="1.5" stroke="currentColor" stroke-width="1.2" />
              <line x1="7" y1="12.3" x2="9" y2="12.3" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" />
            </svg>
          </button>
        </header>

        <div class="session-body app-sidebar">
          <Chat
            sessionId={sessionId}
            onSessionId={handleSessionAssigned}
            cwd={cwd}
            onPermission={setPending}
            onPermissionResolved={(callId) =>
              setPending((cur: any) =>
                callId === null ? null : resolvePendingAfterDecision(cur, callId),
              )}
            activeSession={activeSession}
            restoring={restoring}
          />
        </div>
      </div>

      {/* ===== Modals ===== */}
      {showCwd && (
        <CwdPicker
          current={cwd}
          onPick={handlePickCwd}
          onClose={() => setShowCwd(false)}
        />
      )}
      {settingsSection === 'theme' && (
        <ThemeDialog onClose={() => setSettingsSection(null)} />
      )}
      {settingsSection === 'language' && (
        <LanguageDialog onClose={() => setSettingsSection(null)} />
      )}
      {settingsSection === 'model' && (
        <ModelConfigDialog onClose={() => setSettingsSection(null)} />
      )}
      {settingsSection === 'remote' && (
        <RemoteAccessDialog onClose={() => setSettingsSection(null)} />
      )}
      {pending && <PermissionCard req={pending} onDone={() => setPending((cur: any) => resolvePendingAfterDecision(cur, pending.call_id))} />}
      {headerDialog === 'rename' && activeSession && (
        <RenameDialog
          session={activeSession}
          onClose={() => setHeaderDialog(null)}
          onDone={(name) => {
            handleSessionRenamed(activeSession.id, name);
            setSessionListVersion((v) => v + 1);
          }}
        />
      )}
      {headerDialog === 'delete' && activeSession && (
        <DeleteDialog
          session={activeSession}
          onClose={() => setHeaderDialog(null)}
          onDone={() => {
            handleSessionDeleted(activeSession.id);
            setSessionListVersion((v) => v + 1);
          }}
        />
      )}
    </div>
  );
}
