import { useEffect, useState } from 'react';

interface StatusBarProps {
  apiBaseUrl: string;
  daemonReady: boolean;
  cwd: string;
  openFilePath: string | null;
}

export function StatusBar({ apiBaseUrl, daemonReady, cwd, openFilePath }: StatusBarProps) {
  const gitBranch = 'main'; // Placeholder — will integrate real git later

  const dirName = cwd.split(/[/\\]/).pop() || '';
  const fileName = openFilePath?.split(/[/\\]/).pop() || '';

  return (
    <div className="status-bar">
      <div className="status-left">
        <span className="status-item" title={cwd}>
          <span className="status-icon">📁</span>
          <span className="status-text">{dirName || 'No folder'}</span>
        </span>
        {gitBranch && (
          <span className="status-item">
            <span className="status-icon">⎇</span>
            <span className="status-text">{gitBranch}</span>
          </span>
        )}
        {openFilePath && (
          <span className="status-item" title={openFilePath}>
            <span className="status-icon">📄</span>
            <span className="status-text">{fileName}</span>
          </span>
        )}
      </div>
      <div className="status-right">
        <span className="status-item">
          <span className={`status-dot ${daemonReady ? 'status-ok' : 'status-err'}`} />
          <span className="status-text">{daemonReady ? 'Daemon Ready' : 'Connecting...'}</span>
        </span>
        <span className="status-item">
          <span className="status-text">AtomCode IDE</span>
        </span>
      </div>
    </div>
  );
}
