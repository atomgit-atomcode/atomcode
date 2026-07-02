import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  applyAtMentionSelection,
  detectAtMentionRange,
  ensureActiveDescendantVisible,
  replaceAtMention,
  splitAtToken,
} from '../src/utils/atMention';

const root = join(process.cwd(), 'webview-ui');

test('detects @ mentions only at start or after whitespace', () => {
  assert.deepEqual(detectAtMentionRange('@src', 4), {
    start: 0,
    end: 4,
    token: 'src',
  });
  assert.deepEqual(detectAtMentionRange('open @src', 9), {
    start: 5,
    end: 9,
    token: 'src',
  });
  assert.equal(detectAtMentionRange('name@example.com', 'name@example.com'.length), null);
});

test('splits @ mention tokens into scope directory and filter', () => {
  assert.deepEqual(splitAtToken('extensions/vs'), {
    scopeDir: 'extensions/',
    filter: 'vs',
  });
});

test('replaces the selected @ mention without terminating the token', () => {
  const input = 'read @ext';
  const range = detectAtMentionRange(input, input.length);
  assert.ok(range);
  const next = replaceAtMention(input, range, 'extensions/vscode/');

  assert.deepEqual(next, {
    text: 'read @extensions/vscode/',
    cursor: 'read @extensions/vscode/'.length,
  });
  assert.deepEqual(detectAtMentionRange(next.text, next.cursor)?.token, 'extensions/vscode/');
});

test('keeps @ picker open after selecting a directory', () => {
  const input = 'read @ext';
  const range = detectAtMentionRange(input, input.length);
  assert.ok(range);

  assert.deepEqual(applyAtMentionSelection(input, range, 'extensions/vscode/', true), {
    text: 'read @extensions/vscode/',
    cursor: 'read @extensions/vscode/'.length,
    keepOpen: true,
    query: 'extensions/vscode/',
  });
});

test('closes @ picker after selecting a file', () => {
  const input = 'read @ext';
  const range = detectAtMentionRange(input, input.length);
  assert.ok(range);

  assert.deepEqual(applyAtMentionSelection(input, range, 'extensions/vscode/package.json', false), {
    text: 'read @extensions/vscode/package.json',
    cursor: 'read @extensions/vscode/package.json'.length,
    keepOpen: false,
    query: '',
  });
});

test('scrolls the active @ menu row into the visible list', () => {
  const container = { scrollTop: 36, clientHeight: 96 };

  ensureActiveDescendantVisible(container, { offsetTop: 150, offsetHeight: 28 });
  assert.equal(container.scrollTop, 82);

  ensureActiveDescendantVisible(container, { offsetTop: 24, offsetHeight: 28 });
  assert.equal(container.scrollTop, 24);
});

test('@ picker active row keeps text readable in light themes', () => {
  const css = readFileSync(join(root, 'src/styles/input.css'), 'utf8');
  const variables = readFileSync(join(root, 'src/styles/variables.css'), 'utf8');

  assert.match(css, /\.at-mention-picker\s+\.file-picker-item\.active/);
  assert.match(css, /--app-command-menu-active-background/);
  assert.match(variables, /--app-command-menu-active-background:\s*color-mix\(in srgb,\s*var\(--app-brand\)/);
  assert.match(css, /--app-command-menu-active-accent/);
  assert.doesNotMatch(
    css.match(/\.at-mention-picker\s+\.file-picker-item\.active\s*\{(?<body>[^}]+)\}/)?.groups?.body ?? '',
    /--app-menu-selection-background/,
  );
});

test('/ and @ pickers share command menu selection colors', () => {
  const css = readFileSync(join(root, 'src/styles/input.css'), 'utf8');

  assert.match(css, /\.slash-item\.active\s*\{[^}]*--app-command-menu-active-background/s);
  assert.match(css, /\.at-mention-picker\s+\.file-picker-item\.active\s*\{[^}]*--app-command-menu-active-background/s);
  assert.match(css, /\.slash-picker\.allow-hover\s+\.slash-item:hover\s*\{[^}]*--app-command-menu-hover-background/s);
  assert.match(css, /\.at-mention-picker\s+\.file-picker-item:hover\s*\{[^}]*--app-command-menu-hover-background/s);
  assert.match(css, /\.slash-item\.active\s*\{[^}]*--app-command-menu-active-accent/s);
  assert.match(css, /\.at-mention-picker\s+\.file-picker-item\.active\s*\{[^}]*--app-command-menu-active-accent/s);
});
