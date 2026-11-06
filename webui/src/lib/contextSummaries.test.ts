import test from 'node:test';
import assert from 'node:assert/strict';
import { nextContextSummary } from './contextSummaries.ts';
test('absolute summary counts restore silently and replay is idempotent', () => {
  assert.deepEqual(nextContextSummary(null, 3), { count: 3, notice: false });
  assert.deepEqual(nextContextSummary(3, 3, true), { count: 3, notice: false });
  assert.deepEqual(nextContextSummary(3, 4, true), { count: 4, notice: true });
  assert.deepEqual(nextContextSummary(4, 3), { count: 4, notice: false });
  assert.deepEqual(nextContextSummary(4), { count: 4, notice: false });
  assert.deepEqual(nextContextSummary(null, 0), { count: 0, notice: false });
  for (const value of [-1, NaN, 1.5, Infinity]) assert.deepEqual(nextContextSummary(3, value, true), { count: 3, notice: false });
});
