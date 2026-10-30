import type { ComponentChildren } from 'preact';
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks';
import { getSessionTranscript, type TranscriptTurn } from '../api';
import { historyBoundary, historyWindow, type HistoryMessage } from '../lib/inlineHistory';
import { useT } from '../settings';

/** Display-only paging. The runtime conversation and composer are never replaced. */
export function CompactionHistory(props: {
  projectHash: string; sessionId: string; messages: HistoryMessage[]; reset: number;
  canLoadBefore?: boolean; compact?: boolean;
  focusTurn?: { turnId: number; request: number } | null;
  viewport: () => HTMLElement | null;
  onBeforeRead: () => void;
  onWindow: (active: boolean, showLive: boolean, liveStart: number) => void;
  onNavigate: (node: HTMLElement) => void;
  onReadingTurn?: (turnId: number | null) => void;
  renderTurn: (turn: TranscriptTurn, onEdges: (before: boolean, after: boolean) => void) => ComponentChildren;
}) {
  const t = useT();
  const latest = useRef(props); latest.current = props;
  const [window, setWindow] = useState<{ start: number; end: number; turns: TranscriptTurn[] } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const windowRef = useRef(window); windowRef.current = window;
  const pending = useRef<AbortController | null>(null);
  const retry = useRef<() => void>(() => {});
  const epoch = useRef(0);
  // Ignore navigation/anchor correction frames; wheel and touch gestures still page immediately.
  const ignoreScrollUntil = useRef(0);
  const index = useRef<TranscriptTurn[] | null>(null);
  const nodes = useRef(new Map<number, HTMLElement>());
  const innerEdges = useRef(new Map<number, { before: boolean; after: boolean }>());
  const [bounds, setBounds] = useState<{ end: number; liveStart: number; joined: boolean } | null>(null);

  function cancel() { epoch.current += 1; pending.current?.abort(); pending.current = null; }
  function reset() {
    cancel(); setBusy(false); setError(''); setWindow(null); windowRef.current = null;
    latest.current.onWindow(false, true, 0);
  }
  useLayoutEffect(() => { reset(); return cancel; }, [props.reset]);
  useEffect(() => {
    if (props.focusTurn) void load('target', props.focusTurn.turnId);
  }, [props.focusTurn]);

  useLayoutEffect(() => {
    const current = windowRef.current;
    if (!current || !index.current) return;
    const boundary = historyBoundary(index.current, props.messages);
    if (current.end > boundary.end) { boundary.end = index.current.length; boundary.joined = false; }
    if (!bounds || boundary.end !== bounds.end || boundary.liveStart !== bounds.liveStart || boundary.joined !== bounds.joined) {
      setBounds(boundary);
      props.onWindow(true, boundary.joined && current.end >= boundary.end, boundary.liveStart);
    }
  }, [props.messages]);

  async function load(direction: 'before' | 'after' | 'target', target?: number) {
    if (pending.current && direction !== 'target') return;
    cancel();
    const run = epoch.current;
    const controller = new AbortController(); pending.current = controller;
    const p = latest.current;
    const viewport = p.viewport();
    retry.current = () => { void load(direction, target); };
    setBusy(true); setError('');
    try {
      // Refresh the index on explicit navigation and the first upward load, not per token.
      const records = direction === 'target' || !index.current
        ? (await getSessionTranscript(p.projectHash, p.sessionId, controller.signal, { index: true })).turns : index.current;
      if (controller.signal.aborted) return;
      index.current = records;
      const boundary = historyBoundary(records, latest.current.messages);
      const current = windowRef.current;
      const found = target == null ? -1 : records.findIndex(turn => turn.turn_id === target);
      if (direction === 'target' && found < 0) throw new Error('Turn unavailable');
      // A directly selected record is reachable even if the live suffix overlaps it.
      if (found >= boundary.end || (direction !== 'target' && current && current.end > boundary.end)) { boundary.end = records.length; boundary.joined = false; }
      const range = direction === 'target' || !current
        ? historyWindow(boundary.end, found < 0 ? undefined : found, p.compact ? 1 : 2)
        : direction === 'before'
          ? { start: Math.max(0, current.start - (p.compact ? 1 : 2)), end: current.start }
          : { start: current.end, end: Math.min(boundary.end, current.end + (p.compact ? 1 : 2)) };
      if (range.start >= range.end) return;
      p.onBeforeRead();
      const selected = records.slice(range.start, range.end);
      // One event-log read per batch, rather than six full-log reads in parallel.
      const result = await getSessionTranscript(p.projectHash, p.sessionId, controller.signal,
        { turnIds: selected.map(turn => turn.turn_id) });
      const details = selected.map(turn => {
        const detail = result.turns.find(record => record.turn_id === turn.turn_id);
        if (!detail) throw new Error('Turn unavailable');
        return detail;
      });
      if (controller.signal.aborted) return;
      const readingLine = viewport?.getBoundingClientRect().top ?? 0;
      const anchor = viewport ? Array.from(viewport.querySelectorAll<HTMLElement>('[data-history-turn], .user-message-wrapper, .timeline-message'))
        .find(node => node.getBoundingClientRect().bottom > readingLine + 8) : undefined;
      const oldTop = anchor?.getBoundingClientRect().top;
      const oldHeight = viewport?.scrollHeight ?? 0;
      const oldScroll = viewport?.scrollTop ?? 0;
      let next = direction === 'target' || !current
        ? { ...range, turns: details }
        : direction === 'before'
          ? { start: range.start, end: current.end, turns: [...details, ...current.turns] }
          : { start: current.start, end: range.end, turns: [...current.turns, ...details] };
      // Bound mounted detail on long sessions; the opposite side remains scroll-loadable.
      const maxTurns = p.compact ? 3 : 6;
      if (next.turns.length > maxTurns) {
        if (direction === 'before') next = { ...next, end: next.start + maxTurns, turns: next.turns.slice(0, maxTurns) };
        else next = { ...next, start: next.end - maxTurns, turns: next.turns.slice(-maxTurns) };
      }
      // Compaction/snapshot replay can replace the live suffix while detail is in flight.
      // Join against its current identity/indices, never the request's captured messages.
      const committedBoundary = historyBoundary(records, latest.current.messages);
      if (next.end > committedBoundary.end) { committedBoundary.end = records.length; committedBoundary.joined = false; }
      setBounds(committedBoundary); windowRef.current = next; setWindow(next);
      latest.current.onWindow(true, committedBoundary.joined && next.end >= committedBoundary.end, committedBoundary.liveStart);
      requestAnimationFrame(() => {
        if (controller.signal.aborted || run !== epoch.current) return;
        ignoreScrollUntil.current = performance.now() + 800;
        if (target != null) {
          const node = nodes.current.get(target);
          if (node) { p.onReadingTurn?.(target); p.onNavigate(node); }
        } else if (viewport) {
          viewport.scrollTop = anchor?.isConnected && oldTop != null
            ? viewport.scrollTop + anchor.getBoundingClientRect().top - oldTop
            : direction === 'before' ? oldScroll + viewport.scrollHeight - oldHeight : oldScroll;
        }
      });
    } catch (err) {
      if (!controller.signal.aborted) setError(String(err));
    } finally {
      if (!controller.signal.aborted) { pending.current = null; setBusy(false); }
    }
  }

  useEffect(() => {
    const viewport = latest.current.viewport();
    if (!viewport) return;
    let previous = viewport.scrollTop;
    const older = () => {
      const first = windowRef.current?.turns[0];
      if (first && innerEdges.current.get(first.turn_id)?.before) return;
      if (latest.current.canLoadBefore === false) return;
      if (viewport.scrollTop <= 120 && (!windowRef.current || windowRef.current.start > 0)) void load('before');
    };
    const newer = () => {
      const turns = windowRef.current?.turns;
      const last = turns?.[turns.length - 1];
      if (last && innerEdges.current.get(last.turn_id)?.after) return;
      void load('after');
    };
    const scroll = () => {
      const top = viewport.scrollTop;
      const line = viewport.getBoundingClientRect().top + 48;
      let reading: { id: number; top: number; bottom: number } | undefined;
      for (const [id, node] of nodes.current) {
        const rect = node.getBoundingClientRect();
        const y = rect.top;
        if (y <= line && (!reading || y > reading.top)) reading = { id, top: y, bottom: rect.bottom };
      }
      latest.current.onReadingTurn?.(reading && reading.bottom > line ? reading.id : null);
      if (performance.now() < ignoreScrollUntil.current) { previous = top; return; }
      if (top < previous) older();
      else if (top > previous && viewport.scrollHeight - top - viewport.clientHeight < 160 && windowRef.current) newer();
      previous = top;
    };
    const wheel = (event: WheelEvent) => {
      if (event.deltaY < 0) older();
      else if (event.deltaY > 0 && viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < 160 && windowRef.current) newer();
    };
    let touchY: number | null = null;
    const touchStart = (event: TouchEvent) => { touchY = event.touches[0]?.clientY ?? null; };
    const touchMove = (event: TouchEvent) => {
      const y = event.touches[0]?.clientY;
      if (touchY != null && y != null && y > touchY + 8) older();
      touchY = y ?? null;
    };
    viewport.addEventListener('touchstart', touchStart, { passive: true });
    viewport.addEventListener('touchmove', touchMove, { passive: true });
    viewport.addEventListener('scroll', scroll, { passive: true });
    viewport.addEventListener('wheel', wheel, { passive: true });
    return () => { viewport.removeEventListener('scroll', scroll); viewport.removeEventListener('wheel', wheel); viewport.removeEventListener('touchstart', touchStart); viewport.removeEventListener('touchmove', touchMove); cancel(); };
  }, []);

  return <>
    {busy && <div class="timeline-history-status is-loading" role="status"><span>{t('chat.historyLoading')}</span></div>}
    {error && <div class="timeline-history-status" role="alert">{t('chat.historyFailed')} {error} <button onClick={() => retry.current()}>{t('chat.historyRetry')}</button></div>}
    {window?.turns.map(turn => <div key={turn.turn_id} data-history-turn={turn.turn_id}
      class={turn.undone ? 'timeline-history-turn is-undone' : 'timeline-history-turn'}
      ref={node => { if (node) nodes.current.set(turn.turn_id, node); else { nodes.current.delete(turn.turn_id); innerEdges.current.delete(turn.turn_id); } }}>
      {turn.undone && <div class="timeline-history-status">{t('chat.historyUndone')}</div>}
      {props.renderTurn(turn, (before, after) => { if (before || after) innerEdges.current.set(turn.turn_id, { before, after }); else innerEdges.current.delete(turn.turn_id); })}
    </div>)}
    {window && bounds && window.end < bounds.end && !busy && <div class="timeline-history-status">
      <button onClick={() => void load('after')}>{t('chat.historyLoadNext')}</button>
    </div>}
  </>;
}
