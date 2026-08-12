import { test } from 'node:test';
import assert from 'node:assert/strict';
import { shouldSendComposerOnEnter } from './composerKeyboard.ts';

test('desktop Enter sends while Shift+Enter remains a newline', () => {
  assert.equal(shouldSendComposerOnEnter({ key: 'Enter', shiftKey: false, isComposing: false }, false), true);
  assert.equal(shouldSendComposerOnEnter({ key: 'Enter', shiftKey: true, isComposing: false }, false), false);
});

test('coarse-pointer Enter remains a newline for mobile soft keyboards', () => {
  assert.equal(shouldSendComposerOnEnter({ key: 'Enter', shiftKey: false, isComposing: false }, true), false);
});

test('composition and non-Enter keys never submit', () => {
  assert.equal(shouldSendComposerOnEnter({ key: 'Enter', shiftKey: false, isComposing: true }, false), false);
  assert.equal(shouldSendComposerOnEnter({ key: 'a', shiftKey: false, isComposing: false }, false), false);
});
