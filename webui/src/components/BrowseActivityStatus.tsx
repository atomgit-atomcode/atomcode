import { useEffect, useState } from 'preact/hooks';
import { useT } from '../settings';
import { formatTurnDuration } from '../lib/turnStats';
import './browse-activity-status.css';

export function BrowseActivityStatus({ state, startedAt }: {
  state: 'running' | 'waiting' | 'syncing'; startedAt?: number;
}) {
  const t = useT();
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (state === 'syncing' || startedAt == null) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [state, startedAt]);
  return <div class="msg-actions msg-actions-left browse-activity-status" role="status" aria-live="off">
    <span>{t(state === 'syncing' ? 'browse.syncingFinal' : state === 'waiting' ? 'browse.waiting' : 'browse.running')}</span>
    {state !== 'syncing' && startedAt != null && <><span aria-hidden="true">·</span><span class="browse-elapsed">{t('browse.elapsed', {
      duration: formatTurnDuration(Math.floor(Math.max(0, now - startedAt) / 1000) * 1000),
    })}</span></>}
  </div>;
}
