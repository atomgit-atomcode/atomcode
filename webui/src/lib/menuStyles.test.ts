import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

test('/ and @ menus share command menu selection colors', () => {
  const css = readFileSync(join(root, 'src/styles/app.css'), 'utf8');
  const theme = readFileSync(join(root, 'src/styles/theme.css'), 'utf8');

  assert.match(theme, /--app-command-menu-hover-background:/);
  assert.match(theme, /--app-command-menu-active-background:/);
  assert.match(theme, /--app-command-menu-active-background:color-mix\(in srgb,var\(--app-brand\)/);
  assert.match(theme, /--app-command-menu-active-accent:/);
  assert.match(css, /\.slash-row\.active\s*\{\s*background:\s*var\(--app-command-menu-active-background\)/);
  assert.match(css, /\.at-row\.active\s*\{\s*background:\s*var\(--app-command-menu-active-background\)/);
  assert.match(css, /\.slash-row\.active\s*\{[^}]*--app-command-menu-active-accent/s);
  assert.match(css, /\.at-row\.active\s*\{[^}]*--app-command-menu-active-accent/s);
  assert.match(css, /\.slash-row:hover\s*\{\s*background:\s*var\(--app-command-menu-hover-background\)/);
  assert.match(css, /\.at-row:hover\s*\{\s*background:\s*var\(--app-command-menu-hover-background\)/);
});
