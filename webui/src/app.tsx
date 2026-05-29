// Task 15 — Two-column layout: sidebar + chat, header with cwd breadcrumb + config

import { useEffect, useState } from 'preact/hooks';
import { Chat } from './components/Chat';
import { Sidebar } from './components/Sidebar';
import { ConfigPanel } from './components/ConfigPanel';
import { CwdPicker } from './components/CwdPicker';
import { PermissionCard } from './components/PermissionCard';
import { getProject, SessionMetaWithProject } from './api';

function cwdDisplay(cwd: string): { prefix: string; name: string } {
  // Show last component as bold name, rest as muted prefix
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
    // cwd stays unchanged
  }

  function handleSelectSession(session: SessionMetaWithProject) {
    setSessionId(session.id);
    setActiveSession(session);
    // Update cwd to the session's working dir
    if (session.working_dir) {
      setCwd(session.working_dir);
    }
  }

  const { prefix, name } = cwdDisplay(cwd);

  return (
    <div class="h-screen flex flex-col bg-slate-50 text-slate-800">
      {/* ===== Header ===== */}
      <header class="h-12 shrink-0 flex items-center justify-between px-4 border-b border-slate-200 bg-white">
        <div class="flex items-center gap-2">
          <span class="font-mono font-bold text-indigo-600">▲ atomcode webui</span>
        </div>

        <div class="flex items-center gap-3 text-sm">
          {/* CWD breadcrumb button */}
          <button
            onClick={() => setShowCwd(true)}
            class="flex items-center gap-1.5 px-2.5 py-1 rounded-lg bg-slate-100 hover:bg-slate-200 font-mono text-xs"
            title="切换工作目录"
          >
            <span class="text-slate-400">📁</span>
            {cwd ? (
              <>
                <span class="text-slate-400">{prefix}</span>
                <span class="font-semibold text-slate-700">{name || cwd}</span>
              </>
            ) : (
              <span class="text-slate-400">（未设置）</span>
            )}
            <span class="text-slate-400">▾</span>
          </button>

          <span class="text-slate-300">|</span>

          {/* Config button */}
          <button
            onClick={() => setShowConfig(true)}
            class="text-slate-500 hover:text-slate-800"
          >
            ⚙ 配置
          </button>
        </div>
      </header>

      {/* ===== Body: sidebar + chat ===== */}
      <div class="flex-1 flex min-h-0">
        <Sidebar
          activeSessionId={sessionId}
          onSelect={handleSelectSession}
          onNew={handleNewSession}
        />

        <main class="flex-1 flex flex-col min-w-0 bg-slate-50">
          <Chat
            sessionId={sessionId}
            onSessionId={setSessionId}
            cwd={cwd}
            onPermission={setPending}
            activeSession={activeSession}
          />
        </main>
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
