import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = join(process.cwd(), 'webview-ui');

/** Windows checkouts are CRLF; the assertions below are written against LF source text. */
const readSource = (path: string) => readFileSync(path, 'utf8').replace(/\r\n/g, '\n');

test('slash command picker is bounded and scrollable like the @ picker', () => {
  const css = readSource(join(root, 'src/styles/input.css'));
  const block = css.match(/\.slash-picker\s*\{(?<body>[^}]+)\}/)?.groups?.body ?? '';

  assert.match(block, /max-height\s*:/);
  assert.match(block, /overflow-y\s*:\s*auto/);
});

test('slash command picker scrolls the active keyboard selection into view', () => {
  const source = readSource(join(root, 'src/components/SlashPicker.tsx'));

  assert.match(source, /ensureActiveDescendantVisible/);
  assert.match(source, /querySelector<HTMLButtonElement>\('\.slash-item\.active'\)/);
});

test('slash command picker avoids showing hover and keyboard selections at the same time', () => {
  const source = readSource(join(root, 'src/components/SlashPicker.tsx'));
  const css = readSource(join(root, 'src/styles/input.css'));

  assert.match(source, /allowHoverHighlight/);
  assert.match(source, /setAllowHoverHighlight\(false\)/);
  assert.match(source, /onMouseMove/);
  assert.match(css, /\.slash-picker\.allow-hover\s+\.slash-item:hover/);
});
