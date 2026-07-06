import assert from 'node:assert/strict';
import { test } from 'node:test';
import { markdownToHtml, prepareMarkdownForRender, repairStreamingMarkdown } from '../dist/components/markdownRendering.js';

test('assistant markdown escapes raw html instead of dropping literal tags', () => {
  const html = markdownToHtml('请输出一段 </script> 和 <script>alert(1)</script> 文本');

  assert.match(html, /&lt;\/script&gt;/);
  assert.match(html, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
  assert.doesNotMatch(html, /<script>/);
});

test('gfm table followed by prose keeps prose outside the table', () => {
  const prepared = prepareMarkdownForRender([
    '| 示例 | 说明 |',
    '| - | - |',
    '| Pandoc | 转换 |',
    '后续正文',
  ].join('\n'));
  const html = markdownToHtml(prepared);

  assert.match(html, /<\/table>\s*<p>后续正文<\/p>/);
  assert.doesNotMatch(html, /<td>后续正文<\/td>/);
});

test('single dash table delimiter is repaired like marked parses it', () => {
  const prepared = prepareMarkdownForRender([
    '| A | B |',
    '|-|-|',
    '| 1 | 2 |',
    'plain text',
  ].join('\n'));
  const html = markdownToHtml(prepared);

  assert.match(html, /<\/table>\s*<p>plain text<\/p>/);
  assert.doesNotMatch(html, /<td>plain text<\/td>/);
});

test('table followed by fenced code does not swallow the fence', () => {
  const prepared = prepareMarkdownForRender([
    '| A | B |',
    '| - | - |',
    '| 1 | 2 |',
    '```ts',
    'const value = 1;',
    '```',
  ].join('\n'));
  const html = markdownToHtml(prepared);

  assert.match(html, /<\/table>\s*<pre><code class="language-ts">const value = 1;/);
});

test('table repair ignores fenced code samples', () => {
  const markdown = [
    '```md',
    '| A | B |',
    '| - | - |',
    '| 1 | 2 |',
    'plain text',
    '```',
  ].join('\n');

  assert.equal(prepareMarkdownForRender(markdown), markdown);
});

test('table repair ignores html blocks', () => {
  const markdown = [
    '<div>',
    '| A | B |',
    '| - | - |',
    '| 1 | 2 |',
    'plain text',
    '</div>',
    '',
    'after',
  ].join('\n');

  assert.equal(prepareMarkdownForRender(markdown), markdown);
});

test('streaming markdown repairs unclosed code fence after preprocessing', () => {
  const repaired = repairStreamingMarkdown([
    '说明：',
    '```rust',
    'fn main() {',
  ].join('\n'));

  assert.equal(repaired, [
    '说明：',
    '```rust',
    'fn main() {',
    '```',
    '',
  ].join('\n'));
});
