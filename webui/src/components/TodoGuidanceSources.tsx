import { useLayoutEffect, useRef, useState } from 'preact/hooks';
import { getTodoSources, type TodoSourceView, type TodoSourcePage } from '../api';
import { useT } from '../settings';
import type { TodoItem } from '../lib/todoState';

/** Discover linked sources up front; read original text only on explicit selection. */
export function TodoGuidanceSources(props: { projectHash: string; sessionId: string; items: TodoItem[]; revision: number }) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const [view, setView] = useState<TodoSourceView | null>(null);
  const [page, setPage] = useState<TodoSourcePage | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const epoch = useRef(0);
  const pending = useRef<AbortController | null>(null);
  const signature = JSON.stringify(props.items);
  const current = useRef(signature); current.current = signature;
  function cancel() { epoch.current += 1; pending.current?.abort(); pending.current = null; }
  useLayoutEffect(() => {
    cancel(); setOpen(false); setView(null); setPage(null); setError(''); setBusy(false);
    void load();
    return cancel;
  }, [props.projectHash, props.sessionId, signature, props.revision]);

  async function load(sourceSeq?: number, offset = 0) {
    cancel(); const run = epoch.current;
    const controller = new AbortController(); pending.current = controller;
    setBusy(true); setError(''); setPage(null);
    if (sourceSeq == null) setView(null);
    try {
      const result = await getTodoSources(props.projectHash, props.sessionId, controller.signal,
        sourceSeq == null ? undefined : { source_seq: sourceSeq, batch_seq: view!.batch_seq!, batch_index: view!.batch_index, offset });
      if (controller.signal.aborted || run !== epoch.current) return;
      if (sourceSeq == null) {
        if (JSON.stringify(result.todos) !== current.current) throw new Error(t('todo.source.changed'));
        setView(result); setPage(null);
      } else {
        if (result.batch_seq !== view?.batch_seq || result.batch_index !== view.batch_index || !result.page || result.page.seq !== sourceSeq || result.page.session_id !== props.sessionId) throw new Error(t('todo.source.changed'));
        setPage(result.page);
      }
    } catch (failure) {
      if (controller.signal.aborted || run !== epoch.current) return;
      setView(null); setPage(null);
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally { if (run === epoch.current) setBusy(false); }
  }
  function toggle() {
    if (open) { cancel(); setOpen(false); setBusy(false); }
    else { setOpen(true); }
  }
  // Unknown/loading is distinct from a successful empty result. Failed discovery
  // remains retryable instead of masquerading as "no sources".
  if (!view) return error ? <div class="todo-guidance-sources"><p role="alert">{error} <button type="button" class="btn" onClick={() => void load()}>{t('todo.source.retry')}</button></p></div> : null;
  if (!view.sources.length) return null;
  return <div class="todo-guidance-sources">
    <button type="button" class="btn btn-ghost" aria-expanded={open} onClick={toggle}>{t('todo.source.open', { count: view.sources.length })}</button>
    {open && <div>
      <p class="todo-source-hint">{t('todo.source.reference')}</p>
      {busy && <p role="status">{t('todo.source.loading')}</p>}
      {error && <p role="alert">{error} <button type="button" class="btn" onClick={() => void load()}>{t('todo.source.retry')}</button></p>}
      {view?.sources.map(source => <button key={source.seq} type="button" class="btn btn-ghost" disabled={busy} onClick={() => void load(source.seq)}>{t('todo.source.message', { seq: source.seq })} · {source.chars.toLocaleString()} {t('todo.source.characters')}</button>)}
      {page && <div class="todo-source-page">
        <div>{t('todo.source.message', { seq: page.seq })} · {page.offset.toLocaleString()}–{(page.offset + Array.from(page.text).length).toLocaleString()} / {page.total_chars.toLocaleString()}</div>
        <pre>{page.text}</pre>
        <div class="todo-source-pagination">
          <button type="button" class="btn" disabled={busy || page.offset === 0} onClick={() => void load(page.seq, Math.max(0, page.offset - 4096))}>{t('todo.source.previous')}</button>
          <button type="button" class="btn" disabled={busy || page.next_offset == null} onClick={() => page.next_offset != null && void load(page.seq, page.next_offset)}>{t('todo.source.next')}</button>
        </div>
      </div>}
    </div>}
  </div>;
}
