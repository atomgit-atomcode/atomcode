// 数学定界符识别与 KaTeX 渲染（webui 数学渲染的纯逻辑部分）。
//
// 设计要点（对照主流 AI 工具的实践，见 issue 调研）：
// - 分词必须在 Markdown 解析期完成（marked 扩展），而不是渲染后扫 DOM：
//   后者会破坏代码 span 边界，且 marked 文本层已剥蚀反斜杠转义。
// - 定界符分两档：$$、\(...\)、\[...\] 零歧义全开；单 $ 按 pandoc 规则
//   （开定界符后不能是空白；闭定界符前不能是空白、后不能是数字），
//   避免 "价格 $5 和 $10" 这类货币文本被误吞（同 issue #825 的教训）。
// - 反引号 guard：合法行内公式不含反引号。匹配体含反引号即放弃，
//   整段回退给 codespan/text 处理，避免 `` `${FOO}` … `${BAR}` `` 两个
//   代码 span 之间的文本被当成公式（claude.ai 的已知 bug）。
import katex from 'katex';
import type { RendererExtension, Tokens, TokenizerExtension } from 'marked';

/** 一次数学定界符匹配的结果（不含渲染）。 */
export interface MathMatch {
  /** 含定界符的完整原文，marked 用它推进游标。 */
  raw: string;
  /** 去掉定界符后的 TeX 源码。 */
  tex: string;
  /** true 表示 display 模式（独立公式块）。 */
  display: boolean;
}

/** 行内数学：$...$、$$...$$、\(...\)、\[...\]，必须匹配在 src 起点处。 */
export function matchInlineMath(src: string): MathMatch | null {
  if (!src) return null;

  if (src.startsWith('\\(')) {
    const end = src.indexOf('\\)', 2);
    if (end < 0) return null;
    const tex = src.slice(2, end);
    if (!tex.trim() || tex.includes('`') || tex.includes('\n')) return null;
    return { raw: src.slice(0, end + 2), tex: tex.trim(), display: false };
  }

  if (src.startsWith('$$')) {
    const end = src.indexOf('$$', 2);
    if (end < 0) return null;
    const tex = src.slice(2, end);
    if (!tex.trim() || tex.includes('`')) return null;
    return { raw: src.slice(0, end + 2), tex: tex.trim(), display: true };
  }

  if (src.startsWith('\\[')) {
    const end = src.indexOf('\\]', 2);
    if (end < 0) return null;
    const tex = src.slice(2, end);
    if (!tex.trim() || tex.includes('`')) return null;
    return { raw: src.slice(0, end + 2), tex: tex.trim(), display: true };
  }

  if (!src.startsWith('$')) return null;

  // pandoc：开定界符后不能是空白或另一个 $。
  if (!src[1] || /\s|\$/.test(src[1])) return null;

  for (let i = 1; i < src.length; i++) {
    const c = src[i];
    // 单 $ 公式不跨行；遇反引号立即放弃（代码保护）。
    if (c === '\n' || c === '`') return null;
    if (c !== '$') continue;
    if (src[i - 1] === '\\') continue; // \$ 转义不是闭定界符
    if (/\s/.test(src[i - 1])) continue; // 闭定界符前不能是空白
    const after = src[i + 1];
    if (after !== undefined && /[\d$]/.test(after)) continue; // 后不能是数字或 $
    const tex = src.slice(1, i);
    if (!tex.trim()) return null;
    return { raw: src.slice(0, i + 1), tex, display: false };
  }
  return null;
}

// 块级数学独占若干行：行首（至多 3 空格缩进）的 $$...$$ 或 \[...\]。
// 4 空格缩进留给缩进代码块；围栏代码块由 fences 先行消费，天然不受影响。
const BLOCK_DOLLAR_RE = /^ {0,3}\$\$([\s\S]+?)\$\$[ \t]*(?:\n|$)/;
const BLOCK_BRACKET_RE = /^ {0,3}\\\[([\s\S]+?)\\\][ \t]*(?:\n|$)/;

/** 块级数学：行首 $$...$$ 或 \[...\]，必须匹配在 src 起点处。 */
export function matchBlockMath(src: string): MathMatch | null {
  const m = BLOCK_DOLLAR_RE.exec(src) ?? BLOCK_BRACKET_RE.exec(src);
  if (!m) return null;
  const tex = m[1];
  // 含 ``` 视为围栏串扰，放弃（回退给后续块 tokenizer）。
  if (!tex.trim() || tex.includes('```')) return null;
  return { raw: m[0], tex: tex.trim(), display: true };
}

/** KaTeX 渲染；失败时降级为转义后的原文，绝不吞内容。 */
function renderKatex(tex: string, display: boolean): string {
  try {
    return katex.renderToString(tex, {
      displayMode: display,
      throwOnError: false,
      strict: 'ignore',
      trust: false,
    });
  } catch {
    const esc = tex.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    return `<code class="math-fallback">${esc}</code>`;
  }
}

/** 找 src 中首个未被反斜杠转义的 $ 的下标（给 marked 的 start 提示用）。 */
function firstUnescapedDollar(src: string): number {
  let i = -1;
  while ((i = src.indexOf('$', i + 1)) >= 0) {
    let backslashes = 0;
    for (let j = i - 1; j >= 0 && src[j] === '\\'; j--) backslashes++;
    if (backslashes % 2 === 1) continue; // \$ 转义
    return i;
  }
  return -1;
}

function minIndex(...candidates: number[]): number | undefined {
  const valid = candidates.filter((i) => i >= 0);
  return valid.length ? Math.min(...valid) : undefined;
}

interface MathToken extends Tokens.Generic {
  math: string;
  display: boolean;
}

/** 供 marked.use({ extensions }) 注册的数学扩展（块级 + 行内）。 */
export function createMathExtensions(): (TokenizerExtension & RendererExtension)[] {
  const blockMath: TokenizerExtension & RendererExtension = {
    name: 'mathBlock',
    level: 'block',
    start(src: string) {
      const m = /^ {0,3}(?:\$\$|\\\[)/m.exec(src);
      return m ? m.index : undefined;
    },
    tokenizer(src: string) {
      const hit = matchBlockMath(src);
      if (!hit) return undefined;
      const token: MathToken = {
        type: 'mathBlock',
        raw: hit.raw,
        math: hit.tex,
        display: hit.display,
        tokens: [],
      };
      return token;
    },
    renderer(token: Tokens.Generic) {
      const t = token as MathToken;
      return renderKatex(t.math, t.display);
    },
  };

  const inlineMath: TokenizerExtension & RendererExtension = {
    name: 'mathInline',
    level: 'inline',
    start(src: string) {
      return minIndex(firstUnescapedDollar(src), src.indexOf('\\('), src.indexOf('\\['));
    },
    tokenizer(src: string) {
      const hit = matchInlineMath(src);
      if (!hit) return undefined;
      const token: MathToken = {
        type: 'mathInline',
        raw: hit.raw,
        math: hit.tex,
        display: hit.display,
        tokens: [],
      };
      return token;
    },
    renderer(token: Tokens.Generic) {
      const t = token as MathToken;
      return renderKatex(t.math, t.display);
    },
  };

  return [blockMath, inlineMath];
}
