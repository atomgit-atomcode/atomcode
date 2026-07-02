import { test } from 'node:test';
import assert from 'node:assert';
import {
  detectAtMentionRange,
  ensureActiveDescendantVisible,
  replaceAtMention,
  splitAtToken,
} from '../src/utils/atMention';

test('detects @ mentions only at start or after whitespace', () => {
  assert.deepEqual(detectAtMentionRange('@src', 4), {
    start: 0,
    end: 4,
    token: 'src',
  });
  assert.deepEqual(detectAtMentionRange('open @src', 9), {
    start: 5,
    end: 9,
    token: 'src',
  });
  assert.equal(detectAtMentionRange('name@example.com', 'name@example.com'.length), null);
});

test('splits @ mention tokens into scope directory and filter', () => {
  assert.deepEqual(splitAtToken('extensions/vs'), {
    scopeDir: 'extensions/',
    filter: 'vs',
  });
});

test('replaces the selected @ mention with an @relative/path token', () => {
  const input = 'read @ext';
  const range = detectAtMentionRange(input, input.length);
  assert.ok(range);
  assert.deepEqual(replaceAtMention(input, range, 'extensions/vscode/'), {
    text: 'read @extensions/vscode/ ',
    cursor: 'read @extensions/vscode/ '.length,
  });
});

test('scrolls the active @ menu row into the visible list', () => {
  const container = { scrollTop: 36, clientHeight: 96 };

  ensureActiveDescendantVisible(container, { offsetTop: 150, offsetHeight: 28 });
  assert.equal(container.scrollTop, 82);

  ensureActiveDescendantVisible(container, { offsetTop: 24, offsetHeight: 28 });
  assert.equal(container.scrollTop, 24);
});
