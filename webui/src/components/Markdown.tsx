import { marked } from 'marked';
import DOMPurify from 'dompurify';
import { useMemo } from 'preact/hooks';
import { createMathExtensions } from '../lib/markdownMath';

marked.setOptions({ gfm: true, breaks: false });

// 关闭 GFM 单/双波浪号删除线：模型输出里 `~` 多是字面量（步骤区间 `1~3`、路径
// `~/projects`），而非删除线。开着的话 `步骤 1~3 …继续执行 4~7` 会被当成
// `1<del>3 …4</del>7`，吃掉波浪号且整段加删除线，与 TUI 显示不一致（issue #825）。
// 覆盖 del tokenizer 返回 undefined → 波浪号回退为普通文本。
marked.use({ tokenizer: { del: () => undefined } });

// 数学渲染：解析期分词（$...$、$$...$$、\(...\)、\[...\]），详见 lib/markdownMath.ts。
// 扩展先于内置 tokenizer 执行；它只消费数学起点，并跳过 HTML 原文区，
// 让代码 span/围栏由内置 tokenizer 消费（见 markdownMath.ts）。
marked.use({ extensions: createMathExtensions() });

// Custom code-block renderer → matches the ported .code-block-wrapper CSS.
const renderer = new marked.Renderer();
renderer.code = function (code: string, infostring?: string) {
  const text = code ?? '';
  if (!text.trim()) return '';
  const lang = (infostring ?? '').split(/\s+/)[0] ?? '';
  // Escape HTML in code; syntax highlighting is intentionally omitted here.
  const esc = text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  return (
    `<div class="code-block-wrapper">` +
    `<pre><code class="${lang ? `language-${lang}` : ''}">${esc}</code></pre>` +
    `<button class="copy-button" type="button" data-copy="${encodeURIComponent(text)}">Copy</button>` +
    `</div>`
  );
};

function highlightHtml(html: string, search: string): string {
  if (!search.trim()) return html;
  const parser = new DOMParser();
  const doc = parser.parseFromString(html, 'text/html');
  const walk = doc.createTreeWalker(doc.body, NodeFilter.SHOW_TEXT, null);
  const nodes: Text[] = [];
  let node: Node | null;
  while ((node = walk.nextNode())) {
    nodes.push(node as Text);
  }
  const searchLower = search.toLowerCase();
  for (const textNode of nodes) {
    const text = textNode.nodeValue ?? '';
    if (!text.toLowerCase().includes(searchLower)) continue;
    
    const parent = textNode.parentNode;
    if (!parent) continue;

    // 数学子树不参与搜索高亮：往 KaTeX 的 MathML/排版 span 里插 <mark>
    // 会破坏渲染结构，且 annotation 里的 TeX 原文高亮无意义。
    if ((parent as HTMLElement).closest?.('.katex')) continue;

    const parentTag = (parent as HTMLElement).tagName?.toLowerCase();
    if (parentTag === 'script' || parentTag === 'style') continue;

    const newFragment = doc.createDocumentFragment();
    let lastIndex = 0;
    let index = text.toLowerCase().indexOf(searchLower);
    while (index !== -1) {
      if (index > lastIndex) {
        newFragment.appendChild(doc.createTextNode(text.substring(lastIndex, index)));
      }
      const mark = doc.createElement('mark');
      mark.className = 'msg-search-highlight';
      mark.textContent = text.substring(index, index + search.length);
      newFragment.appendChild(mark);
      lastIndex = index + search.length;
      index = text.toLowerCase().indexOf(searchLower, lastIndex);
    }
    if (lastIndex < text.length) {
      newFragment.appendChild(doc.createTextNode(text.substring(lastIndex)));
    }
    parent.replaceChild(newFragment, textNode);
  }
  return doc.body.innerHTML;
}

export function Markdown({ content, search }: { content: string; search?: string }) {
  const html = useMemo(() => {
    const raw = marked.parse(content ?? '', { renderer }) as string;
    // SECURITY: model output is untrusted — sanitize before injecting as HTML.
    // KaTeX 输出携带 MathML <semantics><annotation encoding="..."> 原 TeX
    // （供无障碍/复制），DOMPurify 默认不放行这三个面，需显式 ADD。
    const sanitized = DOMPurify.sanitize(raw, {
      ADD_ATTR: ['data-copy', 'encoding'],
      ADD_TAGS: ['semantics', 'annotation', 'maction'],
    });
    if (search && search.trim()) {
      return highlightHtml(sanitized, search);
    }
    return sanitized;
  }, [content, search]);

  function onClick(e: MouseEvent) {
    const t = (e.target as HTMLElement)?.closest('.copy-button') as HTMLElement | null;
    if (t?.dataset.copy) {
      navigator.clipboard?.writeText(decodeURIComponent(t.dataset.copy)).catch(() => {});
      const prev = t.textContent;
      t.textContent = 'Copied';
      setTimeout(() => {
        t.textContent = prev;
      }, 1200);
    }
  }

  return (
    <div
      class="markdown-root assistant-message-content"
      onClick={onClick}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
