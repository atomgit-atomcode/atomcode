// Two-column layout: sidebar + chat, header with cwd breadcrumb + config.
// VSCode design system: timeline messages, violet brand, floating input.

import { useEffect, useState } from 'preact/hooks';
import { Chat } from './components/Chat';
import { Sidebar } from './components/Sidebar';
import { ConfigPanel } from './components/ConfigPanel';
import { CwdPicker } from './components/CwdPicker';
import { PermissionCard } from './components/PermissionCard';
import { getProject, SessionMetaWithProject } from './api';

function cwdDisplay(cwd: string): { prefix: string; name: string } {
  const clean = cwd.replace(/\/+$/, '');
  const idx = clean.lastIndexOf('/');
  if (idx < 0) return { prefix: '', name: cwd };
  return { prefix: clean.slice(0, idx + 1), name: clean.slice(idx + 1) };
}

export function App() {
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

  // ☰：移动端开/关抽屉；桌面端收起/展开侧栏。
  function toggleSidebar() {
    if (window.matchMedia('(max-width: 768px)').matches) {
      setSidebarOpen((o) => !o);
    } else {
      setSidebarCollapsed((c) => !c);
    }
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

  const { prefix, name } = cwdDisplay(cwd);

  return (
    <div class="app">
      {/* ===== Header ===== */}
      <header class="header">
        <button
          class="ghost-btn hamburger-btn"
          onClick={toggleSidebar}
          aria-label="菜单"
          title="收起/展开会话列表"
        >
          ☰
        </button>
        <span class="header-title">▲ atomcode</span>

        <span class="header-spacer" />

        <button
          class="cwd-breadcrumb"
          onClick={() => setShowCwd(true)}
          title="切换工作目录"
        >
          {cwd ? (
            <>
              <span class="cwd-prefix">{prefix}</span>
              <span class="cwd-name">{name || cwd}</span>
            </>
          ) : (
            <span class="cwd-prefix">（未设置工作目录）</span>
          )}
          <span class="cwd-chevron">▾</span>
        </button>

        <button
          class="ghost-btn"
          onClick={() => setShowConfig(true)}
          aria-label="配置"
          title="配置"
        >
          ⚙
        </button>
      </header>

      {/* ===== Body: sidebar + chat ===== */}
      <div class="session-row">
        <Sidebar
          activeSessionId={sessionId}
          onSelect={handleSelectSession}
          onNew={handleNewSession}
          open={sidebarOpen}
          collapsed={sidebarCollapsed}
          reloadKey={sessionListVersion}
        />
        <div
          class={'sidebar-backdrop' + (sidebarOpen ? ' show' : '')}
          onClick={() => setSidebarOpen(false)}
        />

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
          onPick={(p) => setCwd(p)}
          onClose={() => setShowCwd(false)}
        />
      )}
      {showConfig && <ConfigPanel onClose={() => setShowConfig(false)} />}
      {pending && <PermissionCard req={pending} onDone={() => setPending(null)} />}
    </div>
  );
}
