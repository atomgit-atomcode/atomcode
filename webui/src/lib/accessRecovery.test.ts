import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createAccessRecovery } from './accessRecovery.ts';
import type { WebuiAccess } from './compat.ts';

function fixture(probe: () => Promise<WebuiAccess>) {
  const results: WebuiAccess[] = [];
  const checking: boolean[] = [];
  const timers = new Map<number, () => void>();
  let next = 0;
  const recovery = createAccessRecovery({
    probe, onResult: r => results.push(r), onChecking: v => checking.push(v),
    schedule: (run, delay) => { assert.equal(delay, 5000); timers.set(++next, run); return next as unknown as ReturnType<typeof setTimeout>; },
    cancel: id => { timers.delete(id as unknown as number); },
  });
  return { recovery, results, checking, timers };
}

test('a transient account 401 cannot latch access denied when the protected probe succeeds', async () => {
  const f = fixture(async () => 'allowed');
  await f.recovery.check();
  assert.deepEqual(f.results, ['allowed']);
  assert.equal(f.timers.size, 0);
  assert.deepEqual(f.checking, [true, false]);
});

test('rejected access retries, but network failures never count as restored authorization', async () => {
  const answers: WebuiAccess[] = ['denied', 'unavailable', 'allowed'];
  const f = fixture(async () => answers.shift()!);
  let gate = true;
  await f.recovery.check();
  gate = f.results.at(-1) === 'allowed';
  assert.equal(gate, false);
  assert.equal(f.timers.size, 1);
  await f.recovery.check();
  assert.equal(f.results.at(-1), 'unavailable');
  assert.equal(gate, false);
  assert.equal(f.timers.size, 1, 'only one retry timer');
  await f.recovery.check();
  assert.equal(f.results.at(-1), 'allowed');
  assert.equal(f.timers.size, 0);
});

test('foreground, online and unauthorized events share one probe; disposal ignores late replies', async () => {
  let calls = 0;
  let finish!: (r: WebuiAccess) => void;
  const f = fixture(() => { calls++; return new Promise(resolve => { finish = resolve; }); });
  const first = f.recovery.check();
  const second = f.recovery.check();
  assert.equal(first, second);
  assert.equal(calls, 1);
  f.recovery.dispose();
  finish('denied');
  await first;
  assert.deepEqual(f.results, []);
  assert.equal(f.timers.size, 0);
  await f.recovery.check();
  assert.equal(calls, 1);
});

test('retry is scheduled after completion and disposed timers cannot start new probes', async () => {
  let calls = 0;
  const f = fixture(async () => { calls++; return 'denied'; });
  await f.recovery.check();
  const retry = [...f.timers.values()][0];
  f.recovery.dispose();
  assert.equal(f.timers.size, 0);
  retry();
  await Promise.resolve();
  assert.equal(calls, 1);
});
