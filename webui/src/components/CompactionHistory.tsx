import { useEffect, useRef, useState } from 'preact/hooks';
import { getSessionTranscript, type TranscriptTurn } from '../api';
import { useT } from '../settings';

/** Read-only history; it never replaces the active conversation or composer. */
export function CompactionHistory({ projectHash, sessionId }: { projectHash: string; sessionId: string }) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const [turns, setTurns] = useState<TranscriptTurn[] | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const pending = useRef<AbortController | null>(null);
  useEffect(() => () => pending.current?.abort(), []);
  async function load() {
    setOpen(true);
    if (busy) return;
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
  return <section class="compaction-history">
    <button type="button" aria-expanded={open} onClick={() => open ? setOpen(false) : void load()}>
      {t(open ? 'chat.historyClose' : 'chat.historyOpen')}
    </button>
    {open && <div class="compaction-history-records">
      <p>{t('chat.historyExplanation')}</p>
      {busy && <p role="status">{t('chat.historyLoading')}</p>}
      {error && <p role="alert">{t('chat.historyFailed')} {error} <button onClick={() => void load()}>{t('chat.historyRetry')}</button></p>}
      {turns?.length === 0 && <p>{t('chat.historyEmpty')}</p>}
      {turns?.map(turn => <article key={turn.turn_id}>
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
