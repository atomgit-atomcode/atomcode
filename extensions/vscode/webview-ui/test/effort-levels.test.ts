import test from 'node:test';
import assert from 'node:assert/strict';
import { effortLabelFor, effortOptionsFor } from '../src/components/effortOptions';

const DEFAULT_LABEL = 'Default';

test('options mirror the levels the selected model declares', () => {
  // lumen-alpha declares low/medium/high — max must not be offered for it.
  assert.deepEqual(effortOptionsFor(['low', 'medium', 'high'], DEFAULT_LABEL), [
    { value: null, label: DEFAULT_LABEL },
    { value: 'low', label: 'Low' },
    { value: 'medium', label: 'Medium' },
    { value: 'high', label: 'High' },
  ]);
});

test('a different model offers a different set', () => {
  // deepseek-flash declares high/max only.
  assert.deepEqual(
    effortOptionsFor(['high', 'max'], DEFAULT_LABEL).map((option) => option.value),
    [null, 'high', 'max'],
  );
});

test('switching models re-derives the options', () => {
  const lumen = effortOptionsFor(['low', 'medium', 'high'], DEFAULT_LABEL);
  const deepseek = effortOptionsFor(['high', 'max'], DEFAULT_LABEL);

  assert.notDeepEqual(lumen, deepseek);
  assert.ok(!lumen.some((option) => option.value === 'max'));
  assert.ok(!deepseek.some((option) => option.value === 'low'));
});

test('a model that declares no levels keeps every level the wire accepts', () => {
  for (const levels of [undefined, null, []] as Array<string[] | null | undefined>) {
    assert.deepEqual(
      effortOptionsFor(levels, DEFAULT_LABEL).map((option) => option.value),
      [null, 'low', 'medium', 'high', 'xhigh', 'max'],
    );
  }
});

test('the auto sentinel never becomes a switchable level', () => {
  assert.deepEqual(
    effortOptionsFor(['auto', 'high'], DEFAULT_LABEL).map((option) => option.value),
    [null, 'high'],
  );
});

test('the trigger label follows the level in force and falls back to Default', () => {
  const options = effortOptionsFor(['low', 'medium', 'high'], DEFAULT_LABEL);

  assert.equal(effortLabelFor(options, 'medium', DEFAULT_LABEL), 'Medium');
  assert.equal(effortLabelFor(options, null, DEFAULT_LABEL), DEFAULT_LABEL);
  // A stale value the endpoint no longer declares shows "Default" rather than a raw token.
  assert.equal(effortLabelFor(options, 'max', DEFAULT_LABEL), DEFAULT_LABEL);
});
