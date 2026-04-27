import React from 'react';
import { useChatContext } from '../state/ChatProvider';
import { ModelSelector } from './ModelSelector';
import { postMessage } from '../vscode';

export function Header() {
  const { dispatch, newConversation } = useChatContext();

  return (
    <header className="header">
      <div className="header-left">
        <button
          className="header-btn"
          onClick={() => dispatch({ type: 'TOGGLE_HISTORY' })}
          title="Toggle history"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
            <rect x="2" y="3" width="12" height="1.5" rx="0.5" />
            <rect x="2" y="7.25" width="12" height="1.5" rx="0.5" />
            <rect x="2" y="11.5" width="12" height="1.5" rx="0.5" />
          </svg>
        </button>
        <span className="header-title">AtomCode</span>
      </div>

      <div className="header-center">
        <ModelSelector />
      </div>

      <div className="header-actions">
        <button
          className="header-btn"
          onClick={() => newConversation()}
          title="New conversation"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
            <path d="M8 1.5a.75.75 0 01.75.75v5h5a.75.75 0 010 1.5h-5v5a.75.75 0 01-1.5 0v-5h-5a.75.75 0 010-1.5h5v-5A.75.75 0 018 1.5z" />
          </svg>
        </button>
        <button
          className="header-btn"
          onClick={() => postMessage({ type: 'openSettings' })}
          title="Settings"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
            <path d="M9.1 4.4L8.6 2H7.4l-.5 2.4-.7.3-2-1.3-.9.8 1.3 2-.3.7L2 7.4v1.2l2.4.5.3.7-1.3 2 .8.9 2-1.3.7.3.5 2.4h1.2l.5-2.4.7-.3 2 1.3.9-.8-1.3-2 .3-.7L14 8.6V7.4l-2.4-.5-.3-.7 1.3-2-.8-.9-2 1.3-.7-.3zM8 10a2 2 0 110-4 2 2 0 010 4z" />
          </svg>
        </button>
        <button
          className="header-btn"
          onClick={() => postMessage({ type: 'popout' })}
          title="Open in editor"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
            <path d="M2 3.5A1.5 1.5 0 013.5 2H6v1.5H3.5v9h9V10H14v2.5a1.5 1.5 0 01-1.5 1.5h-9A1.5 1.5 0 012 12.5v-9zM9 2h5v5h-1.5V4.06L7.28 9.28 6.22 8.22 11.44 3H9V2z" />
          </svg>
        </button>
      </div>
    </header>
  );
}
