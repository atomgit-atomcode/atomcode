import { test } from 'node:test';
import assert from 'node:assert';
import { marked } from 'marked';
import {
  createMathExtensions,
  matchBlockMath,
  matchInlineMath,
} from './markdownMath.ts';

// ---------- matchInlineMath：行内定界符识别 ----------

test('matches basic inline $...$ math', () => {
  assert.deepEqual(matchInlineMath('$x^2$ rest'), {
    raw: '$x^2$',
    tex: 'x^2',
    display: false,
  });
});

test('inline math adjacent to CJK text is recognized', () => {
  // matchInlineMath 只匹配字符串起点（marked 会把游标推进到定界符处再调用）。
  assert.equal(matchInlineMath('$x$的值')?.tex, 'x');
  assert.equal(matchInlineMath('$\\alpha$成立')?.tex, '\\alpha');
});

test('matches inline display $$...$$', () => {
  const hit = matchInlineMath('$$E=mc^2$$ tail');
  assert.equal(hit?.display, true);
  assert.equal(hit?.tex, 'E=mc^2');
});

test('matches \\(...\\) inline math', () => {
  const hit = matchInlineMath('\\(x^2\\) more');
  assert.equal(hit?.display, false);
  assert.equal(hit?.tex, 'x^2');
});

test('unclosed delimiters fall back to plain text', () => {
  assert.equal(matchInlineMath('$unclosed'), null);
  assert.equal(matchInlineMath('$$unclosed'), null);
  assert.equal(matchInlineMath('\\(unclosed'), null);
  assert.equal(matchInlineMath('\\[unclosed'), null);
});

// ---------- pandoc 空白/数字规则（货币与散文保护） ----------

test('currency amounts are not swallowed', () => {
  assert.equal(matchInlineMath('$5 和 $10'), null);
  assert.equal(matchInlineMath('$5 and $10'), null);
  assert.equal(matchInlineMath('价格 $100，节省 $25 总计'), null);
});

test('closing $ followed by a digit does not delimit', () => {
  // pandoc 规则：闭定界符后是数字则不算，防止 "$5-$10" 被切成 "5-" 公式。
  assert.equal(matchInlineMath('$5-$10'), null);
});

test('opening $ followed by whitespace or $ is not a delimiter', () => {
  assert.equal(matchInlineMath('$ x$'), null);
  assert.equal(matchInlineMath('$$'), null);
});

test('closing $ preceded by whitespace is not a delimiter', () => {
  assert.equal(matchInlineMath('$x $'), null);
});

// ---------- 反引号 guard（代码 span 保护） ----------

test('inline math containing a backtick is rejected', () => {
  // claude.ai 已知 bug 的回归用例：两个代码 span 之间的 `$...$` 误渲染。
  // tokenizer 从 "${FOO}` 起扫描时必须放弃。
  assert.equal(matchInlineMath('${FOO}` x `${BAR}'), null);
});

test('single-$ math never crosses a newline', () => {
  assert.equal(matchInlineMath('$a\nb$'), null);
});

// ---------- 转义 ----------

test('escaped \\$ is not a delimiter', () => {
  assert.equal(matchInlineMath('\\$5 and \\$6'), null);
  // 公式内部的 \$ 是合法内容，不提前闭合。
  assert.equal(matchInlineMath('$a\\$b$')?.tex, 'a\\$b');
});

test('closing dollar uses backslash parity, including a final TeX row break', () => {
  assert.equal(matchInlineMath(String.raw`$x\\$`)?.tex, String.raw`x\\`);
  assert.equal(matchInlineMath(String.raw`$x\\\$`), null);
  const html = render(String.raw`$x\\$`);
  assert.ok(html.includes('class="katex"'), html);
  assert.ok(!html.includes('katex-error'), html);
});

// ---------- matchBlockMath：块级 ----------

