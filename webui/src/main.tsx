import { apiFetch, hasRelayPairing } from './lib/transport';
import { render } from 'preact';
import { App } from './app';
import { SettingsProvider } from './settings';
// Bundled serif for the landing greeting (close to claude.ai's display serif).
import '@fontsource/source-serif-4/400.css';
import '@fontsource/source-serif-4/500.css';
// KaTeX 数学渲染样式（字体经 Vite 打包进 dist，随二进制嵌入）。
import 'katex/dist/katex.min.css';
import './styles/theme.css';
import './styles/app.css';
import './index.css';

import { AuthRequired } from './components/AuthRequired';
import { CompatNotice } from './components/CompatNotice';
import { signedIn, probeWebuiAccess } from './lib/compat';
import { createAccessRecovery } from './lib/accessRecovery';
import { getToken } from './api';

import { useEffect, useRef, useState } from 'preact/hooks';
import { UNAUTHORIZED_EVENT } from './components/LoginButton';

/**
 * The app, or — when the daemon does not know this page — where to get the
 * link that it does know.
 *
 * Asked once before the app mounts (opened without the link the terminal
 * printed) and again whenever the app hears a 401 (the webui restarted under
 * an open page). Revalidate on a protected endpoint and retry without
 * unmounting an already-open App: one failed relay request is not permanent
 * evidence of a missing link.
 */
function Root({ signedIn: initially }: { signedIn: boolean }) {
  const [ok, setOk] = useState(initially);
  const [mounted, setMounted] = useState(initially);
  const [checking, setChecking] = useState(false);
  const recoveryRef = useRef<ReturnType<typeof createAccessRecovery> | null>(null);
  useEffect(() => {
    const recovery = createAccessRecovery({
      probe: () => probeWebuiAccess(apiFetch, getToken()),
      onChecking: setChecking,
      onResult: (result) => {
        if (result === 'allowed') { setMounted(true); setOk(true); }
        else if (result === 'denied') setOk(false);
        // An offline relay is not evidence that the browser lost authorization.
      },
    });
    recoveryRef.current = recovery;
    const check = () => { void recovery.check(); };
    const visible = () => { if (document.visibilityState === 'visible') check(); };
    window.addEventListener(UNAUTHORIZED_EVENT, check);
    window.addEventListener('online', check);
    window.addEventListener('pageshow', check);
    document.addEventListener('visibilitychange', visible);
    if (!initially) check();
    return () => {
      recovery.dispose();
      recoveryRef.current = null;
      window.removeEventListener(UNAUTHORIZED_EVENT, check);
      window.removeEventListener('online', check);
      window.removeEventListener('pageshow', check);
      document.removeEventListener('visibilitychange', visible);
    };
  }, []);
  return <>
    {/* Preserve drafts, subscriptions and scroll intent during access recovery.
        Protected requests still require the existing server-side credentials. */}
    {mounted && <div style={{ display: 'contents', visibility: ok ? 'visible' : 'hidden' }} aria-hidden={!ok}><App /></div>}
    {!ok && <div class="auth-required-overlay"><AuthRequired
      recovering={mounted || hasRelayPairing()}
      checking={checking}
      onRetry={() => { void recoveryRef.current?.check(); }}
    /></div>}
  </>;
}

// The installed app's answer for when the webui is not running (`public/sw.js`):
// a page that says how to start it, instead of the browser's "can't reach this
// page". Only where the browser allows a worker — localhost is a secure
// context; a plain-http LAN address is not, and simply goes without.
if ('serviceWorker' in navigator && window.isSecureContext) {
  navigator.serviceWorker.register('./sw.js', { scope: './' }).catch(() => {
    /* no offline page; everything else works as before */
  });
}

void signedIn(apiFetch, getToken()).then((ok) => {
  render(
    <SettingsProvider>
      <CompatNotice />
      <Root signedIn={ok} />
    </SettingsProvider>,
    document.getElementById('app')!,
  );
});
