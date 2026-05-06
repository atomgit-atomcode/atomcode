import React, { useState, useMemo } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { groupSessionsByDate, formatTimeAgo } from '../utils/format';
import type { SessionMeta } from '../state/types';

const DATE_ORDER = ['Today', 'Yesterday', 'This Week', 'Older'];

interface SessionListProps {
  variant?: 'overlay' | 'sidebar';
}

export function SessionList({ variant = 'overlay' }: SessionListProps) {
  const { state, dispatch, loadSession, newConversation } = useChatContext();
  const [search, setSearch] = useState('');
  const isOverlay = variant === 'overlay';

  const filteredSessions = useMemo(() => {
    if (!search.trim()) return state.sessions;
    const q = search.toLowerCase();
    return state.sessions.filter(
      (s) =>
        (s.name ?? '').toLowerCase().includes(q) ||
        (s.title ?? '').toLowerCase().includes(q),
    );
  }, [state.sessions, search]);

  const groups = useMemo(() => groupSessionsByDate(filteredSessions), [filteredSessions]);

  function handleSelect(session: SessionMeta) {
    loadSession(session.id, session.project_hash);
    if (isOverlay) {
      dispatch({ type: 'TOGGLE_HISTORY' });
    }
  }

  const content = (
    <div className={`session-list session-list-${variant}`} onClick={(e) => e.stopPropagation()}>
      <div className="session-list-header">
        <div className="session-title-row">
          <h3>ATOMCODE</h3>
          {isOverlay && (
            <button className="ghost-btn" onClick={() => dispatch({ type: 'TOGGLE_HISTORY' })} title="Close">
              &times;
            </button>
          )}
        </div>
        <button className="session-new-btn" onClick={() => newConversation()}>
          <span className="session-new-icon">+</span>
          <span>New session</span>
        </button>
        <input
          className="session-search"
          type="text"
          placeholder="Search sessions..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          autoFocus={isOverlay}
        />
      </div>
      <div className="session-list-body">
        {filteredSessions.length === 0 ? (
          <div className="session-empty">No sessions yet</div>
        ) : (
          DATE_ORDER.map((label) => {
            const items = groups[label];
            if (!items || items.length === 0) return null;
            return (
              <div key={label}>
                <div className="session-group-label">{label}</div>
                {items.map((s) => {
                  const isActive = s.id === state.activeSessionId;
                  return (
                    <button
                      key={`${s.project_hash ?? 'current'}:${s.id}`}
                      className={`session-item${isActive ? ' active' : ''}`}
                      onClick={() => handleSelect(s)}
                      title={s.name || s.title || 'Untitled'}
                    >
                      <span className="session-item-name">
                        {s.name || s.title || 'Untitled'}
                      </span>
                      <span className="session-item-time">
                        {formatTimeAgo(s.updated_at ?? s.created_at)}
                      </span>
                    </button>
                  );
                })}
              </div>
            );
          })
        )}
      </div>
    </div>
  );

  return (
    isOverlay ? (
      <div className="session-overlay" onClick={() => dispatch({ type: 'TOGGLE_HISTORY' })}>
        {content}
      </div>
    ) : content
  );
}
