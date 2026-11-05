import { useEffect, useRef, useState } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import { getHooks, testHook, type HookCatalogue, type HookTestResult } from '../api';
import { useSettings } from '../settings';
import './hooksDialog.css';

export function HooksDialog({ onClose }: { onClose: () => void }) {
  const { t } = useSettings();
  const [data, setData] = useState<HookCatalogue | null>(null);
  const [selected, setSelected] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [running, setRunning] = useState(false);
  const runningRef = useRef(false);
  const [result, setResult] = useState<HookTestResult | null>(null);
  const alive = useRef(true);
  const dialog = useRef<HTMLElement>(null);
  const request = useRef<AbortController | null>(null);
  async function refresh() {
    request.current?.abort();
    const controller = new AbortController(); request.current = controller;
    setLoading(true); setError(''); setResult(null);
    try {
      const value = await getHooks(controller.signal);
      if (alive.current && !controller.signal.aborted) {
        setData(value); setSelected(current => value.items.some(item => item.id === current) ? current : value.items[0]?.id ?? '');
      }
    } catch (e) { if (alive.current && !controller.signal.aborted) setError(String(e)); }
    finally { if (alive.current && !controller.signal.aborted) setLoading(false); }
  }
  useEffect(() => {
    alive.current = true;
    const previous = document.activeElement as HTMLElement | null;
    dialog.current?.querySelector<HTMLButtonElement>('button')?.focus();
    void refresh();
    return () => { alive.current = false; request.current?.abort(); if (previous?.isConnected) previous.focus(); };
  }, []);
  useEffect(() => {
    const key = (event: KeyboardEvent) => {
      if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); if (!runningRef.current) onClose(); }
      if (event.key === 'Tab') {
        const nodes = Array.from(dialog.current?.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled)') ?? []);
        if (nodes.length === 0) { event.preventDefault(); dialog.current?.focus(); return; }
        const first = nodes[0], last = nodes[nodes.length - 1];
        if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
      }
    };
    window.addEventListener('keydown', key, true); return () => window.removeEventListener('keydown', key, true);
  }, []);
  const item = data?.items.find(item => item.id === selected);
  async function run() {
    if (!item || !item.testable || runningRef.current || loading) return;
    runningRef.current = true;
    setRunning(true); setError(''); setResult(null);
    try { const value = await testHook(item.id); if (alive.current) setResult(value.result); }
    catch (e) { if (alive.current) setError(String(e)); }
    finally { runningRef.current = false; if (alive.current) setRunning(false); }
  }
  return createPortal(<div class="modal-overlay" onClick={e => { if (e.target === e.currentTarget && !runningRef.current) onClose(); }}>
    <section ref={dialog} tabIndex={-1} class="modal-card hooks-modal" role="dialog" aria-modal="true" aria-labelledby="hooks-title">
      <header class="modal-header"><h3 id="hooks-title">{t('hooks.title')}</h3><button type="button" class="ghost-btn modal-close" disabled={running} onClick={() => { if (!runningRef.current) onClose(); }} aria-label={t('hooks.close')}>×</button></header>
      <div class="modal-body hooks-body">
        <p>{t('hooks.note')}</p>
        {data && <p class="hooks-path">{data.cwd}</p>}
        <button type="button" class="btn" onClick={() => void refresh()} disabled={loading || running}>{t('hooks.refresh')}</button>
        {loading && <p role="status">{t('settings.loading')}</p>}
        {error && <p role="alert" class="modal-error">{error}</p>}
        {data && <>
          <details><summary>{t('hooks.paths')}</summary>{data.files.map(file => <div class="hooks-source" key={file.path}>
            <strong>{file.scope}</strong><div class="hooks-path">{file.path}</div><pre>{JSON.stringify(file.status, null, 2)}</pre>
          </div>)}{data.plugins.map(plugin => <div key={plugin.plugin}>{plugin.plugin} · {plugin.trusted ? t('hooks.trusted') : t('hooks.untrusted')}{plugin.invalid ? ` · ${t('hooks.invalid')}` : ''}</div>)}</details>
          {data.items.length === 0 && <p>{t('hooks.empty')}</p>}
          <div class="hooks-layout"><nav aria-label={t('hooks.title')}>{data.items.map(entry => <button type="button" class={'hooks-entry' + (entry.id === selected ? ' active' : '')} key={entry.id} disabled={running} aria-pressed={entry.id === selected} onClick={() => { setSelected(entry.id); setResult(null); setError(''); }}>
            <strong>{entry.event}</strong><span>{entry.name}</span><small>{entry.scope} · {entry.disabled ? t('hooks.disabled') : entry.testable ? t('hooks.configured') : t('hooks.invalid')}</small>
          </button>)}</nav>
          {item && <div class="hooks-detail"><h4>{item.name}</h4><p>{item.event} · {item.matcher ?? '*'} · {item.timeout_ms} ms</p><div class="hooks-path">{item.source}</div><pre>{item.command}</pre>{Boolean(item.args?.length) && <pre>args: {JSON.stringify(item.args)}</pre>}
            <button type="button" class="btn" disabled={!item.testable || running || loading} onClick={run}>{running ? t('hooks.running') : t('hooks.test')}</button>
            {result && <div class="hooks-result" role="status"><p>{result.completed ? `${t('hooks.exit')} ${result.exit_code ?? '—'}` : t('hooks.incomplete')}</p><h5>stdout</h5><pre>{result.stdout || '—'}</pre><h5>stderr</h5><pre>{result.stderr || '—'}</pre></div>}
          </div>}</div>
        </>}
      </div>
    </section>
  </div>, document.body);
}
