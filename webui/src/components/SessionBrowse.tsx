import { useEffect, useState } from 'preact/hooks';
import { getSession, type SessionMetaWithProject } from '../api';
import { sessionMessagesToMarkdownLines } from '../lib/historyMessages';
import { useT } from '../settings';
import { Markdown } from './Markdown';

/** A display-only reader: never attaches a runtime or subscribes to its events. */
export function SessionBrowse({ session }: { session: SessionMetaWithProject }) {
  const t = useT();
  const [text, setText] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setText(null); setFailed(false);
    if (!session.project_hash) { setFailed(true); return () => controller.abort(); }
    getSession(session.project_hash, session.id, controller.signal).then(detail => {
      if (!controller.signal.aborted) setText(sessionMessagesToMarkdownLines(detail.messages, detail.name).join('\n'));
    }).catch(() => { if (!controller.signal.aborted) setFailed(true); });
    return () => controller.abort();
  }, [session.id, session.project_hash, retry]);
  return <div class="messages-container"><div class="timeline-inner">
    {failed ? <div role="alert">{t('chat.historyFailed')}<button onClick={() => setRetry(n => n + 1)}>{t('chat.historyRetry')}</button></div>
      : text === null ? <div role="status">{t('chat.historyLoading')}</div> : <Markdown content={text} />}
  </div></div>;
}
