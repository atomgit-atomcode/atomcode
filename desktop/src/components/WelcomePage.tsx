interface WelcomePageProps {
  daemonReady: boolean;
  cwd: string;
  onOpenFolder: () => void;
}

export function WelcomePage({ daemonReady, cwd, onOpenFolder }: WelcomePageProps) {
  return (
    <div className="welcome-page">
      <div className="welcome-content">
        <div className="welcome-logo">
          <svg width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
            <path d="M12 2L2 7l10 5 10-5-10-5z" />
            <path d="M2 17l10 5 10-5" />
            <path d="M2 12l10 5 10-5" />
          </svg>
        </div>
        <h1 className="welcome-title">AtomCode IDE</h1>
        <p className="welcome-subtitle">AI-powered code editor</p>

        {!daemonReady && (
          <div className="welcome-status welcome-not-ready">
            <div className="welcome-spinner" />
            <span>Starting AtomCode daemon...</span>
          </div>
        )}

        {daemonReady && !cwd && (
          <div className="welcome-actions">
            <button className="welcome-btn welcome-btn-primary" onClick={onOpenFolder}>
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z" />
              </svg>
              Open Folder
            </button>
            <p className="welcome-hint">Open a project folder to start coding with AI</p>
          </div>
        )}

        {daemonReady && cwd && (
          <div className="welcome-project">
            <p className="welcome-project-dir">📍 {cwd}</p>
            <p className="welcome-hint">Open a file from the explorer, or ask the AI to help you get started.</p>
          </div>
        )}

        <div className="welcome-features">
          <div className="welcome-feature">
            <span className="wf-icon">🤖</span>
            <span className="wf-text">AI-powered code generation & editing</span>
          </div>
          <div className="welcome-feature">
            <span className="wf-icon">📂</span>
            <span className="wf-text">Project file explorer</span>
          </div>
          <div className="welcome-feature">
            <span className="wf-icon">💬</span>
            <span className="wf-text">Natural language chat with context</span>
          </div>
          <div className="welcome-feature">
            <span className="wf-icon">🖥️</span>
            <span className="wf-text">Integrated terminal</span>
          </div>
        </div>
      </div>
    </div>
  );
}
