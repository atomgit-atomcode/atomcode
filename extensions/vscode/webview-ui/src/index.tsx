import { createRoot } from 'react-dom/client';
import { App } from './App';
import './styles/variables.css';
import './styles/index.css';
import './styles/components.css';
import './styles/messages.css';
import './styles/input.css';
import './styles/highlight.css';

/* Detect VS Code theme and set body class for syntax highlighting */
(function detectTheme() {
  const bg = getComputedStyle(document.body).getPropertyValue('--vscode-editor-background').trim();
  if (!bg) return;
  // Parse hex or rgb color and compute luminance
  const m = bg.match(/[\d.]+/g);
  if (!m || m.length < 3) return;
  const [r, g, b] = m.map(Number);
  const lum = 0.299 * r + 0.587 * g + 0.114 * b;
  if (lum > 160) {
    document.body.classList.add('vscode-light');
  }
})();

const root = document.getElementById('root');
if (root) {
  createRoot(root).render(<App />);
}
