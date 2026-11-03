import { test } from 'node:test';
import assert from 'node:assert/strict';
import { sessionSelectionDisposition } from './sessionBrowse.ts';

test('an executing session can be left for history and returned to without rebinding', () => {
  assert.equal(sessionSelectionDisposition('A', 'B', true, false), 'browse');
  assert.equal(sessionSelectionDisposition('A', 'C', true, true), 'browse');
  assert.equal(sessionSelectionDisposition('A', 'A', true, true), 'return');
});
test('completion does not turn subsequent historical navigation into runtime activation', () => {
  assert.equal(sessionSelectionDisposition('A', 'B', false, true), 'browse');
  assert.equal(sessionSelectionDisposition('A', 'A', false, true), 'return');
  assert.equal(sessionSelectionDisposition('A', 'B', false, false), 'activate');
  assert.equal(sessionSelectionDisposition(null, 'B', false, false), 'activate');
});
