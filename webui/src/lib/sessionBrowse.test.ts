import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createBrowseRefresh, sessionSelectionDisposition } from './sessionBrowse.ts';

test('an executing session can be left for history and returned to without rebinding', () => {
  assert.equal(sessionSelectionDisposition('A', 'B', true, false), 'browse');
  assert.equal(sessionSelectionDisposition('A', 'C', true, true), 'browse');
  assert.equal(sessionSelectionDisposition('A', 'A', true, true), 'return');
});
test('a session another runtime holds is watched, not taken', () => {
  assert.equal(sessionSelectionDisposition('A', 'B', false, false, true), 'browse');
  assert.equal(sessionSelectionDisposition(null, 'B', false, false, true), 'browse');
  assert.equal(sessionSelectionDisposition('A', 'A', false, false, true), 'return');
});
test('completion does not turn subsequent historical navigation into runtime activation', () => {
  assert.equal(sessionSelectionDisposition('A', 'B', false, true), 'browse');
  assert.equal(sessionSelectionDisposition('A', 'A', false, true), 'return');
  assert.equal(sessionSelectionDisposition('A', 'B', false, false), 'activate');
  assert.equal(sessionSelectionDisposition(null, 'B', false, false), 'activate');
});

test('slow refreshes do not overlap or overwrite a newer page', async () => {
  let complete!: (value: string) => void;
  let reads = 0;
  const applied: string[] = [];
  const refresh = createBrowseRefresh({
    needed: () => true, ready: () => true,
    read: () => { reads++; return new Promise<string>(resolve => { complete = resolve; }); },
    apply: value => applied.push(value),
  });
  const first = refresh.run();
  await refresh.run();
  assert.equal(reads, 1);
  complete('first'); await first;
  const second = refresh.run();
  complete('newer'); await second;
  assert.deepEqual(applied, ['first', 'newer']);
});

test('the final refresh remains owed while hidden, reading history, or after a failure', async () => {
  let owed = true;
  let ready = false;
  let fail = true;
  let reads = 0;
  const refresh = createBrowseRefresh({
    needed: () => owed, ready: () => ready,
    read: async () => { reads++; if (fail) throw new Error('offline'); return 'final'; },
    apply: () => { owed = false; },
  });
  await refresh.run(); assert.equal(reads, 0); assert.equal(owed, true);
  ready = true;
  await refresh.run(); assert.equal(owed, true);
  fail = false;
  await refresh.run(); assert.equal(owed, false);
  await refresh.run(); assert.equal(reads, 2);
});

test('navigation discards a late refresh even when the request ignores abort', async () => {
  let complete!: (value: string) => void;
  let applied = false;
  let signal!: AbortSignal;
  const refresh = createBrowseRefresh({
    needed: () => true, ready: () => true,
    read: received => { signal = received; return new Promise<string>(resolve => { complete = resolve; }); },
    apply: () => { applied = true; },
  });
  const pending = refresh.run(); refresh.stop(); complete('old'); await pending;
  assert.equal(signal.aborted, true); assert.equal(applied, false);
});

test('history opened during a refresh keeps the update pending', async () => {
  let complete!: (value: string) => void;
  let ready = true;
  let owed = true;
  const refresh = createBrowseRefresh({
    needed: () => owed, ready: () => ready,
    read: () => new Promise<string>(resolve => { complete = resolve; }),
    apply: () => { owed = false; },
  });
  const pending = refresh.run(); ready = false; complete('final'); await pending;
  assert.equal(owed, true);
  ready = true;
  const retry = refresh.run(); complete('final'); await retry;
  assert.equal(owed, false);
});
