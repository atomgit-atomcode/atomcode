import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  chatRecoveryPolicy,
  classifyChatDone,
  createLiveLifecycleState,
  isCurrentChatStream,
  liveDetachDisposition,
  liveSnapshotQueueDisposition,
  reduceChatRecovery,
  reduceLiveLifecycle,
  restoreLiveSnapshot,
  syncAttachDisposition,
} from './chatTerminal.ts';

test('legacy done and stopped are natural completions that preserve queued messages', () => {
  assert.deepEqual(classifyChatDone({}), {
    kind: 'completed',
    discardQueued: false,
  });
  assert.deepEqual(classifyChatDone({ stopReason: 'stopped' }), {
    kind: 'completed',
    discardQueued: false,
  });
});

test('budget and loop-guard terminals are incomplete and discard queued messages', () => {
  assert.deepEqual(
    classifyChatDone({
      stopReason: 'max_rounds',
      message: 'Turn reached its round budget',
    }),
    {
      kind: 'incomplete',
      discardQueued: true,
      detail: 'Turn reached its round budget',
    },
  );

  for (const stopReason of ['max_continuations', 'repeat_loop', 'tool_loop_detected']) {
    assert.deepEqual(classifyChatDone({ stopReason }), {
      kind: 'incomplete',
      discardQueued: true,
      detail: stopReason,
    });
  }
});

test('unknown explicit stop reasons fail closed instead of being reported as success', () => {
  assert.deepEqual(classifyChatDone({ stopReason: 'future_terminal' }), {
    kind: 'incomplete',
    discardQueued: true,
    detail: 'future_terminal',
  });
});

test('live errors are diagnostic until one authoritative idle state consumes the terminal', () => {
  let state = createLiveLifecycleState();
  ({ state } = reduceLiveLifecycle(state, { type: 'state', running: true }));

  const diagnostic = reduceLiveLifecycle(state, {
    type: 'error',
    message: 'provider stream failed',
  });
  state = diagnostic.state;
  assert.equal(state.running, true);
  assert.equal(diagnostic.diagnostic, 'provider stream failed');
  assert.equal(diagnostic.terminal, undefined);

  const terminal = reduceLiveLifecycle(state, {
    type: 'state',
    running: false,
    stopReason: 'tool_loop_detected',
  });
  state = terminal.state;
  assert.equal(state.running, false);
  assert.deepEqual(terminal.terminal, {
    kind: 'incomplete',
    discardQueued: true,
    detail: 'tool_loop_detected',
  });

  const duplicate = reduceLiveLifecycle(state, {
    type: 'state',
    running: false,
    stopReason: 'tool_loop_detected',
  });
  assert.equal(duplicate.terminal, undefined);
});

test('live snapshot never infers running from a trailing user message', () => {
  const restored = restoreLiveSnapshot([{ role: 'user', text: 'persisted but idle' }]);
  assert.deepEqual(restored, {
    messages: [{ role: 'user', text: 'persisted but idle' }],
    running: false,
  });
});

test('live replay input and running state restore an active turn after an idle snapshot', () => {
  let state = createLiveLifecycleState();
  ({ state } = reduceLiveLifecycle(state, { type: 'snapshot' }));
  assert.equal(state.running, false);

  ({ state } = reduceLiveLifecycle(state, { type: 'input_accepted' }));
  assert.equal(state.running, true);

  ({ state } = reduceLiveLifecycle(state, { type: 'state', running: true }));
  assert.equal(state.running, true);
});

test('an active live turn cannot be detached locally without a recoverable protocol', () => {
  assert.deepEqual(liveDetachDisposition(true), {
    allowed: false,
    reason: 'active_turn',
  });
  assert.deepEqual(liveDetachDisposition(false), { allowed: true });
});

test('reconnect snapshot discards an unresolved queue when terminal replay is unavailable', () => {
  assert.deepEqual(liveSnapshotQueueDisposition(true, 2), {
    discardQueued: true,
    reason: 'terminal_unknown',
  });
  assert.deepEqual(liveSnapshotQueueDisposition(false, 2), {
    discardQueued: false,
  });
  assert.deepEqual(liveSnapshotQueueDisposition(true, 0), {
    discardQueued: false,
  });
});

test('detached and unknown chat recovery states block sends and queue draining', () => {
  let state = reduceChatRecovery('ready', {
    type: 'session_switch',
    hasSession: true,
  });
  assert.equal(state, 'checking');
  assert.deepEqual(chatRecoveryPolicy(state), {
    allowSend: false,
    allowQueueDrain: false,
    allowStop: true,
  });

  state = reduceChatRecovery(state, { type: 'active_check_failed' });
  assert.equal(state, 'terminal_unknown');
  assert.equal(chatRecoveryPolicy(state).allowSend, false);

  state = reduceChatRecovery(state, { type: 'stop_failed' });
  assert.equal(state, 'terminal_unknown');
  state = reduceChatRecovery(state, { type: 'stop_succeeded' });
  assert.equal(state, 'ready');
});

test('detached active chat unlocks only after stop success or an authoritative terminal', () => {
  let state = reduceChatRecovery('checking', {
    type: 'active_check_succeeded',
    active: true,
  });
  assert.equal(state, 'detached_active');
  assert.equal(chatRecoveryPolicy(state).allowSend, false);
  assert.equal(chatRecoveryPolicy(state).allowQueueDrain, false);

  state = reduceChatRecovery(state, { type: 'authoritative_terminal' });
  assert.equal(state, 'ready');
  assert.deepEqual(chatRecoveryPolicy(state), {
    allowSend: true,
    allowQueueDrain: true,
    allowStop: false,
  });
});

test('sync cannot replace an active or unresolved non-sync transport', () => {
  assert.deepEqual(syncAttachDisposition(true, 'ready'), {
    allowed: false,
    reason: 'active_chat',
  });
  assert.deepEqual(syncAttachDisposition(false, 'terminal_unknown'), {
    allowed: false,
    reason: 'unresolved_chat',
  });
  assert.deepEqual(syncAttachDisposition(false, 'ready'), { allowed: true });
});

test('late chat events are rejected by request and session generation', () => {
  assert.equal(isCurrentChatStream('request-a', 3, 'request-a', 3, false), true);
  assert.equal(isCurrentChatStream('request-a', 3, 'request-b', 3, false), false);
  assert.equal(isCurrentChatStream('request-a', 3, 'request-a', 4, false), false);
  assert.equal(isCurrentChatStream('request-a', 3, 'request-a', 3, true), false);
});
