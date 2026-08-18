import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

test('landing composer starts at two text rows and keeps shared auto-growth behavior', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');
  const css = readFileSync(join(root, 'src/styles/app.css'), 'utf8');

  assert.match(chat, /class="message-input"\s*\n\s*rows=\{2\}/);
  assert.match(css, /\.message-input\s*\{[^}]*min-height:\s*3em;/s);
  assert.doesNotMatch(css, /\.landing-inner \.message-input\s*\{[^}]*min-height:/s);
  assert.match(chat, /ta\.style\.height = Math\.min\(ta\.scrollHeight, 160\) \+ 'px';/);
});

test('composer leaves native IME text uncontrolled until composition commits', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');

  assert.match(chat, /value=\{composing \? undefined : input\}/);
  assert.match(chat, /onCompositionStart=\{handleCompositionStart\}/);
  assert.match(chat, /onCompositionEnd=\{handleCompositionEnd\}/);
  assert.match(chat, /composingRef\.current \|\| e\.isComposing \|\| e\.keyCode === 229/);
});

test('landing review shortcut uses the supported review command', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');
  const i18n = readFileSync(join(root, 'src/i18n.ts'), 'utf8');

  assert.match(chat, /insert: '\/review '/);
  assert.doesNotMatch(chat, /insert: '\/code-review '/);
  assert.match(i18n, /'chat\.chipReview': '\/review /);
  assert.doesNotMatch(i18n, /'chat\.chipReview': '\/code-review /);
});

test('mobile composer isolates selectors from the send tap target', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');
  const css = readFileSync(join(root, 'src/styles/app.css'), 'utf8');

  assert.match(chat, /class="input-footer-primary"/);
  assert.match(chat, /class="input-footer-actions"/);
  assert.match(chat, /class="input-turn-controls"/);
  assert.match(css, /@media \(max-width: 768px\)[\s\S]*?\.input-footer\s*\{[^}]*flex-direction:\s*column;/);
  assert.match(css, /\.input-footer-actions\s*\{[^}]*display:\s*grid;[^}]*grid-template-columns:\s*auto minmax\(0, 1fr\) auto;/);
  assert.match(css, /\.input-turn-controls\s*>\s*\.btn-send,[\s\S]*?width:\s*44px;[\s\S]*?height:\s*44px;/);
  assert.match(css, /\.input-footer-actions \.model-controls\s*>\s*\.model-selector:not\(\.effort-selector\)\s*\{[^}]*flex:\s*1 1 0;[^}]*max-width:\s*none;/);
  assert.match(css, /\.input-footer-actions \.model-selector-trigger,[\s\S]*?min-height:\s*44px;/);
  assert.match(css, /padding:\s*6px 8px max\(8px, env\(safe-area-inset-bottom\)\);/);
});
