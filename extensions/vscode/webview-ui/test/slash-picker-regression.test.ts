import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = join(process.cwd(), 'webview-ui');

test('slash command picker is bounded and scrollable like the @ picker', () => {
  const css = readFileSync(join(root, 'src/styles/input.css'), 'utf8');
  const block = css.match(/\.slash-picker\s*\{(?<body>[^}]+)\}/)?.groups?.body ?? '';

  assert.match(block, /max-height\s*:/);
  assert.match(block, /overflow-y\s*:\s*auto/);
});

test('slash command picker scrolls the active keyboard selection into view', () => {
  const source = readFileSync(join(root, 'src/components/SlashPicker.tsx'), 'utf8');

  assert.match(source, /ensureActiveDescendantVisible/);
  assert.match(source, /querySelector<HTMLButtonElement>\('\.slash-item\.active'\)/);
});

test('slash command picker avoids showing hover and keyboard selections at the same time', () => {
  const source = readFileSync(join(root, 'src/components/SlashPicker.tsx'), 'utf8');
  const css = readFileSync(join(root, 'src/styles/input.css'), 'utf8');

  assert.match(source, /allowHoverHighlight/);
  assert.match(source, /setAllowHoverHighlight\(false\)/);
  assert.match(source, /onMouseMove/);
  assert.match(css, /\.slash-picker\.allow-hover\s+\.slash-item:hover/);
});
