import React, { useEffect, useId, useRef, useState } from 'react';
import type { TodoItemData, TodoStatus } from '../state/types';
import { useT } from '../i18n';

function statusLabel(status: TodoStatus, t: ReturnType<typeof useT>): string {
  if (status === 'in_progress') return t('todo.inProgress');
  if (status === 'completed') return t('todo.completed');
  return t('todo.pending');
}

function statusGlyph(status: TodoStatus): string {
  if (status === 'in_progress') return '●';
  if (status === 'completed') return '✓';
  return '○';
}

export function TodoPanel({ items }: { items: TodoItemData[] }) {
  const t = useT();
  const listId = useId();
  const hasOpenItems = items.some((item) => item.status !== 'completed');
  const [expanded, setExpanded] = useState(hasOpenItems);
  const previousHasOpenItems = useRef(hasOpenItems);
  const completed = items.filter((item) => item.status === 'completed').length;

  useEffect(() => {
    if (!previousHasOpenItems.current && hasOpenItems) {
      setExpanded(true);
    }
    previousHasOpenItems.current = hasOpenItems;
  }, [hasOpenItems]);

  return (
    <section className="todo-panel" aria-label={t('todo.title')}>
      <button
        type="button"
        className="todo-panel-header"
        aria-expanded={expanded}
        aria-controls={listId}
        title={expanded ? t('todo.collapse') : t('todo.expand')}
        onClick={() => setExpanded((value) => !value)}
      >
        <span className="todo-panel-heading">{t('todo.title')}</span>
        <span className="todo-panel-progress">
          {t('todo.progress', { completed, total: items.length })}
        </span>
        <span className={`todo-panel-chevron${expanded ? ' expanded' : ''}`} aria-hidden="true">›</span>
      </button>
      {expanded && (
        <div id={listId} className="todo-panel-list" role="list">
          {items.map((item, index) => {
            const label = statusLabel(item.status, t);
            return (
              <div
                key={index}
                className={`todo-panel-item todo-panel-item-${item.status}`}
                role="listitem"
                aria-label={`${label}: ${item.content}`}
              >
                <span className="todo-panel-marker" aria-hidden="true">
                  {statusGlyph(item.status)}
                </span>
                <span className="todo-panel-content">{item.content}</span>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}
