import { test } from 'node:test';
import assert from 'node:assert/strict';
import { acknowledgeLiveSteers, pendingSteersToDraft, reconcileSteerReceipt } from './liveSteer.ts';

test('reconcileSteerReceipt keeps a steered submit pending when no terminal was observed', () => {
  // The bug: a tab that observes a live turn without a fresh `state{running:true}`
  // (attached mid-turn, or before the first running state) has running=false while
  // the runtime is genuinely active. A `steered` receipt is authoritative — the
  // runtime folded the input — so it must NOT be rolled back into the composer.
  assert.equal(
    reconcileSteerReceipt('steered', { running: false, terminalConsumed: false }),
    'confirm',
  );
  assert.equal(
    reconcileSteerReceipt('steered', { running: true, terminalConsumed: false }),
    'confirm',
  );
});

test('reconcileSteerReceipt releases a steered submit that raced the turn terminal', () => {
  // The receipt raced the turn terminal: the runtime accepted the input but the
  // turn ended before folding it. The kernel re-runs such leftover steers as the
  // next turn (agent.rs leftover-steer drain), so the client must NOT bounce it
  // back to the composer (that breaks TUI parity and, since the runtime already
  // re-runs it, would duplicate). Just drop the pending marker and defer to the
  // runtime's authoritative re-run.
  assert.equal(
    reconcileSteerReceipt('steered', { running: false, terminalConsumed: true }),
    'release',
  );
});

test('reconcileSteerReceipt clears the pending marker for a started turn', () => {
  // disposition `started` means a new turn began; the submit IS that turn's input,
  // so drop the pending steer marker without restoring.
  assert.equal(
    reconcileSteerReceipt('started', { running: true, terminalConsumed: false }),
    'clear',
  );
  assert.equal(
    reconcileSteerReceipt('started', { running: false, terminalConsumed: false }),
    'clear',
  );
});

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
