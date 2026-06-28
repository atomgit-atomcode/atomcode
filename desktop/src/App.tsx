import { useState, useEffect, useCallback, useRef } from 'react';
import { ActivityBar, ActivityTab } from './components/ActivityBar';
import { FileExplorer } from './components/FileExplorer';
import { CodeEditor } from './components/CodeEditor';
import { ChatPanel } from './components/ChatPanel';
import { TerminalPanel } from './components/TerminalPanel';
import { StatusBar } from './components/StatusBar';
import { WelcomePage } from './components/WelcomePage';
import { PluginsPanel } from './components/PluginsPanel';
import { changeDir, getProject } from './api/daemonClient';

interface OpenFile {
  path: string;
  content: string;
  language: string;
}

interface AppProps {
  apiBaseUrl: string;
}

export function App({ apiBaseUrl }: AppProps) {
  const [activeTab, setActiveTab] = useState<ActivityTab>('explorer');
  const [cwd, setCwd] = useState('');
  const [openFile, setOpenFile] = useState<OpenFile | null>(null);
  const [daemonReady, setDaemonReady] = useState(false);
  const [showChat, setShowChat] = useState(false);
  const cwdRef = useRef(cwd);

  // Keep ref in sync
  useEffect(() => { cwdRef.current = cwd; }, [cwd]);

  // Check daemon health on mount
  useEffect(() => {
    let cancelled = false;
    async function init() {
      try {
        const resp = await fetch(`${apiBaseUrl}/health`);
        if (!resp.ok) throw new Error('Not ready');
        if (cancelled) return;
        setDaemonReady(true);

        // Try to get current workspace
        try {
          const project = await getProject(apiBaseUrl);
          if (project.working_dir) {
            setCwd(project.working_dir);
          }
        } catch {
          // No project yet, wait for user to open folder
        }
      } catch {
        if (!cancelled) {
          setTimeout(init, 1000);
        }
      }
    }
    init();
    return () => { cancelled = true; };
  }, [apiBaseUrl]);

  // Listen for folder-open from Electron menu
  useEffect(() => {
    const cleanup = window.electronAPI?.onOpenFolder((folderPath: string) => {
      handleOpenFolder(folderPath);
    });
    return cleanup;
  }, [apiBaseUrl]);

  async function handleOpenFolder(folderPath: string) {
    try {
      await changeDir(apiBaseUrl, folderPath);
      setCwd(folderPath);
    } catch (err) {
      console.error('Failed to open folder:', err);
    }
  }

  const handlePickFolder = useCallback(async () => {
    if (window.electronAPI) {
      const folderPath = await window.electronAPI.openFolderDialog();
      if (folderPath) {
        await handleOpenFolder(folderPath);
      }
    }
  }, [apiBaseUrl]);

  const handleOpenFile = useCallback(async (path: string) => {
    // If path is relative (no drive letter, not starting with / or \), prepend cwd
    const fullPath = path.includes(':') || path.startsWith('/') || path.startsWith('\\')
      ? path
      : `${cwdRef.current.replace(/\\/g, '/')}/${path}`;
    try {
      const resp = await fetch(`${apiBaseUrl}/fs/read?path=${encodeURIComponent(fullPath)}`);
      if (!resp.ok) {
        const errText = await resp.text();
        console.error('Failed to read file:', errText);
        return;
      }
      const data = await resp.json();
      if (data && data.content != null) {
        setOpenFile({
          path: data.path,
          content: data.content,
          language: data.language ?? '',
        });
      }
    } catch (err) {
      console.error('Failed to open file:', err);
    }
  }, [apiBaseUrl]);

  const handleSaveFile = useCallback(async (path: string, content: string) => {
    await fetch(`${apiBaseUrl}/fs/write`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path, content }),
    });
  }, [apiBaseUrl]);

  return (
    <div className="ide-root">
      {/* Horizontal row: activity bar, side panel, main content, chat panel */}
      <div className="ide-body">
        {/* Activity Bar (far left) */}
        <ActivityBar
          activeTab={activeTab}
          onTabChange={setActiveTab}
          showChat={showChat}
          onToggleChat={() => setShowChat((s) => !s)}
        />

        {/* Side Panel */}
        <div className={'ide-side-panel' + (showChat ? ' ide-side-panel-chat' : '')}>
          {activeTab === 'explorer' && (
            <FileExplorer
              apiBaseUrl={apiBaseUrl}
              cwd={cwd}
              onOpenFile={handleOpenFile}
              onOpenFolder={handlePickFolder}
            />
          )}
          {activeTab === 'search' && (
            <div className="ide-panel-placeholder">
              <div className="placeholder-icon">🔍</div>
              <div className="placeholder-text">Search (coming soon)</div>
            </div>
          )}
          {activeTab === 'git' && (
            <div className="ide-panel-placeholder">
              <div className="placeholder-icon">⎇</div>
              <div className="placeholder-text">Source Control (coming soon)</div>
            </div>
          )}
          {activeTab === 'plugins' && (
            <PluginsPanel apiBaseUrl={apiBaseUrl} />
          )}
        </div>

        {/* Chat panel (slides in from right) */}
        {showChat && (
          <div className="ide-chat-panel">
            <ChatPanel
              apiBaseUrl={apiBaseUrl}
              cwd={cwd}
              onClose={() => setShowChat(false)}
            />
          </div>
        )}

        {/* Main content area */}
        <div className="ide-main">
          {openFile ? (
            <CodeEditor
              filePath={openFile.path}
              content={openFile.content}
              language={openFile.language}
              apiBaseUrl={apiBaseUrl}
              onSave={handleSaveFile}
            />
          ) : (
            <WelcomePage
              daemonReady={daemonReady}
              cwd={cwd}
              onOpenFolder={handlePickFolder}
            />
          )}

          {/* Terminal (bottom) */}
          <TerminalPanel apiBaseUrl={apiBaseUrl} cwd={cwd} />
        </div>
      </div>

      {/* Status Bar (bottom) — full width horizontal bar */}
      <StatusBar
        apiBaseUrl={apiBaseUrl}
        daemonReady={daemonReady}
        cwd={cwd}
        openFilePath={openFile?.path ?? null}
      />
    </div>
  );
}
