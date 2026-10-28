import { useT } from '../settings';

/**
 * What this page says when it was opened without the link the terminal
 * printed — in a second browser, from a bookmark, after the cookie was
 * cleared. Every request would be refused, and an empty app with nothing
 * loading reads as the service being down.
 *
 * The link is not single-use: the one `/webui` printed works in any browser
 * for as long as that webui runs, so that is where this points.
 */
export function AuthRequired({ recovering = false, checking = false, onRetry }: {
  recovering?: boolean; checking?: boolean; onRetry?: () => void;
} = {}) {
  const t = useT();
  return (
    <div class="auth-required">
      <div class="auth-required-card">
        <h1>{t(recovering ? 'auth.recover.title' : 'auth.required.title')}</h1>
        <p>{t(recovering ? 'auth.recover.body' : 'auth.required.body')}</p>
        <p class="auth-required-hint">{t('auth.required.hint')}</p>
        {onRetry && <button class="btn-auth-retry" onClick={onRetry} disabled={checking}>
          {t(checking ? 'auth.recover.checking' : 'auth.recover.retry')}
        </button>}
      </div>
    </div>
  );
}
