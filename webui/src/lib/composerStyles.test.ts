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
