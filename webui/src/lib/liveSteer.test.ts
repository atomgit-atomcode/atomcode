import { test } from 'node:test';
import assert from 'node:assert/strict';
import { acknowledgeLiveSteers, pendingSteersToDraft } from './liveSteer.ts';

test('acknowledgeLiveSteers consumes only matching FIFO inputs', () => {
  const pending = [
    { id: 'one-id', text: 'one', confirmed: true },
    { id: 'two-id', text: 'two', images: [{ media_type: 'image/png', data: 'abc' }], confirmed: true },
  ];

  assert.deepEqual(
    acknowledgeLiveSteers(pending, [{ text: 'peer input', images: [] }]),
    pending,
  );
  assert.deepEqual(
    acknowledgeLiveSteers(
      pending,
      [{ text: 'VL-preprocessed text', images: [] }],
      ['one-id'],
    ),
    [pending[1]],
  );
  assert.deepEqual(
    acknowledgeLiveSteers(pending, [{ text: 'one', images: [] }]),
    [pending[1]],
  );
  assert.deepEqual(
    acknowledgeLiveSteers(pending, [
      { text: 'one', images: [] },
      { text: 'two', images: [{ media_type: 'image/png', data: 'abc' }] },
    ]),
    [],
  );
});

test('pendingSteersToDraft preserves text and image order', () => {
  assert.deepEqual(
    pendingSteersToDraft([
      { id: 'first-id', text: 'first', confirmed: true },
      { id: 'second-id', text: 'second', images: [{ media_type: 'image/jpeg', data: 'xyz' }], confirmed: true },
    ]),
    {
      text: 'first\nsecond',
      images: [{ media_type: 'image/jpeg', data: 'xyz' }],
    },
  );
});
