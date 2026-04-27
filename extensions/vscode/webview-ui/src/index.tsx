import { createRoot } from 'react-dom/client';
import { App } from './App';
import './styles/variables.css';
import './styles/index.css';
import './styles/components.css';
import './styles/messages.css';
import './styles/input.css';

const root = document.getElementById('root');
if (root) {
  createRoot(root).render(<App />);
}
