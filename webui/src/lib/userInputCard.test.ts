import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

test('structured input modal ignores backdrop clicks and offers an explicit close action', () => {
  const card = readFileSync(join(root, 'src/components/UserInputCard.tsx'), 'utf8');

  assert.match(card, /class="modal-overlay"[\s\S]*?onPointerDown=\{\(event\) => \{[\s\S]*?event\.stopPropagation\(\);/);
  assert.match(card, /onClick=\{\(event\) => event\.stopPropagation\(\)\}/);
  assert.match(card, /aria-label=\{t\('userInput\.close'\)\}/);
  assert.match(card, /onClose=\{\(\) => void skip\(\)\}/);
  assert.match(card, /onClose=\{\(\) => void skipAll\(\)\}/);
});

test('structured input modal closes only after the daemon accepts the response', () => {
  const card = readFileSync(join(root, 'src/components/UserInputCard.tsx'), 'utf8');
  const acceptedGuards = card.match(/if \(!result\.accepted\) throw new Error/g) ?? [];

  // Single submit/skip and batch submit/skip must all retain the modal on rejection.
  assert.equal(acceptedGuards.length, 4);
  assert.doesNotMatch(card, /await submitAnswer\([^;]+\);\s*onDone\(\);/s);
});
