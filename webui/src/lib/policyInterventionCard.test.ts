import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

test('policy recovery closes only after a recovery turn is accepted', () => {
  const card = readFileSync(join(root, 'src/components/PolicyInterventionCard.tsx'), 'utf8');

  assert.match(card, /const accepted = await onSubmit\(/);
  assert.match(card, /if \(!accepted\) throw new Error/);
  assert.match(card, /catch \{[\s\S]*?setError\(true\);/);
  assert.ok(card.indexOf('if (!accepted)') < card.indexOf('onClose();', card.indexOf('const accepted')));
});

test('both live and ordinary chat transports consume policy interventions', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');
  const handlers = chat.match(/case 'policy_intervention'/g) ?? [];

  assert.equal(handlers.length, 2);
});
