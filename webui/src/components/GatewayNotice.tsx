import { useState } from 'preact/hooks';
import { useT } from '../settings';
import { gatewaySigningAvailable } from '../lib/androidHost';

/**
 * A line across the top for an Android build without the request-signing overlay.
 *
 * The AtomGit gateway (`llm-api.atomgit.com`) rejects any request that carries only
 * an OAuth bearer: it answers `ATOMCODE_SIG_MISSING` (403). The signature lives in
 * a closed-source crate the official pipeline drops in over the public stub, so a
 * build made without it cannot complete a single turn against that provider — and
 * the failure arrives mid-conversation, as a provider error that reads like an
 * outage or a bad key.
 *
 * Only the app can tell: `nativeGatewaySigningAvailable()` is about the linked
 * `.so`, which the page cannot see. A desktop build shares the limitation, but
 * there the CLI reports it; `gatewaySigningAvailable()` returns `null` outside the
 * Android host so this component stays out of the browser's way.
 *
 * Dismissible for the session only: the condition is permanent for this build, but
 * a person who has read it should not have it nag them while they configure a
 * different provider.
 */
export function GatewayNotice() {
  const t = useT();
  const [signing] = useState(() => gatewaySigningAvailable());
  const [hidden, setHidden] = useState(false);
  if (signing !== false || hidden) return null;
  return (
    <div class="compat-notice gateway-notice" role="status">
      <span>{t('gateway.unsigned')}</span>
      <button
        type="button"
        class="compat-notice-close"
        aria-label={t('gateway.dismiss')}
        onClick={() => setHidden(true)}
      >
        ×
      </button>
    </div>
  );
}
