import { test } from 'node:test';
import assert from 'node:assert';
import {
  findMatches,
  getMessageSearchableText,
  buildSearchMatches,
  highlightPlainText,
  highlightHtml,
} from '../src/utils/search';
import type { ChatMessage } from '../src/state/types';

function userMessage(id: string, text: string): ChatMessage {
  return { id, role: 'user', text, timestamp: 0 };
}

test('findMatches returns empty for empty query', () => {
  assert.deepEqual(findMatches('hello world', ''), []);
  assert.deepEqual(findMatches('hello world', '   '), []);
});

test('findMatches locates all occurrences case-insensitively', () => {
  const ranges = findMatches('Foo foo FOO', 'foo');
  assert.equal(ranges.length, 3);
  assert.deepEqual(ranges[0], { start: 0, length: 3 });
  assert.deepEqual(ranges[1], { start: 4, length: 3 });
  assert.deepEqual(ranges[2], { start: 8, length: 3 });
});

test('findMatches handles no-match', () => {
  assert.deepEqual(findMatches('hello', 'xyz'), []);
});

test('getMessageSearchableText returns raw text for user messages', () => {
  const msg = userMessage('u1', 'hello <script>');
  assert.equal(getMessageSearchableText(msg), 'hello <script>');
});

test('getMessageSearchableText covers only highlightable blocks (text/artifact, not tool/status)', () => {
  const msg: ChatMessage = {
    id: 'a1',
    role: 'assistant',
    text: 'intro',
    timestamp: 0,
    blocks: [
      { id: 'b1', type: 'text', content: 'intro' },
      {
        id: 'b2',
        type: 'tool',
        tool: { id: 't1', name: 'bash', args: 'lsargs', output: 'file.txt', status: 'done' },
      },
      { id: 'b3', type: 'text', content: 'done' },
    ],
  };
  const text = getMessageSearchableText(msg);
  assert.ok(text.includes('intro'));
  assert.ok(text.includes('done'));
  // tool args/output are NOT searched: they have no highlight path, so matching
  // them would produce phantom (counted-but-not-highlighted) hits — the very
  // mislabel this feature must avoid. Same for `status` blocks.
  assert.ok(!text.includes('file.txt'));
  assert.ok(!text.includes('lsargs'));
});

test('buildSearchMatches groups matches by message id', () => {
  const msgs = [
    userMessage('u1', 'find foo here'),
    userMessage('u2', 'no match'),
    userMessage('u3', 'foo again'),
  ];
  const matches = buildSearchMatches(msgs, 'foo');
  assert.equal(matches.length, 2);
  assert.equal(matches[0].messageId, 'u1');
  assert.equal(matches[0].ranges.length, 1);
  assert.equal(matches[1].messageId, 'u3');
  assert.equal(matches[1].ranges.length, 1);
});

test('highlightPlainText wraps matches in mark tags', () => {
  const html = highlightPlainText('find foo and FOO', 'foo');
  assert.ok(html.includes('<mark class="search-highlight">foo</mark>'));
  assert.ok(html.includes('<mark class="search-highlight">FOO</mark>'));
});

test('highlightPlainText escapes HTML entities', () => {
  const html = highlightPlainText('a <b> foo', 'foo');
  assert.ok(html.includes('&lt;b&gt;'));
  assert.ok(!html.includes('<b>'));
});

test('highlightPlainText returns escaped text when no query', () => {
  const html = highlightPlainText('a <b> & c', '');
  assert.ok(html.includes('&lt;b&gt;'));
  assert.ok(html.includes('&amp;'));
});

test('highlightHtml injects marks in text nodes only', () => {
  const html = highlightHtml('<p>find foo here</p>', 'foo');
  assert.ok(html.includes('<mark class="search-highlight">foo</mark>'));
});

test('highlightHtml does not highlight inside tags', () => {
  const html = highlightHtml('<a href="foo.html">link</a>', 'foo');
  assert.ok(!html.includes('<mark class="search-highlight">foo</mark>'));
});

test('highlightHtml does not highlight inside code blocks', () => {
  const html = highlightHtml('<pre><code>foo bar</code></pre>', 'foo');
  assert.ok(!html.includes('<mark'));
});

test('highlightHtml returns unchanged when no query', () => {
  const input = '<p>hello</p>';
  assert.equal(highlightHtml(input, ''), input);
});

test('highlightHtml matches entity-encoded query in HTML text', () => {
  // The raw text is "a & b" but marked renders it as "a &amp; b".
  // Searching for "a & b" should still match.
  const html = highlightHtml('<p>a &amp; b</p>', 'a & b');
  assert.ok(html.includes('<mark class="search-highlight">a &amp; b</mark>'));
});

test('highlightHtml matches entity-encoded query with angle brackets', () => {
  const html = highlightHtml('<p>use &lt;div&gt; here</p>', '<div>');
  assert.ok(html.includes('<mark class="search-highlight">&lt;div&gt;</mark>'));
});