test('matches a multi-line $$...$$ block', () => {
  const hit = matchBlockMath('$$\nx = y\n$$\nnext');
  assert.equal(hit?.display, true);
  assert.equal(hit?.tex, 'x = y');
  assert.equal(hit?.raw, '$$\nx = y\n$$\n');
});

test('matches a matrix block containing \\\\[0.3em] row breaks', () => {
  const src = '$$\n\\begin{matrix} a \\\\[0.3em] b \\end{matrix}\n$$\n';
  const hit = matchBlockMath(src);
  assert.equal(hit?.display, true);
  assert.ok(hit!.tex.includes('\\\\[0.3em]'));
});

test('matches \\[...\\] block math at line start', () => {
  const hit = matchBlockMath('\\[ E = mc^2 \\]\nrest');
  assert.equal(hit?.tex, 'E = mc^2');
});

test('block math without trailing newline at EOF still matches', () => {
  assert.equal(matchBlockMath('$$x$$')?.tex, 'x');
});

test('4-space indented $$ stays an indented code block', () => {
  assert.equal(matchBlockMath('    $$x$$\n'), null);
});

test('fenced code blocks are never math', () => {
  assert.equal(matchBlockMath('```math\n$$x$$\n```\n'), null);
  assert.equal(matchBlockMath('```\n$$x$$\n```\n'), null);
});

// ---------- 集成：经 marked 全链路（解析期分词） ----------

function render(md: string): string {
  marked.setOptions({ gfm: true });
  marked.use({ extensions: createMathExtensions() });
  return marked.parse(md) as string;
}

test('inline and display math render through marked', () => {
  const html = render('质能方程 $E=mc^2$ 成立。\n\n$$\n\\frac{a}{b}\n$$\n');
  assert.ok(html.includes('katex'), `expected katex output, got: ${html}`);
  assert.ok(html.includes('katex-display'));
});

test('code fences and code spans are immune (claude.ai bug regression)', () => {
  const fence = render('```\n$foo$-$bar$\n```\n');
  assert.ok(!fence.includes('katex'), fence);
  assert.ok(fence.includes('$foo$-$bar$'));

  const spans = render('运行 `${FOO}` 与 `${BAR}` 两个变量');
  assert.ok(!spans.includes('katex'), spans);
  assert.ok(spans.includes('<code>'));
});

test('raw HTML code and keyboard text stay literal, with math resuming afterwards', () => {
  for (const tag of ['code', 'kbd']) {
    const raw = `<${tag}>$x$ and $$y$$</${tag}>`;
    const html = render(raw + ' 后面的公式 $z$');
    assert.ok(html.includes(raw), html);
    assert.equal((html.match(/class="katex"/g) ?? []).length, 1, html);
  }
});

test('invalid TeX falls back to the complete escaped source for all delimiters', () => {
  for (const raw of [
    String.raw`$\frac{1}{$`,
    String.raw`$$ \frac{1}{ $$`,
    String.raw`\( \frac{1}{ \)`,
    String.raw`\[ \frac{1}{ \]`,
  ]) {
    const html = render(raw);
    assert.ok(html.includes(`<code class="math-fallback">${raw}</code>`), html);
    assert.ok(!html.includes('katex-error'), html);
  }
  const raw = String.raw`$\frac{<img src=x onerror=alert(1)> &}{$`;
  const html = render(raw);
  assert.ok(html.includes('math-fallback'), html);
  assert.ok(html.includes('&lt;img src=x onerror=alert(1)&gt; &amp;'), html);
  assert.ok(!html.includes('<img'), html);
});

test('currency prose renders without math', () => {
  const html = render('价格 $5 和 $10，合计 $15。');
  assert.ok(!html.includes('katex'), html);
});

test('\\(...\\) and \\[...\\] render through marked', () => {
  const html = render('设 \\(x^2\\) 为二次项。\n\n\\[ a^2 + b^2 = c^2 \\]\n');
  assert.ok(html.includes('katex'), html);
});
