import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PromptHistory, promptHistoryDirection } from './promptHistory.ts';

test('arrows traverse user entries, clamp at oldest and restore the original draft', () => {
  const history = new PromptHistory<string>();
  const entries = ['first', 'second'];
  assert.deepEqual(history.move('one', entries, 'draft', -1), { entry: 'second' });
  history.accept('second');
  assert.deepEqual(history.move('one', entries, 'second', -1), { entry: 'first' });
  history.accept('first');
  assert.equal(history.move('one', entries, 'first', -1), null);
  assert.deepEqual(history.move('one', entries, 'first', 1), { entry: 'second' });
  history.accept('second');
  assert.deepEqual(history.move('one', entries, 'second', 1), { text: 'draft' });
  assert.equal(history.move('one', entries, 'draft', 1), null);
  assert.deepEqual(history.move('one', entries, 'draft', -1), { entry: 'second' });
});

test('edits and session changes reset browsing; new live entries do not shift an active cursor', () => {
  const history = new PromptHistory<string>();
  history.move('one', ['old'], '', -1); history.accept('old');
  assert.deepEqual(history.move('one', ['old', 'new live input'], 'old', 1), { text: '' });
  history.move('one', ['old'], '', -1); history.accept('old');
  assert.equal(history.move('two', ['different session'], 'old', 1), null);
  assert.deepEqual(history.move('two', ['different session'], 'edited draft', -1), { entry: 'different session' });
  history.accept('different session');
  assert.equal(history.move('two', ['different session'], 'manual edit', 1), null);
  history.reset();
  assert.equal(history.move('two', [], '', -1), null);
});

test('multiline boundaries, selections, and modified arrow keys retain native editing', () => {
  const event = { key: 'ArrowUp', shiftKey: false, ctrlKey: false, altKey: false, metaKey: false };
  assert.equal(promptHistoryDirection(event, 'one\ntwo', 2, 2), -1);
  assert.equal(promptHistoryDirection(event, 'one\ntwo', 5, 5), null);
  assert.equal(promptHistoryDirection({ ...event, key: 'ArrowDown' }, 'one\ntwo', 2, 2), null);
  assert.equal(promptHistoryDirection({ ...event, key: 'ArrowDown' }, 'one\ntwo', 5, 5), 1);
  assert.equal(promptHistoryDirection(event, 'selected', 0, 2), null);
  for (const modifier of ['shiftKey', 'ctrlKey', 'altKey', 'metaKey']) {
    assert.equal(promptHistoryDirection({ ...event, [modifier]: true }, '', 0, 0), null);
  }
});
