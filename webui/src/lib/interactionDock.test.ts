import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();
const read = (path: string) => readFileSync(join(root, path), 'utf8');

test('all turn-blocking interactions use the composer dock instead of modal overlays', () => {
  for (const path of [
    'src/components/PermissionCard.tsx',
    'src/components/UserInputCard.tsx',
    'src/components/PolicyInterventionCard.tsx',
  ]) {
    const source = read(path);
    assert.match(source, /<InteractionDock/);
    assert.doesNotMatch(source, /class="modal-overlay"/);
  }
});

test('chat replaces both regular and landing composers with one blocking interaction seat', () => {
  const chat = read('src/components/Chat.tsx');
  assert.match(chat, /pendingPermission/);
  assert.match(chat, /blockingInteraction/);
  assert.match(chat, /blockingInteraction \? \(/);
  assert.match(chat, /class="interaction-dock-seat"/);
});

test('interaction dock is non-modal, height-capped, and scrolls only its body', () => {
  const dock = read('src/components/InteractionDock.tsx');
  const css = read('src/styles/app.css');

  assert.match(dock, /role="region"/);
  assert.doesNotMatch(dock, /aria-modal/);
  assert.match(css, /\.interaction-dock-card[\s\S]*?max-height:/);
  assert.match(css, /\.interaction-dock-body[\s\S]*?overflow-y: auto/);
});

test('interaction dock owns focus safely and chat restores the composer afterwards', () => {
  const dock = read('src/components/InteractionDock.tsx');
  const chat = read('src/components/Chat.tsx');
  const userInput = read('src/components/UserInputCard.tsx');

  assert.match(dock, /tabIndex=\{-1\}/);
  assert.match(dock, /querySelector<HTMLElement>\('\[data-interaction-autofocus\]:not\(\[disabled\]\)'\)/);
  assert.match(dock, /\?\.focus\(\{ preventScroll: true \}\)/);
  assert.match(userInput, /data-interaction-autofocus/);
  assert.match(chat, /wasBlockingInteractionRef/);
  assert.match(chat, /textareaRef\.current\?\.focus\(\)/);
});

test('ordinary management dialogs remain modal', () => {
  assert.match(read('src/components/SettingsDialogs.tsx'), /class="modal-overlay"/);
  assert.match(read('src/components/SessionDialogs.tsx'), /class="modal-overlay"/);
  assert.match(read('src/components/CwdPicker.tsx'), /class="modal-overlay"/);
});
