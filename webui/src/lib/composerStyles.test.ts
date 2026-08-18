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

test('composer mirrors IME drafts without un-controlling the textarea', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');

  // Always controlled: Preact only writes dom.value when it differs from state,
  // so mirroring every input event (composition drafts included) never disturbs
  // the IME pre-edit buffer, and an unrelated re-render cannot write a stale
  // value back. The old `value={composing ? undefined : input}` un-control hack
  // could read a not-yet-committed ta.value at compositionend (WebKit) and then
  // force-write the stale value, wiping the just-committed IME text.
  assert.match(chat, /value=\{input\}/);
  assert.doesNotMatch(chat, /value=\{composing \? undefined : input\}/);
  // handleInput must mirror every input event — no composition early-return.
  assert.doesNotMatch(chat, /if \(composingRef\.current \|\| \(e as InputEvent\)\.isComposing\) return;/);
  // compositionend must not commit from ta.value; the trailing input event
  // (inputType=insertCompositionText) carries the final committed text.
  assert.doesNotMatch(chat, /function handleCompositionEnd[\s\S]*?commitComposerInput/);
  // The keydown guard that keeps menu navigation / Enter-to-send out of the IME
  // candidate window must stay (Safari reports composition as keyCode 229).
  assert.match(chat, /composingRef\.current \|\| e\.isComposing \|\| e\.keyCode === 229/);
  assert.match(chat, /onCompositionStart=\{handleCompositionStart\}/);
  assert.match(chat, /onCompositionEnd=\{handleCompositionEnd\}/);
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
