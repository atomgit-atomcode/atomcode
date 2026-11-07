import assert from 'node:assert/strict';
import test from 'node:test';
import { goalRecoveryDisplay } from './goalRecovery.ts';

const quota = { kind: 'provider' as const, waiting_for_quota: true, attempt: 1, max_attempts: 12, retry_in_secs: 30 };
test('quota wait counts down locally and zero means checking, never recovered', () => {
  assert.deepEqual(goalRecoveryDisplay(quota, 1000, 6500), { key: 'goal.recovery.quota', secs: 25 });
  assert.deepEqual(goalRecoveryDisplay(quota, 1000, 31000), { key: 'goal.recovery.quotaChecking', secs: 0 });
  assert.deepEqual(goalRecoveryDisplay(quota, 1000, 61000), { key: 'goal.recovery.quotaChecking', secs: 0 });
});
test('a new allowance sample resets the local clock, legacy recovery keeps its labels', () => {
  assert.equal(goalRecoveryDisplay(quota, 60000, 59000).secs, 30);
  assert.equal(goalRecoveryDisplay({ ...quota, waiting_for_quota: false }, 1000, 6000).key, 'goal.recovery.provider');
  assert.equal(goalRecoveryDisplay({ ...quota, waiting_for_quota: false, kind: 'evaluator' }, 1000, 6000).key, 'goal.recovery.evaluator');
});

test('loop quota recovery is labelled as the same iteration, never a goal', () => {
  assert.deepEqual(goalRecoveryDisplay(quota, 1000, 6000, 'loop'), { key: 'loop.recovery.quota', secs: 25 });
  assert.deepEqual(goalRecoveryDisplay(quota, 1000, 31000, 'loop'), { key: 'loop.recovery.quotaChecking', secs: 0 });
});
