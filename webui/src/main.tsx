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

// Asked before the app mounts: opened without the link the terminal printed,
// every request the app makes would be refused, and what it would draw is an
// empty shell. A definite 401 shows where to get the link instead.
void signedIn(fetch, getToken()).then((ok) => {
  render(
    <SettingsProvider>
      <CompatNotice />
      {ok ? <App /> : <AuthRequired />}
    </SettingsProvider>,
    document.getElementById('app')!,
  );
});
