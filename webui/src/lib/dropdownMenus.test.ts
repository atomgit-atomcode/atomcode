import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

/**
 * The three composer dropdowns (mode / model / reasoning effort) are portaled
 * to `<body>` and placed against the viewport — see `useDropdown`.
 *
 * That has one sharp edge: because the menu is no longer inside the
 * trigger's container, a click on a menu item looks like a click "outside".
 * A component-local outside-click handler therefore closes the menu on
 * mousedown, and the item never receives its click: the model and effort
 * menus became impossible to use on a phone, while the mode menu (which had
 * no such handler left) still worked.
 *
 * These tests pin down that the outside-click decision lives in ONE place —
 * `useDropdown` — and that it knows about the menu itself.
 */
test('the outside-click decision lives in the shared hook, not in the selectors', () => {
  const mode = readFileSync(join(root, 'src/components/ModeSelector.tsx'), 'utf8');
  const model = readFileSync(join(root, 'src/components/ModelSelector.tsx'), 'utf8');
  const hook = readFileSync(join(root, 'src/lib/useDropdown.ts'), 'utf8');

  // Neither component may register its own document-level mousedown handler:
  // it would close a portaled menu before the item receives the click.
  assert.doesNotMatch(mode, /addEventListener\(['"]mousedown['"]/);
  assert.doesNotMatch(model, /addEventListener\(['"]mousedown['"]/);

  // Both use the shared hook instead.
  assert.match(mode, /useDropdown\(\{/);
  assert.match(model, /useDropdown\(\{/);
  assert.match(hook, /addEventListener\(['"]mousedown['"]/);
});

test('the shared hook treats a click inside the portaled menu as inside', () => {
  const hook = readFileSync(join(root, 'src/lib/useDropdown.ts'), 'utf8');

  // The menu is checked explicitly: `menuRef.current.contains(target)`.
  assert.match(hook, /menuRef\.current\?\.contains\(target\)/);
  assert.match(hook, /triggerRef\.current\?\.contains\(target\)/);
});

test('each dropdown menu is portaled to the body so the drawer cannot clip it', () => {
  const mode = readFileSync(join(root, 'src/components/ModeSelector.tsx'), 'utf8');
  const model = readFileSync(join(root, 'src/components/ModelSelector.tsx'), 'utf8');

  for (const source of [mode, model]) {
    assert.match(source, /createPortal\(/);
    assert.match(source, /document\.body/);
    // The menu carries the viewport-placed variant, positioned by the hook.
    assert.match(source, /model-dropdown-viewport/);
  }
});
