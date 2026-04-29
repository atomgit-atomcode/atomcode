import React, { useRef, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';

export function SearchBar() {
  const { state, dispatch } = useChatContext();
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  if (!state.searchOpen) return null;

  const matchCount = state.searchQuery
    ? state.messages.filter((m) => m.text.toLowerCase().includes(state.searchQuery.toLowerCase())).length
    : 0;

  return (
    <div style={{
      display: 'flex', alignItems: 'center', gap: 8,
      padding: '6px 10px',
      borderBottom: '0.5px solid var(--app-input-border)',
      background: 'var(--app-primary-background)',
      flexShrink: 0,
    }}>
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="var(--app-secondary-foreground)" strokeWidth="2">
        <circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" />
      </svg>
      <input
        ref={inputRef}
        value={state.searchQuery}
        onChange={(e) => dispatch({ type: 'SET_SEARCH_QUERY', query: e.target.value })}
        onKeyDown={(e) => { if (e.key === 'Escape') dispatch({ type: 'TOGGLE_SEARCH' }); }}
        placeholder="Search messages..."
        style={{
          flex: 1, background: 'transparent', border: 'none', outline: 'none',
          color: 'var(--app-primary-foreground)', fontSize: 12, fontFamily: 'inherit',
        }}
      />
      {state.searchQuery && (
        <span style={{ fontSize: 11, color: 'var(--app-secondary-foreground)' }}>{matchCount} found</span>
      )}
      <button
        onClick={() => dispatch({ type: 'TOGGLE_SEARCH' })}
        style={{
          background: 'transparent', border: 'none', cursor: 'pointer',
          color: 'var(--app-secondary-foreground)', fontSize: 14, padding: 0, lineHeight: 1,
        }}
      >
        ×
      </button>
    </div>
  );
}
