import { useEffect, useRef, useState } from 'preact/hooks';
import { getSessionTranscript, type TranscriptTurn } from '../api';
import { Markdown } from './Markdown';
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
    if (focusTurn?.record?.steps) {
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
      const result = await getSessionTranscript(projectHash, sessionId, controller.signal, target === null ? { all: true } : { turnId: target });
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
        <div class="history-user"><span class="history-role">{t('chat.historyUser')}</span><div class="history-user-text">{turn.user}</div></div>
        <div class="history-steps">
          {turn.steps ? turn.steps.map((step, index) => <section class="history-step" key={index}>
            {step.reasoning && <details class="history-reasoning"><summary>{t('chat.historyDetails')}</summary><Markdown content={step.reasoning} /></details>}
            {step.text && <div class="history-assistant"><Markdown content={step.text} /></div>}
            {step.tools.map(tool => <details class="history-tool" key={tool.id}>
              <summary><strong>{tool.name}</strong><span class={tool.is_error ? 'history-tool-failed' : 'history-tool-status'}>{t(tool.result === null ? 'tool.incomplete' : tool.is_error ? 'tool.failed' : 'tool.done')}</span></summary>
              <div class="history-tool-detail"><span class="history-role">{t('tool.args')}</span><pre>{tool.args}</pre>
                {tool.result !== null && <><span class="history-role">{t('tool.output')}</span><pre>{tool.result}</pre></>}
              </div>
            </details>)}
          </section>) : <>
            <p class="field-hint">{t('chat.historyMerged')}</p>
            <span class="history-role">{t('chat.historyAssistant')}</span><Markdown content={turn.assistant} />
            {(turn.reasoning || turn.tools?.length) && <details><summary>{t('chat.historyDetails')}</summary>
              {turn.reasoning && <Markdown content={turn.reasoning} />}
              {turn.tools?.map((tool, index) => <details class="history-tool" key={index}><summary>{tool.name}</summary><pre>{tool.args}</pre><pre>{tool.result}</pre></details>)}
            </details>}
          </>}
        </div>
      </article>)}
    </div>}
  </section>;
}
