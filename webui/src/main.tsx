import { render } from 'preact';
import { App } from './app';
import './styles/theme.css';
import './styles/app.css';
import './index.css';

render(<App />, document.getElementById('app')!);
