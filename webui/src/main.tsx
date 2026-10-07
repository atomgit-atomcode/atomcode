import { render } from 'preact';
import { App } from './app';
import { SettingsProvider } from './settings';
// Bundled serif for the landing greeting (close to claude.ai's display serif).
import '@fontsource/source-serif-4/400.css';
import '@fontsource/source-serif-4/500.css';
import './styles/theme.css';
import './styles/app.css';
import './index.css';

import { AuthRequired } from './components/AuthRequired';
import { CompatNotice } from './components/CompatNotice';
import { signedIn } from './lib/compat';
import { getToken } from './api';

import { useEffect, useState } from 'preact/hooks';
import { UNAUTHORIZED_EVENT } from './components/LoginButton';

/**
 * The app, or — when the daemon does not know this page — where to get the
 * link that it does know.
 *
 * Asked once before the app mounts (opened without the link the terminal
 * printed) and again whenever the app hears a 401 (the webui restarted under
 * an open page, with a new token): from then on every request would be
 * refused, signing in included, and what the app would show is a button that
 * does nothing.
 */
function Root({ signedIn: initially }: { signedIn: boolean }) {
  const [ok, setOk] = useState(initially);
  useEffect(() => {
    const lost = () => setOk(false);
    window.addEventListener(UNAUTHORIZED_EVENT, lost);
    return () => window.removeEventListener(UNAUTHORIZED_EVENT, lost);
  }, []);
  return ok ? <App /> : <AuthRequired />;
}

void signedIn(fetch, getToken()).then((ok) => {
  render(
    <SettingsProvider>
      <CompatNotice />
      <Root signedIn={ok} />
    </SettingsProvider>,
    document.getElementById('app')!,
  );
});
