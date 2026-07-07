import test from 'node:test';
import assert from 'node:assert/strict';
import {
  beginApprovalModeSwitch,
  completeApprovalModeSwitch,
  failApprovalModeSwitch,
  initApprovalModeState,
} from '../../src/chat/modeState';

test('approval mode state keeps confirmed mode while a switch is pending', () => {
  assert.deepEqual(beginApprovalModeSwitch(initApprovalModeState('build'), 'plan'), {
    confirmedMode: 'build',
    displayMode: 'plan',
    pendingMode: 'plan',
  });
});

test('approval mode state accepts daemon-confirmed success', () => {
  const pending = beginApprovalModeSwitch(initApprovalModeState('build'), 'bypass');

  assert.deepEqual(completeApprovalModeSwitch(pending, 'bypass'), {
    confirmedMode: 'bypass',
    displayMode: 'bypass',
  });
});

test('approval mode state rolls back display mode on failure', () => {
  const pending = beginApprovalModeSwitch(initApprovalModeState('build'), 'plan');

  assert.deepEqual(failApprovalModeSwitch(pending), {
    confirmedMode: 'build',
    displayMode: 'build',
  });
});

test('approval mode state ignores a second selection while pending', () => {
  const pending = beginApprovalModeSwitch(initApprovalModeState('build'), 'plan');

  assert.deepEqual(beginApprovalModeSwitch(pending, 'bypass'), pending);
});
