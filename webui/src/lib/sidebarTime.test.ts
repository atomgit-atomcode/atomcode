import { test } from 'node:test';
import assert from 'node:assert';
import { sidebarRelativeTime } from './sidebarTime.ts';

test('workspace session time stays compact from minutes through years', () => {
  const now = 1_800_000_000_000;
  assert.deepEqual(sidebarRelativeTime(now, now), { unit: 'now', n: 0 });
  assert.deepEqual(sidebarRelativeTime(now - 45 * 60_000, now), { unit: 'minutes', n: 45 });
  assert.deepEqual(sidebarRelativeTime(now - 8 * 3_600_000, now), { unit: 'hours', n: 8 });
  assert.deepEqual(sidebarRelativeTime(now - 3 * 86_400_000, now), { unit: 'days', n: 3 });
  assert.deepEqual(sidebarRelativeTime(now - 90 * 86_400_000, now), { unit: 'months', n: 3 });
  assert.deepEqual(sidebarRelativeTime(now - 730 * 86_400_000, now), { unit: 'years', n: 2 });
});

test('workspace session time accepts unix seconds and clamps future timestamps to now', () => {
  const now = 1_800_000_000_000;
  assert.deepEqual(sidebarRelativeTime((now - 2 * 86_400_000) / 1000, now), { unit: 'days', n: 2 });
  assert.deepEqual(sidebarRelativeTime(now + 60_000, now), { unit: 'now', n: 0 });
});
