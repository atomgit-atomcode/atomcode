import { test } from 'node:test';
import assert from 'node:assert/strict';
import { browseActiveTurn } from './browseTurn.ts';

test('a running checkpoint is active even when it already has a timestamp', () => {
  const messages = [{ role: 'user', ts: 1000 }, { role: 'assistant', ts: 2000 }];
  assert.deepEqual(browseActiveTurn(messages, true), { assistant: 1, startedAt: 1000 });
  assert.deepEqual(browseActiveTurn(messages, false), { assistant: -1, startedAt: undefined });
});
test('a new unanswered turn never relabels the previous completed assistant', () => {
  assert.deepEqual(browseActiveTurn([
    { role: 'user', ts: 1000 }, { role: 'assistant', ts: 2000 }, { role: 'user', ts: 3000 },
  ], true), { assistant: -1, startedAt: 3000 });
});
test('retained context does not restart the clock and missing clocks are not invented', () => {
  assert.deepEqual(browseActiveTurn([
    { role: 'user', ts: 1000 }, { role: 'user', ts: 2000, retainedHistory: true }, { role: 'assistant' },
  ], true), { assistant: 2, startedAt: 1000 });
  for (const ts of [undefined, NaN, Infinity, -1, 0]) {
    assert.equal(browseActiveTurn([{ role: 'user', ts }, { role: 'assistant' }], true).startedAt, undefined);
  }
});
