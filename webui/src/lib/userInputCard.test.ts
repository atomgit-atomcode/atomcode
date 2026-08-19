import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

test('structured input uses the shared composer dock and offers an explicit close action', () => {
  const card = readFileSync(join(root, 'src/components/UserInputCard.tsx'), 'utf8');

  assert.match(card, /<InteractionDock/);
  assert.doesNotMatch(card, /class="modal-overlay"/);
  assert.match(card, /label: t\('userInput\.close'\)/);
  assert.match(card, /onClose=\{\(\) => void skip\(\)\}/);
  assert.match(card, /onClose=\{\(\) => void skipAll\(\)\}/);
});

test('structured input dock closes only after the daemon accepts the response', () => {
  const card = readFileSync(join(root, 'src/components/UserInputCard.tsx'), 'utf8');
  const acceptedGuards = card.match(/if \(!result\.accepted\) throw new Error/g) ?? [];

  // Single submit/skip and batch submit/skip must all retain the dock on rejection.
  assert.equal(acceptedGuards.length, 4);
  assert.doesNotMatch(card, /await submitAnswer\([^;]+\);\s*onDone\(\);/s);
});
