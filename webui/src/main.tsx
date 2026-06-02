import { render } from 'preact';
import { App } from './app';
import { SettingsProvider } from './settings';
import './styles/theme.css';
import './styles/app.css';
import './index.css';

render(
  <SettingsProvider>
    <App />
  </SettingsProvider>,
  document.getElementById('app')!,
);
