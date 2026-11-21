import { useEffect, useState } from 'preact/hooks';
import { useT } from '../settings';

/** Fast cache reads stay silent. Slow reads occupy their own row, never the
 * message below; the timer is cancelled on completion, failure or unmount. */
export function HistoryLoading({ active }: { active: boolean }) {
  const t = useT();
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    if (!active) { setVisible(false); return; }
    const timer = setTimeout(() => setVisible(true), 300);
    return () => clearTimeout(timer);
  }, [active]);
  if (!active || !visible) return null;
  return <div class="history-loading" role="status" aria-label={t('chat.historyLoading')}>
    <span class="history-loading-spinner" aria-hidden="true" />
  </div>;
}
