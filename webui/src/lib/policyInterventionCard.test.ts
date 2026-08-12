import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();

test('policy recovery acknowledgements never submit a model prompt', () => {
  const card = readFileSync(join(root, 'src/components/PolicyInterventionCard.tsx'), 'utf8');
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');

  assert.match(card, /const accepted = await onResolve\(action\)/);
  assert.match(chat, /postLivePolicyInterventionResolution\(policyIntervention\.intervention_id, action\)/);
  assert.doesNotMatch(card, /onSubmit|COMPLETE_EXTERNALLY_MESSAGE|SKIP_STEP_MESSAGE/);
});

test('both live and ordinary chat transports consume policy interventions', () => {
  const chat = readFileSync(join(root, 'src/components/Chat.tsx'), 'utf8');
  const handlers = chat.match(/case 'policy_intervention'/g) ?? [];

  assert.equal(handlers.length, 2);
  assert.match(chat, /case 'policy_intervention_resolved':[\s\S]*?intervention_id/);
  assert.match(chat, /case 'policy_intervention_cleared':[\s\S]*?intervention_id/);
});

test('recovery card stays mounted until the terminal and gates decisions on busy', () => {
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
