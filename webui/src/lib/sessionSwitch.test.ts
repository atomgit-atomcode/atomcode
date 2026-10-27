import test from 'node:test';
import assert from 'node:assert/strict';
import { loadAfterSessionSwitch } from './sessionSwitch.ts';

test('history is fetched only after the shared session switch is accepted', async () => {
  let accept!: (value: boolean) => void;
  const accepted = new Promise<boolean>(resolve => { accept = resolve; });
  let loads = 0;
  const result = loadAfterSessionSwitch(accepted, () => true, async () => ++loads);
  await Promise.resolve();
  assert.equal(loads, 0);
  accept(true);
  assert.equal(await result, 1);
});

test('a rejected or superseded switch never loads its history', async () => {
  let loads = 0;
  assert.equal(await loadAfterSessionSwitch(Promise.resolve(false), () => true, async () => ++loads), undefined);
  let accept!: (value: boolean) => void;
  const accepted = new Promise<boolean>(resolve => { accept = resolve; });
  let generation = 1;
  const result = loadAfterSessionSwitch(accepted, () => generation === 1, async () => ++loads);
  generation = 3;
  accept(true);
  assert.equal(await result, undefined);
  assert.equal(loads, 0);
  assert.equal(await loadAfterSessionSwitch(undefined, () => true, async () => ++loads), 1);
});
