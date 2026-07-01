import { test } from 'node:test';
import assert from 'node:assert';
import {
  detectAtMentionRange,
  ensureActiveDescendantVisible,
  replaceAtMention,
  splitAtToken,
} from './atMention.ts';

test('detects an @ mention at the beginning of the input', () => {
  assert.deepEqual(detectAtMentionRange('@cra', 4), {
    start: 0,
    end: 4,
    token: 'cra',
  });
});

test('detects the active @ mention after whitespace and ignores email addresses', () => {
  assert.deepEqual(detectAtMentionRange('look at @web', 12), {
    start: 8,
    end: 12,
    token: 'web',
  });
  assert.equal(detectAtMentionRange('email@host.com', 'email@host.com'.length), null);
});

test('stops detecting an @ mention after whitespace terminates the token', () => {
  assert.equal(detectAtMentionRange('@webui ', 7), null);
});

test('splits scoped @ mention tokens like the TUI file index', () => {
  assert.deepEqual(splitAtToken('crates/atom'), {
    scopeDir: 'crates/',
    filter: 'atom',
  });
});

test('replaces the active @ token with the selected relative path and trailing space', () => {
  const text = 'check @web';
  const range = detectAtMentionRange(text, text.length);
  assert.ok(range);
  assert.deepEqual(replaceAtMention(text, range, 'webui/src/app.tsx'), {
    text: 'check @webui/src/app.tsx ',
    cursor: 'check @webui/src/app.tsx '.length,
  });
});

test('scrolls the active @ menu row into the visible container', () => {
  const container = { scrollTop: 0, clientHeight: 100 };

  ensureActiveDescendantVisible(container, { offsetTop: 120, offsetHeight: 24 });
  assert.equal(container.scrollTop, 44);

  ensureActiveDescendantVisible(container, { offsetTop: 12, offsetHeight: 24 });
  assert.equal(container.scrollTop, 12);
});
