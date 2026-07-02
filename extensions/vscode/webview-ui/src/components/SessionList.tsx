import React, { useState, useMemo, useEffect } from 'react';
import { useChatContext } from '../state/ChatProvider';
import { groupSessionsByDate, formatTimeAgo } from '../utils/format';
import type { SessionMeta } from '../state/types';
import { MsgKey, useT } from '../i18n';

const DATE_ORDER = ['Today', 'Yesterday', 'This Week', 'Older'];
const DATE_LABEL_KEYS: Record<string, MsgKey> = {
  Today: 'session.today',
  Yesterday: 'session.yesterday',
  'This Week': 'session.thisWeek',
  Older: 'session.older',
};

interface SessionListProps {
  variant?: 'overlay' | 'sidebar';
}

export function SessionList({ variant = 'overlay' }: SessionListProps) {
  const { state, dispatch, openSessionInTab, renameSession, deleteSession, deleteSessions } = useChatContext();
  const t = useT();
  const [search, setSearch] = useState('');
  const [menu, setMenu] = useState<{ session: SessionMeta; x: number; y: number } | null>(null);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [selectMode, setSelectMode] = useState(false);
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

  // Clear selection and exit select mode when session list refreshes
  useEffect(() => {
    setSelectedIds(new Set());
    setSelectMode(false);
  }, [state.sessions]);

  function handleSelect(session: SessionMeta) {
    if (selectMode) {
      toggleSelect(session);
      return;
    }
    setMenu(null);
    openSessionInTab(session.id, session.project_hash);
    if (isOverlay) {
      dispatch({ type: 'TOGGLE_HISTORY' });
    }
  }

  function toggleSelect(session: SessionMeta) {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(session.id)) {
        next.delete(session.id);
      } else {
        next.add(session.id);
      }
      return next;
    });
  }

  function handleNewSession() {
    setMenu(null);
    openSessionInTab();
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

  function enterSelectMode() {
    setSelectMode(true);
  }

  function exitSelectMode() {
    setSelectMode(false);
    setSelectedIds(new Set());
  }

  function handleDeleteSelected() {
    const toDelete = state.sessions.filter((s) => selectedIds.has(s.id));
    if (toDelete.length === 0) return;
    deleteSessions(
      toDelete.map((s) => ({
        id: s.id,
        project_hash: s.project_hash,
        name: s.name || s.title || '',
      })),
    );
    exitSelectMode();
  }

  // Close context menu + handle Escape for select mode
  useEffect(() => {
    if (!menu && !selectMode) return undefined;

    function close() {
      setMenu(null);
    }

    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape') {
        if (selectMode) {
          exitSelectMode();
        } else {
          close();
        }
      }
    }

    function handleClickOutside(e: MouseEvent) {
      if (selectMode) {
        const target = e.target as HTMLElement;
        if (!target.closest('.session-list')) {
          exitSelectMode();
        }
      } else {
        close();
      }
    }

    window.addEventListener('click', handleClickOutside);
    window.addEventListener('scroll', close, true);
    window.addEventListener('keydown', handleKeyDown);
    return () => {
      window.removeEventListener('click', handleClickOutside);
      window.removeEventListener('scroll', close, true);
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [menu, selectMode]);

  const hasSelection = selectMode && selectedIds.size > 0;
  const selectedCount = selectedIds.size;

  const content = (
    <div
      className={`session-list session-list-${variant}${selectMode ? ' has-selection' : ''}`}
      onClick={(e) => e.stopPropagation()}
    >
      <div className="session-list-header">
        <div className="session-title-row">
          <h3>ATOMCODE</h3>
          <div className="session-title-actions">
            <button
              className="select-mode-toggle"
              onClick={selectMode ? exitSelectMode : enterSelectMode}
            >
              {selectMode ? t('session.done') : t('session.select')}
            </button>
            {isOverlay && (
              <button className="ghost-btn" onClick={() => dispatch({ type: 'TOGGLE_HISTORY' })} title={t('session.close')}>
                &times;
              </button>
            )}
          </div>
        </div>
        <button className="session-new-btn" onClick={handleNewSession}>
          <span className="session-new-icon">+</span>
          <span>{t('session.new')}</span>
        </button>
        <input
          className="session-search"
          type="text"
          placeholder={t('session.searchPlaceholder')}
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          autoFocus={isOverlay}
        />
      </div>
      {hasSelection && (
        <div className="session-list-selection-bar">
          <span className="selection-count">{t('session.selectedCount', { count: selectedCount })}</span>
          <button className="selection-bar-btn danger" onClick={handleDeleteSelected}>
            {t('session.deleteSelected')}
          </button>
          <button className="selection-bar-btn" onClick={exitSelectMode}>
            {t('session.cancelSelection')}
          </button>
        </div>
      )}
      <div className="session-list-body">
        {filteredSessions.length === 0 ? (
          <div className="session-empty">{t('session.empty')}</div>
        ) : (
          DATE_ORDER.map((label) => {
            const items = groups[label];
            if (!items || items.length === 0) return null;
            return (
              <div key={label}>
                <div className="session-group-label">{t(DATE_LABEL_KEYS[label])}</div>
                {items.map((s) => {
                  const isActive = s.id === state.activeSessionId;
                  const isChecked = selectedIds.has(s.id);
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
                      className={`session-item${isActive ? ' active' : ''}${selectMode && isChecked ? ' selected' : ''}`}
                      onClick={() => handleSelect(s)}
                      onContextMenu={(e) => handleContextMenu(e, s)}
                      title={selectMode ? undefined : (s.name || s.title || t('session.untitled'))}
                    >
                      <span
                        className={`session-item-checkbox${isChecked ? ' checked' : ''}`}
                        onClick={selectMode ? (e) => {
                          e.stopPropagation();
                          e.preventDefault();
                          toggleSelect(s);
                        } : undefined}
                      />
                      {dotClass && <span className={dotClass} />}
                      <span className="session-item-name">
                        {s.name || s.title || t('session.untitled')}
                      </span>
                      <span className="session-item-time">
                        {formatTimeAgo(s.updated_at ?? s.created_at, t)}
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
            {t('session.rename')}
          </button>
          <button
            type="button"
            className="session-context-item danger"
            onClick={() => {
              deleteSession(menu.session);
              setMenu(null);
            }}
          >
            {t('session.delete')}
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
