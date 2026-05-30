// Two-column layout: sidebar + chat, header with cwd breadcrumb + config.
// VSCode design system: timeline messages, violet brand, floating input.

import { useEffect, useState } from 'preact/hooks';
import { Chat } from './components/Chat';
import { Sidebar } from './components/Sidebar';
import { SettingsPanel } from './components/SettingsPanel';
import { CwdPicker } from './components/CwdPicker';
import { PermissionCard } from './components/PermissionCard';
import { getProject, SessionMetaWithProject } from './api';
import { useT } from './settings';

function cwdDisplay(cwd: string): { prefix: string; name: string } {
  const clean = cwd.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  if (idx < 0) return { prefix: '', name: cwd };
  return { prefix: clean.slice(0, idx + 1), name: clean.slice(idx + 1) };
}

export function App() {
  const t = useT();
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [activeSession, setActiveSession] = useState<SessionMetaWithProject | null>(null);
  const [cwd, setCwd] = useState('');
  const [pending, setPending] = useState<any | null>(null);
  const [showCwd, setShowCwd] = useState(false);
  const [showConfig, setShowConfig] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [sessionListVersion, setSessionListVersion] = useState(0);

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

  // Seed cwd from /project on mount
  useEffect(() => {
    getProject()
      .then((p) => {
        if (p.working_dir) setCwd(p.working_dir);
      })
      .catch(() => {
        // Ignore; cwd stays empty
      });
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
        onOpenSettings={() => setShowConfig(true)}
        reloadKey={sessionListVersion}
        cwd={cwd}
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
      {showConfig && <SettingsPanel onClose={() => setShowConfig(false)} />}
      {pending && <PermissionCard req={pending} onDone={() => setPending(null)} />}
    </div>
  );
}
