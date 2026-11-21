import { test } from 'node:test';
import assert from 'node:assert/strict';
import { resolvePendingAfterDecision } from './pendingPermission.ts';

test('late answer does not remove a newer approval reusing the provider tool ID', () => {
  const pending = {call_id:'reused',approval_id:'new'};
  assert.equal(resolvePendingAfterDecision(pending,'reused','old'),pending);
  assert.equal(resolvePendingAfterDecision(pending,'reused','new'),null);
});
