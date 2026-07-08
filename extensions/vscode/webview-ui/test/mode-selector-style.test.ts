import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';

const root = process.cwd();

test('input dropdown descriptions remain readable in active menu items', () => {
  const variables = readFileSync(join(root, 'webview-ui/src/styles/variables.css'), 'utf8');
  const components = readFileSync(join(root, 'webview-ui/src/styles/components.css'), 'utf8');
  const input = readFileSync(join(root, 'webview-ui/src/styles/input.css'), 'utf8');

  assert.match(variables, /--app-menu-selection-foreground:\s*var\(--vscode-menu-selectionForeground,/);

  assert.match(
    components,
    /\.model-item\.active\s+\.model-item-provider\s*\{[^}]*color:\s*var\(--app-list-active-foreground\);/s,
  );
  assert.match(
    components,
    /\.model-item\.active\s+\.model-default-badge\s*\{[^}]*opacity:\s*1;/s,
  );

  assert.match(
    input,
    /\.attach-menu-item:hover\s*\{[^}]*color:\s*var\(--app-menu-selection-foreground\);/s,
  );
  assert.match(
    input,
    /\.file-picker-item(?:\:hover|\.active),\s*\.file-picker-item(?:\:hover|\.active)\s+\.file-picker-item-name\s*\{[^}]*color:\s*var\(--app-menu-selection-foreground\);/s,
  );
  assert.match(
    input,
    /\.file-picker-item(?:\:hover|\.active)\s+\.file-picker-item-path\s*\{[^}]*color:\s*var\(--app-menu-selection-foreground\);/s,
  );
  assert.match(
    input,
    /\.slash-item\.active\s+\.slash-item-desc\s*\{[^}]*color:\s*var\(--app-command-menu-active-foreground\);/s,
  );
});
