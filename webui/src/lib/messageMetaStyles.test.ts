import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

test('message time and copy controls are persistent and ordered by message side', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');
  const css = readFileSync(join(root, 'src/styles/app.css'), 'utf8');

  assert.match(chat, /msg-actions msg-actions-left[\s\S]*?\{copyBtn\}[\s\S]*?\{showTimestamp/);
  assert.match(chat, /class="msg-actions"[\s\S]*?msg-time msg-time-user[\s\S]*?\{copyBtn\}/);
  assert.doesNotMatch(css, /\.user-message-wrapper:hover \.msg-actions/);
  assert.doesNotMatch(css, /\.timeline-message:hover \.msg-actions/);
  assert.doesNotMatch(css, /\.msg-actions\s*\{[^}]*opacity:\s*0/s);
});

test('message times use a full local date with seconds', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');

  assert.match(chat, /d\.getFullYear\(\).*pad2\(d\.getMonth\(\) \+ 1\).*pad2\(d\.getDate\(\)\).*pad2\(d\.getHours\(\)\).*pad2\(d\.getMinutes\(\)\).*pad2\(d\.getSeconds\(\)\)/s);
  assert.doesNotMatch(chat, /function formatMsgTimeFull/);
});
