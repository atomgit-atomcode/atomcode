import { test } from 'node:test';
import assert from 'node:assert';
import {
  beginModeSwitch,
  completeModeSwitch,
  failModeSwitch,
  initModeState,
  modeForQueuedPrompt,
  type ApprovalMode,
} from './modeSwitch.ts';

test('beginning a mode switch separates confirmed and display mode', () => {
  assert.deepEqual(beginModeSwitch(initModeState('build'), 'plan'), {
    confirmedMode: 'build' satisfies ApprovalMode,
    displayMode: 'plan' satisfies ApprovalMode,
    pendingMode: 'plan' satisfies ApprovalMode,
  });
});

test('successful mode switch promotes daemon-confirmed mode', () => {
  const pending = beginModeSwitch(initModeState('build'), 'plan');

  assert.deepEqual(completeModeSwitch(pending, 'plan'), {
    confirmedMode: 'plan' satisfies ApprovalMode,
    displayMode: 'plan' satisfies ApprovalMode,
  });
});

test('successful mode switch uses the daemon-confirmed mode even when it differs from the request', () => {
  const pending = beginModeSwitch(initModeState('build'), 'plan');

  assert.deepEqual(completeModeSwitch(pending, 'build'), {
    confirmedMode: 'build' satisfies ApprovalMode,
    displayMode: 'build' satisfies ApprovalMode,
  });
});

test('failed mode switch rolls display back to confirmed mode', () => {
  const pending = beginModeSwitch(initModeState('build'), 'bypass');

  assert.deepEqual(failModeSwitch(pending), {
    confirmedMode: 'build' satisfies ApprovalMode,
    displayMode: 'build' satisfies ApprovalMode,
  });
});

test('pending mode switch ignores a second user selection', () => {
  const pending = beginModeSwitch(initModeState('build'), 'plan');

  assert.deepEqual(beginModeSwitch(pending, 'bypass'), pending);
});

test('mode switch supports accept_edits', () => {
  const pending = beginModeSwitch(initModeState('build'), 'accept_edits');
  assert.deepEqual(pending, {
    confirmedMode: 'build' satisfies ApprovalMode,
    displayMode: 'accept_edits' satisfies ApprovalMode,
    pendingMode: 'accept_edits' satisfies ApprovalMode,
  });
  assert.deepEqual(completeModeSwitch(pending, 'accept_edits'), {
    confirmedMode: 'accept_edits' satisfies ApprovalMode,
    displayMode: 'accept_edits' satisfies ApprovalMode,
  });
});

test('queued prompts defer mode binding while a switch is pending', () => {
  assert.equal(modeForQueuedPrompt(initModeState('build')), 'build');
  assert.equal(
    modeForQueuedPrompt(beginModeSwitch(initModeState('build'), 'bypass')),
    undefined,
  );
});
