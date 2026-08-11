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

test('recovery card stays mounted through its own submit and gates on busy', () => {
  // Regression: gating the card's mount on `!busy` unmounted it the instant a
  // recovery submit flipped busy→true, so its loading/error state never rendered
  // and a failed submit was silently swallowed. The card must stay mounted and
  // instead disable its actions while busy.
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');
  assert.doesNotMatch(chat, /policyIntervention && !busy/);
  assert.match(chat, /policyIntervention && \(/);
  assert.match(chat, /<PolicyInterventionCard[\s\S]*?busy=\{busy\}/);

  const card = readFileSync(join(root, 'src/components/PolicyInterventionCard.tsx'), 'utf8');
  assert.match(card, /const disabled = loading \|\| busy;/);
  assert.match(card, /disabled=\{disabled\}/);
  assert.match(card, /if \(disabled\) return;/);
});
