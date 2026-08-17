import { test } from 'node:test';
import assert from 'node:assert';
import { reduceTodoPanelVisibility } from './todoState.ts';

test('a completed turn keeps its task card until the next real user input', () => {
  let visible = reduceTodoPanelVisibility(false, { type: 'todo_call', success: true });
  assert.equal(visible, true);
  // Turn completion is deliberately not a visibility event.
  visible = reduceTodoPanelVisibility(visible, { type: 'user_input' });
  assert.equal(visible, false);
});

test('a later TodoWrite reveals the card while a failed call does not', () => {
  let visible = reduceTodoPanelVisibility(true, { type: 'user_input' });
  visible = reduceTodoPanelVisibility(visible, { type: 'todo_call', success: false });
  assert.equal(visible, false);
  visible = reduceTodoPanelVisibility(visible, { type: 'todo_call' });
  assert.equal(visible, true);
});

test('user cancellation retires the visible task card', () => {
  assert.equal(
    reduceTodoPanelVisibility(true, { type: 'user_cancel' }),
    false,
  );
});
