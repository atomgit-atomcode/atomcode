// Two-column layout: sidebar + chat, header with cwd breadcrumb + config.
// VSCode design system: timeline messages, violet brand, floating input.

import { useEffect, useRef, useState } from 'preact/hooks';
import { Chat } from './components/Chat';
import { Sidebar } from './components/Sidebar';
import { ThemeDialog, LanguageDialog, ModelConfigDialog } from './components/SettingsDialogs';
import { RenameDialog, DeleteDialog } from './components/SessionDialogs';
import { CwdPicker } from './components/CwdPicker';
import { PermissionCard } from './components/PermissionCard';
import { getProject, listSessions, SessionMetaWithProject } from './api';
import { useT, SettingsSection } from './settings';

function cwdDisplay(cwd: string): { prefix: string; name: string } {
  const clean = cwd.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  if (idx < 0) return { prefix: '', name: cwd };
  return { prefix: clean.slice(0, idx + 1), name: clean.slice(idx + 1) };
}

// 从 URL (?session=<id>) 读取要打开的会话 id，用于刷新后恢复。
function readSessionIdFromUrl(): string | null {
  try {
    return new URLSearchParams(window.location.search).get('session');
  } catch {
    return null;
  }
}

export function App() {
  const t = useT();
  const [sessionId, setSessionId] = useState<string | null>(() => readSessionIdFromUrl());
  const [activeSession, setActiveSession] = useState<SessionMetaWithProject | null>(null);
  const [cwd, setCwd] = useState('');
  const [pending, setPending] = useState<any | null>(null);
  const [showCwd, setShowCwd] = useState(false);
  const [settingsSection, setSettingsSection] = useState<SettingsSection | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [sessionListVersion, setSessionListVersion] = useState(0);
  const [headerMenuOpen, setHeaderMenuOpen] = useState(false);
  const [headerDialog, setHeaderDialog] = useState<'rename' | 'delete' | null>(null);
  const headerMenuRef = useRef<HTMLDivElement>(null);

  // Close the header session menu on outside click.
  useEffect(() => {
    if (!headerMenuOpen) return;
    const h = (e: MouseEvent) => {
      if (headerMenuRef.current && !headerMenuRef.current.contains(e.target as Node)) {
        setHeaderMenuOpen(false);
      }
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
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
    getProject()
      .then((p) => {
        if (p.working_dir) setCwd((c) => c || p.working_dir);
      })
      .catch(() => {
        // Ignore; cwd stays empty
      });
  }, []);

  // 把当前 session id 同步进 URL（?session=<id>），刷新后可恢复。
  useEffect(() => {
    const url = new URL(window.location.href);
    if (sessionId) {
      url.searchParams.set('session', sessionId);
    } else {
      url.searchParams.delete('session');
    }
    window.history.replaceState(null, '', url.pathname + url.search + url.hash);
  }, [sessionId]);

  // 刷新后若 URL 带 session 但还没有会话元数据，去会话列表里找回完整元数据
  // （project_hash + working_dir），Chat 才能据此加载历史。仅在挂载时执行一次。
  useEffect(() => {
    if (!sessionId || activeSession) return;
    let cancelled = false;
    listSessions()
      .then((list) => {
        if (cancelled) return;
        const found = list.find((s) => s.id === sessionId);
        if (found) {
          setActiveSession(found);
          if (found.working_dir) setCwd(found.working_dir);
        }
      })
      .catch(() => {
        /* 找不到就维持现状：Chat 会显示「继续会话」提示 */
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
                onClick={() => setHeaderMenuOpen((o) => !o)}
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
              {headerMenuOpen && (
                <div class="item-menu header-session-menu">
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
        </header>

        <div class="session-body app-sidebar">
          <Chat
            sessionId={sessionId}
            onSessionId={handleSessionAssigned}
            cwd={cwd}
            onPermission={setPending}
            activeSession={activeSession}
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
      {pending && <PermissionCard req={pending} onDone={() => setPending(null)} />}
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
