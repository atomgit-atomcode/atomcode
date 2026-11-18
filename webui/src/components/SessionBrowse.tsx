import { useEffect, useRef, useState } from 'preact/hooks';
import { getSessionPage, type DisplayHistoryCursor, type SessionMetaWithProject } from '../api';
import type { ComponentChildren } from 'preact';
import type { SessionMessage } from '../api';
import { useT } from '../settings';
import { createBrowseRefresh } from '../lib/sessionBrowse';

/** How often a session another runtime is running is re-read while watched. */
const LIVE_REFRESH_MS = 3_000;

/** A display-only reader: never attaches a runtime or subscribes to its events.
 *  `live` — another runtime is running the session — re-reads its newest page
 *  so the view follows the log that runtime is writing. */
export function SessionBrowse({ session, renderMessages, live = false }: {
  session: SessionMetaWithProject;
  renderMessages: (messages: SessionMessage[], viewport: () => HTMLDivElement | null, unsettled: boolean) => ComponentChildren;
  live?: boolean;
}) {
  const t = useT();
  const [messages, setMessages] = useState<SessionMessage[] | null>(null);
  const [older, setOlder] = useState<DisplayHistoryCursor>();
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState(false);
  const [retry, setRetry] = useState(0);
  // Do not render a completed footer against the last pre-terminal page.
  const [unsettled, setUnsettled] = useState(live);
  const reader = useRef<AbortController | null>(null);
  const loadingRef = useRef(false);
  const lastScroll = useRef(0);
  const viewport = useRef<HTMLDivElement | null>(null);
  // Messages prepended by `loadOlder`: a refresh replaces only the newest page,
  // so it holds off while the person reads further back.
  const earlierLoaded = useRef(0);
  const shown = useRef('');
  const wasLive = useRef(live);
  const finalRefreshPending = useRef(false);
  const refreshNow = useRef<() => void>(() => {});
  useEffect(() => {
    const controller = new AbortController();
    reader.current = controller;
    loadingRef.current = true; lastScroll.current = 0; earlierLoaded.current = 0; shown.current = '';
    setMessages(null); setOlder(undefined); setFailed(false); setLoading(false);
    if (!session.project_hash) { setFailed(true); return () => controller.abort(); }
    getSessionPage(session.project_hash, session.id, controller.signal).then(detail => {
      if (!controller.signal.aborted) {
        shown.current = JSON.stringify(detail.messages);
        setMessages(detail.messages);
        setOlder(detail.history_older);
        requestAnimationFrame(() => { if (!controller.signal.aborted && viewport.current) viewport.current.scrollTop = viewport.current.scrollHeight; });
      }
    }).catch(() => { if (!controller.signal.aborted) setFailed(true); })
      .finally(() => { if (reader.current === controller) loadingRef.current = false; });
    return () => { controller.abort(); reader.current?.abort(); };
  }, [session.id, session.project_hash, retry]);
  useEffect(() => {
    if (!session.project_hash) return;
    const hash = session.project_hash;
    if (live) setUnsettled(true);
    if (!live && wasLive.current) finalRefreshPending.current = true;
    wasLive.current = live;
    const ready = () => {
      if (document.hidden || loadingRef.current) return false;
      const el = viewport.current;
      const atBottom = !el || el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      return earlierLoaded.current === 0 || atBottom;
    };
    const refresh = createBrowseRefresh({
      needed: () => live || finalRefreshPending.current,
      ready,
      read: signal => getSessionPage(hash, session.id, signal),
      apply: detail => {
        finalRefreshPending.current = false;
        setUnsettled(live);
        const el = viewport.current;
        const atBottom = !el || el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        const text = JSON.stringify(detail.messages);
        if (text === shown.current && earlierLoaded.current === 0) return;
        shown.current = text;
        earlierLoaded.current = 0;
        setMessages(detail.messages);
        setOlder(detail.history_older);
        setFailed(false);
        if (atBottom) requestAnimationFrame(() => { if (active && viewport.current) viewport.current.scrollTop = viewport.current.scrollHeight; });
      },
    });
    let active = true;
    const run = () => { void refresh.run(); };
    refreshNow.current = () => { if (!live && finalRefreshPending.current) run(); };
    run();
    const timer = window.setInterval(run, LIVE_REFRESH_MS);
    document.addEventListener('visibilitychange', run);
    return () => {
      active = false;
      refresh.stop();
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', run);
      refreshNow.current = () => {};
    };
  }, [live, session.id, session.project_hash, retry]);
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
      earlierLoaded.current += detail.messages.length;
      setMessages(previous => [...detail.messages, ...(previous ?? [])]);
      setOlder(detail.history_older);
      requestAnimationFrame(() => { if (!controller.signal.aborted && el?.isConnected) el.scrollTop = top + el.scrollHeight - height; });
    } catch { if (!controller.signal.aborted) setFailed(true); }
    finally { if (reader.current === controller) { loadingRef.current = false; setLoading(false); } }
  }
  return <div class="messages-container" ref={viewport} onScroll={() => {
    const top = viewport.current?.scrollTop ?? 0;
    if (top < lastScroll.current && top < 80) void loadOlder();
    lastScroll.current = top;
    refreshNow.current();
  }} onWheel={event => {
    if (event.deltaY < 0 && (viewport.current?.scrollTop ?? 0) < 80) void loadOlder();
  }}><div class="timeline-inner">
    {older && <button class="history-load-more" disabled={loading} onClick={() => void loadOlder()}>{loading ? '…' : t('chat.loadEarlierHistory')}</button>}
    {failed && <div role="alert">{t('chat.historyFailed')}<button onClick={() => older ? void loadOlder() : setRetry(n => n + 1)}>{t('chat.historyRetry')}</button></div>}
    {messages === null ? !failed && <div role="status">{t('chat.historyLoading')}</div> : renderMessages(messages, () => viewport.current, live || unsettled)}
  </div></div>;
}
