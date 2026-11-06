import assert from 'node:assert/strict';
import test from 'node:test';
import { sameConditions, scoreLabel, safeSource, taskComparable, type Evaluation, type CardTask } from './modelCards.ts';
test('comparison requires the same benchmark and declared conditions', () => {
  const a: Evaluation = { model: 'a', bench: 'math', score: .8, source: 'https://example.test/a', source_id: 'run', by: 'independent', note: '0-shot' };
  assert.ok(sameConditions(a, { ...a, model: 'b', score: .9 }));
  for (const difference of [{ bench: 'math-v2' }, { note: '5-shot' }, { source_id: 'other-run' }, { mode: 'thinking' }, { by: 'lab' }, { configuration: { temperature: 1 } }, { date: '2026-01-01' }, { source: 'https://example.test/other' }]) assert.equal(sameConditions(a, { ...a, ...difference }), false);
  const task = { id: 'coding', comparison_set: { id: 'scope', label: 'scope' } } as CardTask;
  assert.ok(taskComparable(task, task));
  assert.equal(taskComparable(task, { ...task, comparison_set: { id: 'other', label: 'scope' } }), false);
  assert.equal(taskComparable(task, { ...task, comparison_set: null }), false);
});
test('metrics keep their original scale and notes reject executable URLs', () => {
  assert.equal(scoreLabel(.82, { name: 'test', kind: 'acc' }), '82.0%');
  assert.equal(scoreLabel(1450, { name: 'test', kind: 'elo' }), '1450.0');
  assert.equal(scoreLabel(.8, { name: 'test', kind: 'acc', raw_scale_min: 0, raw_scale_max: 10 }), '8.00 / 10');
  assert.equal(safeSource('javascript:alert(1)'), undefined);
  assert.equal(safeSource('file:///private/path'), undefined);
  assert.equal(safeSource('https://example.test/source'), 'https://example.test/source');
});
