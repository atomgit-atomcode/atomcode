import React, { useState, useMemo } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { groupSessionsByDate, formatTimeAgo } from '../utils/format';

const DATE_ORDER = ['Today', 'Yesterday', 'This Week', 'Older'];

export function SessionList() {
  const { state, dispatch, loadSession } = useChatContext();
  const [search, setSearch] = useState('');

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

  function handleSelect(sessionId: string) {
    loadSession(sessionId);
    dispatch({ type: 'TOGGLE_HISTORY' });
  }

  return (
    <div className="session-overlay" onClick={() => dispatch({ type: 'TOGGLE_HISTORY' })}>
      <div className="session-list" onClick={(e) => e.stopPropagation()}>
        <div className="session-list-header">
          <h3>History</h3>
          <input
            className="session-search"
            type="text"
            placeholder="Search sessions..."
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            autoFocus
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
                  {items.map((s) => (
                    <button
                      key={s.id}
                      className="session-item"
                      onClick={() => handleSelect(s.id)}
                    >
                      <span className="session-item-name">
                        {s.name || s.title || 'Untitled'}
                      </span>
                      <span className="session-item-time">
                        {formatTimeAgo(s.updated_at ?? s.created_at)}
                      </span>
                    </button>
                  ))}
                </div>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
}
