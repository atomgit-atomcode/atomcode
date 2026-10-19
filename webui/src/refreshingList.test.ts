import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRefreshingList } from './refreshingList.ts';

test('reopening a shared list discovers added skills in every subscriber', async () => {
  let disk = ['first'];
  const store = createRefreshingList(async () => [...disk]);
  const seen: string[][] = [];
  const stop = store.subscribe((s) => { if (s.items) seen.push(s.items); });
  await store.refresh(true);
  disk = ['first', 'added'];
  await store.refresh(true);
  assert.deepEqual(store.getSnapshot().items, disk);
  assert.deepEqual(seen.at(-1), disk);
  stop();
});

test('failure is retryable and does not become a cached empty success', async () => {
  let fail = true;
  const store = createRefreshingList(async () => {
    if (fail) throw new Error('offline');
    return ['skill'];
  });
  await store.refresh();
  assert.equal(store.getSnapshot().failed, true);
  assert.equal(store.getSnapshot().items, null);
  fail = false;
  await store.refresh();
  assert.deepEqual(store.getSnapshot().items, ['skill']);
  fail = true;
  await store.refresh();
  assert.deepEqual(store.getSnapshot().items, ['skill'], 'a failed refresh retains the last good result');
  assert.equal(store.getSnapshot().failed, true);
});

test('reload invalidation prevents a late old-project response from replacing new skills', async () => {
  let resolveOld!: (value: string[]) => void;
  let calls = 0;
  const store = createRefreshingList(() => ++calls === 1
    ? new Promise<string[]>((resolve) => { resolveOld = resolve; })
    : Promise.resolve(['new-project']));
  const stop = store.subscribe(() => {});
  const old = store.refresh();
  await Promise.resolve();
  store.invalidate();
  assert.equal(store.getSnapshot().items, null);
  await store.refresh();
  resolveOld(['old-project']);
  await old;
  assert.deepEqual(store.getSnapshot().items, ['new-project']);
  assert.equal(store.getSnapshot().loading, false);
  stop();
});

test('a fresh opening supersedes an in-flight result; ordinary retries coalesce', async () => {
  let resolveOld!: (value: string[]) => void;
  let calls = 0;
  const store = createRefreshingList(() => ++calls === 1
    ? new Promise<string[]>((resolve) => { resolveOld = resolve; })
    : Promise.resolve(['fresh']));
  const old = store.refresh();
  assert.equal(store.refresh(), old);
  await Promise.resolve();
  await store.refresh(true);
  resolveOld(['stale']);
  await old;
  assert.deepEqual(store.getSnapshot().items, ['fresh']);
});
