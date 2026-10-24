import { useEffect, useRef, useState } from 'preact/hooks';
import { getSessionTranscript, type TranscriptTurn } from '../api';
import { navigationSummary, turnWindow, type TurnNavigationItem } from '../lib/turnNavigation';
import { useT } from '../settings';

interface Props {
  items: TurnNavigationItem[];
  activeIndex: number;
  projectHash?: string;
  sessionId?: string;
  streaming: boolean;
  onNavigate: (messageIndex: number) => void;
  onHistory: (turn: TranscriptTurn) => void;
}

/** Only the rail is windowed; it does not own or replace the conversation. */
export function TurnNavRail({ items, activeIndex, projectHash, sessionId, streaming, onNavigate, onHistory }: Props) {
  const t = useT();
  const [expanded, setExpanded] = useState(false);
  const [history, setHistory] = useState(false);
  const [turns, setTurns] = useState<TranscriptTurn[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [retry, setRetry] = useState(0);
  const [top, setTop] = useState(0);
  const [height, setHeight] = useState(300);
  const [chosenHistory, setChosenHistory] = useState<number | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  const toggleRef = useRef<HTMLButtonElement | null>(null);
  const [focused, setFocused] = useState<number | null>(null);
  const interactionRef = useRef(false);
  // Invalidate on snapshots/rewind even when turn count is unchanged, but never per token.
  const settledItems = useRef(items);
  if (!streaming) settledItems.current = items;
  const pitch = expanded ? 54 : 26;
  const count = history ? turns.length : items.length;
  const selected = history ? turns.findIndex(turn => turn.turn_id === chosenHistory) : activeIndex;
  const { start, end } = turnWindow(count, top, height, pitch);
  const visibleIndexes = Array.from({ length: end - start }, (_, offset) => start + offset);
  if (focused !== null && focused < count && !visibleIndexes.includes(focused)) {
    visibleIndexes.push(focused);
    visibleIndexes.sort((a, b) => a - b);
  }

  useEffect(() => {
    const el = listRef.current;
    if (!el) return;
    const measure = () => setHeight(el.clientHeight || 300);
    measure();
    if (typeof ResizeObserver === 'undefined') {
      window.addEventListener('resize', measure);
      return () => window.removeEventListener('resize', measure);
    }
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [expanded]);
  useEffect(() => {
    setTop(0); setFocused(null);
    if (listRef.current) listRef.current.scrollTop = 0;
  }, [history, expanded]);
  useEffect(() => {
    if (!history || !projectHash || !sessionId) return;
    const controller = new AbortController();
    setBusy(true); setError('');
    getSessionTranscript(projectHash, sessionId, controller.signal).then(result => {
      if (!controller.signal.aborted) setTurns(result.turns);
    }).catch(err => {
      if (!controller.signal.aborted) setError(String(err));
    }).finally(() => { if (!controller.signal.aborted) setBusy(false); });
    return () => controller.abort();
  }, [history, projectHash, sessionId, streaming, settledItems.current, retry]);
  useEffect(() => {
    const el = listRef.current;
    if (!el || selected < 0 || interactionRef.current) return;
    const position = selected * pitch;
    if (position < el.scrollTop || position + pitch > el.scrollTop + el.clientHeight) {
      el.scrollTop = Math.max(0, position - el.clientHeight / 2);
      setTop(el.scrollTop);
    }
  }, [selected, pitch, count]);

  function activate(index: number) {
    if (history) {
      const turn = turns[index];
      if (turn) { setChosenHistory(turn.turn_id); onHistory(turn); }
    } else if (items[index]) onNavigate(items[index].messageIndex);
    if (window.matchMedia('(max-width: 768px)').matches) {
      setExpanded(false);
      requestAnimationFrame(() => toggleRef.current?.focus());
    }
  }
  function focusRow(index: number) {
    const el = listRef.current;
    if (!el) return;
    el.scrollTop = Math.max(0, index * pitch - el.clientHeight / 2);
    setTop(el.scrollTop);
    requestAnimationFrame(() => el.querySelector<HTMLButtonElement>(`[data-turn-index="${index}"]`)?.focus());
  }
  if (items.length === 0 && !sessionId) return null;
  return <nav class={`turn-nav-rail${expanded ? ' is-expanded' : ''}`} aria-label={t('chat.turnNav')}
    onPointerEnter={() => { interactionRef.current = true; }}
    onPointerLeave={() => { interactionRef.current = false; }}
    onFocusIn={() => { interactionRef.current = true; }}
    onFocusOut={event => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) interactionRef.current = false; }}>
    <button ref={toggleRef} class="turn-nav-toggle" type="button" aria-expanded={expanded}
      aria-label={t(expanded ? 'chat.turnNavCollapse' : 'chat.turnNavExpand')}
      title={t(expanded ? 'chat.turnNavCollapse' : 'chat.turnNavExpand')}
      onClick={() => setExpanded(value => !value)}>{expanded ? '›' : '☷'}{expanded && <span>{t('chat.turnNav')} <small>{count}</small></span>}</button>
    {expanded && <div class="turn-nav-tabs">
      <button type="button" class={!history ? 'is-active' : ''} aria-pressed={!history} onClick={() => setHistory(false)}>{t('chat.turnNavCurrent')}</button>
      {projectHash && sessionId && <button type="button" class={history ? 'is-active' : ''} aria-pressed={history} onClick={() => setHistory(true)}>{t('chat.turnNavHistory')}</button>}
    </div>}
    <div class="turn-nav-list" ref={listRef} onScroll={event => setTop(event.currentTarget.scrollTop)}
      onKeyDown={event => {
        const index = Number((event.target as HTMLElement).closest<HTMLElement>('[data-turn-index]')?.dataset.turnIndex);
        if (!Number.isFinite(index) || count === 0) return;
        let next: number;
        if (event.key === 'ArrowDown') next = Math.min(count - 1, index + 1);
        else if (event.key === 'ArrowUp') next = Math.max(0, index - 1);
        else if (event.key === 'Home') next = 0;
        else if (event.key === 'End') next = count - 1;
        else return;
        event.preventDefault(); focusRow(next);
      }}>
      <div class="turn-nav-track" style={{ height: `${count * pitch}px` }}>
        {visibleIndexes.map(index => {
          const record = history ? turns[index] : null;
          const live = history ? null : items[index];
          const user = navigationSummary(record?.user ?? live?.user ?? '') || t('chat.turnNavImage');
          const assistant = navigationSummary(record?.assistant ?? live?.assistant ?? '');
          const undone = record?.undone;
          const retained = live?.retained;
          const badge = undone ? t('chat.historyUndone') : retained ? t('chat.turnNavRetained') : live?.pending ? t('chat.turnNavPending') : '';
          const label = `${record?.turn_id ?? index + 1}. ${user}${badge ? ` · ${badge}` : ''}${assistant ? `\n${assistant}` : ''}`;
          return <button key={record ? `history-${record.turn_id}` : live!.key} type="button"
            class={`turn-nav-node${selected === index ? ' is-active' : ''}${undone ? ' is-undone' : ''}`}
            style={{ top: `${index * pitch}px`, height: `${pitch}px` }} data-turn-index={index}
            aria-label={label} aria-current={selected === index ? 'location' : undefined}
            title={label} onClick={() => activate(index)}
            onFocus={() => setFocused(index)} onBlur={() => setFocused(null)}>
            <span class="turn-nav-mark" />
            {expanded && <span class="turn-nav-summary"><span><b>{record?.turn_id ?? index + 1}</b> {user}</span>
              <small>{badge && <em>{badge} · </em>}{assistant || t('chat.turnNavWaiting')}</small></span>}
          </button>;
        })}
      </div>
    </div>
    {expanded && history && busy && <div class="turn-nav-status" role="status">{t('chat.historyLoading')}</div>}
    {expanded && history && error && <div class="turn-nav-status" role="alert">{t('chat.historyFailed')} <button onClick={() => setRetry(value => value + 1)}>{t('chat.historyRetry')}</button></div>}
    {expanded && !busy && !error && count === 0 && <div class="turn-nav-status">{t('chat.historyEmpty')}</div>}
    {expanded && history && <p class="turn-nav-footnote">{t('chat.turnNavReadOnly')}</p>}
  </nav>;
}
