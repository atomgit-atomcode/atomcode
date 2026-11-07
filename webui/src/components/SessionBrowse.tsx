import { useEffect, useRef, useState } from 'preact/hooks';
import { getSessionPage, type DisplayHistoryCursor, type SessionMetaWithProject } from '../api';
import { sessionMessagesToMarkdownLines } from '../lib/historyMessages';
import { useT } from '../settings';
import { Markdown } from './Markdown';

/** A display-only reader: never attaches a runtime or subscribes to its events. */
export function SessionBrowse({ session }: { session: SessionMetaWithProject }) {
  const t = useT();
  const [text, setText] = useState<string | null>(null);
  const [older, setOlder] = useState<DisplayHistoryCursor>();
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState(false);
  const [retry, setRetry] = useState(0);
  const reader = useRef<AbortController | null>(null);
  const loadingRef = useRef(false);
  const lastScroll = useRef(0);
  const viewport = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    reader.current = controller;
    loadingRef.current = false; lastScroll.current = 0;
    setText(null); setOlder(undefined); setFailed(false); setLoading(false);
    if (!session.project_hash) { setFailed(true); return () => controller.abort(); }
    getSessionPage(session.project_hash, session.id, controller.signal).then(detail => {
      if (!controller.signal.aborted) {
        setText(sessionMessagesToMarkdownLines(detail.messages, detail.name).join('\n'));
        setOlder(detail.history_older);
        requestAnimationFrame(() => { if (!controller.signal.aborted && viewport.current) viewport.current.scrollTop = viewport.current.scrollHeight; });
      }
    }).catch(() => { if (!controller.signal.aborted) setFailed(true); });
    return () => { controller.abort(); reader.current?.abort(); };
  }, [session.id, session.project_hash, retry]);
  async function loadOlder() {
    if (!older || loadingRef.current || !session.project_hash) return;
    loadingRef.current = true;
    setFailed(false);
    const controller = new AbortController();
    reader.current = controller;
    setLoading(true);
    try {
      const detail = await getSessionPage(session.project_hash, session.id, controller.signal, older);
      if (controller.signal.aborted) return;
      const el = viewport.current;
      const height = el?.scrollHeight ?? 0;
      const top = el?.scrollTop ?? 0;
      setText(previous => sessionMessagesToMarkdownLines(detail.messages, '').join('\n') + '\n\n' + previous);
      setOlder(detail.history_older);
      requestAnimationFrame(() => { if (!controller.signal.aborted && el?.isConnected) el.scrollTop = top + el.scrollHeight - height; });
    } catch { if (!controller.signal.aborted) setFailed(true); }
    finally { if (reader.current === controller) { loadingRef.current = false; setLoading(false); } }
  }
  return <div class="messages-container" ref={viewport} onScroll={() => {
    const top = viewport.current?.scrollTop ?? 0;
    if (top < lastScroll.current && top < 80) void loadOlder();
    lastScroll.current = top;
  }} onWheel={event => {
    if (event.deltaY < 0 && (viewport.current?.scrollTop ?? 0) < 80) void loadOlder();
  }}><div class="timeline-inner">
    {older && <button class="history-load-more" disabled={loading} onClick={() => void loadOlder()}>{loading ? '…' : t('chat.loadEarlierHistory')}</button>}
    {failed && <div role="alert">{t('chat.historyFailed')}<button onClick={() => setRetry(n => n + 1)}>{t('chat.historyRetry')}</button></div>}
    {text === null ? !failed && <div role="status">{t('chat.historyLoading')}</div> : <Markdown content={text} />}
  </div></div>;
}
