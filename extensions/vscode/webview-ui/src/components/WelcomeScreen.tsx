import React from 'react';
import { postMessage } from '../vscode';

interface QuickAction {
  id: string;
  label: string;
  icon: string;
}

const quickActions: QuickAction[] = [
  { id: 'explain', label: 'Explain Code', icon: '💡' },
  { id: 'fix', label: 'Fix Issues', icon: '🔧' },
  { id: 'test', label: 'Write Tests', icon: '🧪' },
  { id: 'refactor', label: 'Refactor', icon: '♻️' },
  { id: 'docs', label: 'Add Docs', icon: '📝' },
  { id: 'review', label: 'Code Review', icon: '🔍' },
];

export function WelcomeScreen() {
  function handleAction(action: string) {
    postMessage({ type: 'quickAction', action });
  }

  return (
    <div className="welcome-screen">
      <div className="welcome-content">
        <h1 className="welcome-title">AtomCode</h1>
        <p className="welcome-subtitle">AI-powered coding assistant</p>
        <div className="quick-actions">
          {quickActions.map((a) => (
            <button
              key={a.id}
              className="quick-action-card"
              onClick={() => handleAction(a.id)}
            >
              <span className="quick-action-icon">{a.icon}</span>
              <span className="quick-action-label">{a.label}</span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
