import React, { useRef, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { useT } from '../i18n';

export function SearchBar() {
  const { state, dispatch } = useChatContext();
  const inputRef = useRef<HTMLInputElement>(null);
  const t = useT();

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  if (!state.searchOpen) return null;

  const total = state.search.matches.length;
  const current = total > 0 ? state.search.currentMatchIndex + 1 : 0;
  const hasMatches = total > 0;

  const goPrev = () => dispatch({ type: 'SEARCH_PREV' });
  const goNext = () => dispatch({ type: 'SEARCH_NEXT' });

  return (
    <div className="search-bar">
      <svg className="search-bar-icon" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="var(--app-secondary-foreground)" strokeWidth="2">
        <circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" />
      </svg>
      <input
        ref={inputRef}
        className="search-bar-input"
        value={state.searchQuery}
        onChange={(e) => dispatch({ type: 'SET_SEARCH_QUERY', query: e.target.value })}
        onKeyDown={(e) => {
          if (e.nativeEvent.isComposing) return;
          if (e.key === 'Escape') {
            e.preventDefault();
            dispatch({ type: 'TOGGLE_SEARCH' });
          } else if (e.key === 'Enter') {
            e.preventDefault();
            if (e.shiftKey) {
              goPrev();
            } else {
              goNext();
            }
          }
        }}
        placeholder={t('search.placeholder')}
      />
      {state.searchQuery.trim() && (
        <span className="search-bar-count">
          {hasMatches
            ? t('search.position', { current, total })
            : t('search.noResults')}
        </span>
      )}
      <button
        className="search-nav-btn"
        onClick={goPrev}
        disabled={!hasMatches}
        aria-label={t('search.prev')}
        title={t('search.prev')}
      >
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none">
          <path d="M11 4L7 8L11 12" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      <button
        className="search-nav-btn"
        onClick={goNext}
        disabled={!hasMatches}
        aria-label={t('search.next')}
        title={t('search.next')}
      >
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none">
          <path d="M5 4L9 8L5 12" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      <button
        className="search-bar-close"
        onClick={() => dispatch({ type: 'TOGGLE_SEARCH' })}
        aria-label={t('search.close')}
        title={t('search.close')}
      >
        ×
      </button>
    </div>
  );
}
