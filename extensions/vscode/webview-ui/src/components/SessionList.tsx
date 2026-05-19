import React, { useState, useMemo, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { groupSessionsByDate, formatTimeAgo } from '../utils/format';
import type { SessionMeta } from '../state/types';

const DATE_ORDER = ['Today', 'Yesterday', 'This Week', 'Older'];

interface SessionListProps {
  variant?: 'overlay' | 'sidebar';
}

export function SessionList({ variant = 'overlay' }: SessionListProps) {
  const { state, dispatch, loadSession, newConversation, renameSession, deleteSession } = useChatContext();
  const [search, setSearch] = useState('');
  const [menu, setMenu] = useState<{ session: SessionMeta; x: number; y: number } | null>(null);
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
    setMenu(null);
    loadSession(session.id, session.project_hash);
    if (isOverlay) {
      dispatch({ type: 'TOGGLE_HISTORY' });
    }
  }

  function handleNewSession() {
    setMenu(null);
    newConversation();
    if (isOverlay) {
      dispatch({ type: 'TOGGLE_HISTORY' });
    }
  }

  function handleContextMenu(e: React.MouseEvent, session: SessionMeta) {
    e.preventDefault();
    e.stopPropagation();
    const menuWidth = 132;
    const menuHeight = 84;
    setMenu({
      session,
      x: Math.min(e.clientX, window.innerWidth - menuWidth - 8),
      y: Math.min(e.clientY, window.innerHeight - menuHeight - 8),
    });
  }

  useEffect(() => {
    if (!menu) return undefined;

    function close() {
      setMenu(null);
    }

    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape') close();
    }

    window.addEventListener('click', close);
    window.addEventListener('scroll', close, true);
    window.addEventListener('keydown', handleKeyDown);
    return () => {
      window.removeEventListener('click', close);
      window.removeEventListener('scroll', close, true);
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [menu]);

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
        <button className="session-new-btn" onClick={handleNewSession}>
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
                  let dotClass = '';
                  if (!isActive) {
                    if (s.isGenerating) {
                      dotClass = 'session-item-dot breathing';
                    } else if (s.hasUnread) {
                      dotClass = 'session-item-dot';
                    }
                  }
                  return (
                    <button
                      key={`${s.project_hash ?? 'current'}:${s.id}`}
                      className={`session-item${isActive ? ' active' : ''}`}
                      onClick={() => handleSelect(s)}
                      onContextMenu={(e) => handleContextMenu(e, s)}
                      title={s.name || s.title || 'Untitled'}
                    >
                      {dotClass && <span className={dotClass} />}
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
      {menu && (
        <div
          className="session-context-menu"
          style={{ left: menu.x, top: menu.y }}
          onClick={(e) => e.stopPropagation()}
          onContextMenu={(e) => e.preventDefault()}
        >
          <button
            type="button"
            className="session-context-item"
            onClick={() => {
              renameSession(menu.session);
              setMenu(null);
            }}
          >
            修改名称
          </button>
          <button
            type="button"
            className="session-context-item danger"
            onClick={() => {
              deleteSession(menu.session);
              setMenu(null);
            }}
          >
            删除会话
          </button>
        </div>
      )}
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
