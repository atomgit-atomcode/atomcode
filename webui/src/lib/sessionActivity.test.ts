import { test } from 'node:test';
import assert from 'node:assert/strict';
import { pollSessionActivity, sessionActivityBadge } from './sessionActivity.ts';

function harness() {
  const tasks: (() => void)[] = [];
  return {
    tasks,
    schedule(task: () => void) {
      tasks.push(task);
      return () => { const i = tasks.indexOf(task); if (i >= 0) tasks.splice(i, 1); };
    },
    async flush() { tasks.shift()?.(); await new Promise(resolve => setImmediate(resolve)); },
  };
}

test('slow project scans never overlap and the next timer starts after completion', async () => {
  const clock = harness();
  let resolve!: (value: string) => void;
  const calls: string[] = [];
  const results: string[] = [];
  const stop = pollSessionActivity({
    hashes: ['one', 'two'], delay: 5, hidden: () => false, schedule: clock.schedule,
    load: async hash => { calls.push(hash); return hash === 'one' ? new Promise<string>(r => { resolve = r; }) : hash; },
    apply: (_, value) => results.push(value),
  });
  await clock.flush();
  assert.deepEqual(calls, ['one']);
  assert.equal(clock.tasks.length, 0, 'no timer can start another scan while the request is pending');
  resolve('one');
  await new Promise(r => setImmediate(r));
  assert.deepEqual(calls, ['one', 'two']);
  assert.deepEqual(results, ['one', 'two']);
  assert.equal(clock.tasks.length, 1);
  stop();
  assert.equal(clock.tasks.length, 0);
});

test('teardown aborts a pending request and discards even an uncooperative late response', async () => {
  const clock = harness();
  let resolve!: (value: string) => void;
  let signal!: AbortSignal;
  const results: string[] = [];
  const stop = pollSessionActivity({
    hashes: ['old', 'never'], delay: 5, hidden: () => false, schedule: clock.schedule,
    load: (_, s) => { signal = s; return new Promise<string>(r => { resolve = r; }); },
    apply: (_, value) => results.push(value),
  });
  await clock.flush();
  stop();
  assert.equal(signal.aborted, true);
  resolve('stale');
  await new Promise(r => setImmediate(r));
  assert.deepEqual(results, []);
  assert.equal(clock.tasks.length, 0);
});

test('hidden pages skip requests and failed projects do not stop later projects', async () => {
  const clock = harness();
  let hidden = true;
  const results: string[] = [];
  const stop = pollSessionActivity({
    hashes: ['broken', 'ok'], delay: 5, hidden: () => hidden, schedule: clock.schedule,
    load: async hash => { if (hash === 'broken') throw Error('offline'); return hash; },
    apply: (_, value) => results.push(value),
  });
  await clock.flush();
  assert.deepEqual(results, []);
  hidden = false;
  await clock.flush();
  assert.deepEqual(results, ['ok']);
  stop();
});

test('waiting takes precedence for both the current and an external active turn', () => {
  assert.equal(sessionActivityBadge('waiting', true, true), 'waiting');
  assert.equal(sessionActivityBadge('waiting', false, false), 'waiting');
  assert.equal(sessionActivityBadge(undefined, true, true), 'running');
  assert.equal(sessionActivityBadge('running', false, false), 'running');
  assert.equal(sessionActivityBadge('open', false, true), undefined);
  assert.equal(sessionActivityBadge('open', false, false), 'open');
});
