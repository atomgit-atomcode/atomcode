import { render } from 'preact';
import { App } from './app';
import { SettingsProvider } from './settings';
// Bundled serif for the landing greeting (close to claude.ai's display serif).
import '@fontsource/source-serif-4/400.css';
import '@fontsource/source-serif-4/500.css';
import './styles/theme.css';
import './styles/app.css';
import './index.css';

render(
  <SettingsProvider>
    <App />
  </SettingsProvider>,
  document.getElementById('app')!,
);
