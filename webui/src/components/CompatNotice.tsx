import { useState } from 'preact/hooks';
import { useT } from '../settings';
import { compatNoticeDismissed, dismissCompatNotice, missingCssSince } from '../lib/compat';

/**
 * A line across the top for a browser engine older than the page was built
 * for — Huawei's (Chromium 99), older 360/QQ builds. The page still works
 * there (every feature it uses has a fallback), but it looks wrong in places,
 * and without this the first thought is that the service is broken.
 */
export function CompatNotice() {
  const t = useT();
  const [since] = useState(() => missingCssSince());
  const [hidden, setHidden] = useState(() => compatNoticeDismissed());
  if (since === null || hidden) return null;
  return (
    <div class="compat-notice" role="status">
      <span>{t('compat.oldBrowser', { version: String(since) })}</span>
      <button
        type="button"
        class="compat-notice-close"
        aria-label={t('compat.dismiss')}
        onClick={() => {
          dismissCompatNotice();
          setHidden(true);
        }}
      >
        ×
      </button>
    </div>
  );
}
