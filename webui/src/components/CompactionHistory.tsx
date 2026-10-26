import { useEffect, useRef, useState } from 'preact/hooks';
import { getSessionTranscript, type TranscriptTurn } from '../api';
import { useT } from '../settings';

/** Read-only history; it never replaces the active conversation or composer. */
export function CompactionHistory({ projectHash, sessionId, focusTurn, onNavigate }: {
  projectHash: string; sessionId: string; focusTurn?: { turnId: number; request: number; record?: TranscriptTurn } | null; onNavigate?: (node: HTMLElement) => void;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const [turns, setTurns] = useState<TranscriptTurn[] | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const pending = useRef<AbortController | null>(null);
  const [focusedTurn, setFocusedTurn] = useState<number | null>(null);
  const targetRef = useRef<HTMLElement | null>(null);
  useEffect(() => {
    if (focusTurn?.record) {
      pending.current?.abort();
      setOpen(true); setBusy(false); setError('');
      setFocusedTurn(focusTurn.turnId); setTurns([focusTurn.record]);
    } else if (focusTurn) void load(focusTurn.turnId);
    else if (focusedTurn !== null) { pending.current?.abort(); setBusy(false); }
  }, [focusTurn]);
  useEffect(() => {
    if (!open || focusedTurn === null || !turns || !focusTurn || focusedTurn !== focusTurn.turnId) return;
    const frame = requestAnimationFrame(() => {
      const node = targetRef.current;
      if (!node) return;
      if (onNavigate) onNavigate(node);
      else node.scrollIntoView({ block: 'start', behavior: 'auto' });
    });
    return () => cancelAnimationFrame(frame);
  }, [open, focusedTurn, turns, focusTurn]);
  useEffect(() => () => pending.current?.abort(), []);
  async function load(target: number | null = null) {
    setOpen(true);
    setFocusedTurn(target);
    pending.current?.abort();
    const controller = new AbortController();
    pending.current = controller;
    setBusy(true);
    setError('');
    setTurns(null);
    try {
      const result = await getSessionTranscript(projectHash, sessionId, controller.signal);
      if (!controller.signal.aborted) setTurns(result.turns);
    } catch (err) {
      if (!controller.signal.aborted) setError(String(err));
    } finally {
      if (!controller.signal.aborted) setBusy(false);
    }
  }
  // The navigation rail owns history discovery; only render after a selection.
  if (!open) return null;
  return <section class="compaction-history">
    <button type="button" aria-expanded={open} onClick={() => {
      pending.current?.abort(); setBusy(false); setOpen(false);
    }}>
      {t('chat.historyClose')}
    </button>
    {open && <div class="compaction-history-records">
      {focusedTurn !== null && <button type="button" onClick={() => void load()}>{t('chat.turnNavShowAll')}</button>}
      <p>{t('chat.historyExplanation')}</p>
      {busy && <p role="status">{t('chat.historyLoading')}</p>}
      {error && <p role="alert">{t('chat.historyFailed')} {error} <button onClick={() => void load(focusedTurn)}>{t('chat.historyRetry')}</button></p>}
      {turns?.length === 0 && <p>{t('chat.historyEmpty')}</p>}
      {focusedTurn !== null && turns && !turns.some(turn => turn.turn_id === focusedTurn) && <p role="status">{t('chat.historyEmpty')}</p>}
      {turns?.filter(turn => focusedTurn === null || turn.turn_id === focusedTurn).map(turn => <article key={turn.turn_id}
        class={focusedTurn === turn.turn_id ? 'turn-nav-history-target' : undefined}
        ref={el => { if (turn.turn_id === focusedTurn) targetRef.current = el; }}>
        <div>{t('chat.historyTurn')} {turn.turn_id}{turn.undone ? ` · ${t('chat.historyUndone')}` : ''}</div>
        <div>{t('chat.historyUser')}</div><pre>{turn.user}</pre>
        <div>{t('chat.historyAssistant')}</div><pre>{turn.assistant}</pre>
        {(turn.reasoning || turn.tools?.length) && <details><summary>{t('chat.historyDetails')}</summary>
          {turn.reasoning && <pre>{turn.reasoning}</pre>}
          {turn.tools?.map((tool, index) => <div key={index}><strong>{tool.name}</strong><pre>{tool.args}</pre><pre>{tool.result}</pre></div>)}
        </details>}
      </article>)}
    </div>}
  </section>;
}
