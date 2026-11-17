import { useEffect, useRef, useState } from 'preact/hooks';
import { createPortal } from 'preact/compat';
import { workbenchPreview } from '../api';
import { useT } from '../settings';
import './file-preview.css';

/** Read-only preview, scoped to the message's session even while another runs. */
export function FilePreviewDialog({ sessionId, path, onClose }: {
  sessionId: string | null; path: string; onClose: () => void;
}) {
  const t = useT();
  const dialog = useRef<HTMLDialogElement>(null);
  const [result, setResult] = useState<{ url: string; image: boolean; text: string }>();
  const [error, setError] = useState('');
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  useEffect(() => {
    const controller = new AbortController();
    let url = '';
    setResult(undefined); setError('');
    if (!sessionId) { setError(t('filePreview.noSession')); return; }
    void workbenchPreview(sessionId, path, controller.signal).then(async blob => {
      const image = /^image\/(png|jpeg|gif|webp)$/i.test(blob.type);
      const text = image ? '' : await blob.text();
      if (controller.signal.aborted) return;
      url = URL.createObjectURL(blob);
      setResult({ url, image, text });
    }).catch(error => {
      if (!controller.signal.aborted) setError(String(error));
    });
    return () => { controller.abort(); if (url) URL.revokeObjectURL(url); };
  }, [sessionId, path]);
  const filename = path.split(/[\\/]/).pop() || 'download';
  return createPortal(<dialog ref={dialog} class="file-preview-dialog" aria-label={t('filePreview.title')}
    onClose={onClose} onClick={event => { if (event.target === event.currentTarget) dialog.current?.close(); }}
    onKeyDown={event => { if (event.key === 'Escape') event.stopPropagation(); }}>
    <header>
      <div><strong>{t('filePreview.title')}</strong><p title={path}>{path}</p></div>
      {result && <a href={result.url} download={filename}>{t('filePreview.download')}</a>}
      <button type="button" autoFocus onClick={() => dialog.current?.close()} aria-label={t('filePreview.close')}>×</button>
    </header>
    <div class="file-preview-body">
      {error ? <div role="alert"><strong>{t('filePreview.failed')}</strong><p>{error}</p></div>
        : !result ? <p role="status">{t('filePreview.loading')}</p>
        : result.image ? <img src={result.url} alt={filename} onError={() => setError(t('filePreview.invalidImage'))} />
        : <pre tabIndex={0}>{result.text}</pre>}
    </div>
  </dialog>, document.body);
}
