import React from 'react';
import { useChatContext } from '../state/ChatProvider';
import { useT } from '../i18n';

export function Header() {
  const { dispatch, openSidebar, openSessionInTab } = useChatContext();
  const t = useT();

  return (
    <header className="header">
      <button className="ghost-btn" onClick={openSidebar} title={t('header.openSessions')}>
        <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
          <rect x="2" y="3" width="12" height="1.5" rx="0.5" />
          <rect x="2" y="7.25" width="12" height="1.5" rx="0.5" />
          <rect x="2" y="11.5" width="12" height="1.5" rx="0.5" />
        </svg>
      </button>
      <span className="header-spacer" />
      <button className="ghost-btn" onClick={() => openSessionInTab()} title={t('header.newConversation')}>
        <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
          <path d="M8 1.5a.75.75 0 01.75.75v5h5a.75.75 0 010 1.5h-5v5a.75.75 0 01-1.5 0v-5h-5a.75.75 0 010-1.5h5v-5A.75.75 0 018 1.5z" />
        </svg>
      </button>
      <button className="ghost-btn" onClick={() => dispatch({ type: 'TOGGLE_SEARCH' })} title={t('header.search')}>
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
          <circle cx="11" cy="11" r="7" /><line x1="21" y1="21" x2="16.65" y2="16.65" />
        </svg>
      </button>
    </header>
  );
}
