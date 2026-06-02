import { marked } from 'marked';
import DOMPurify from 'dompurify';
import { useMemo } from 'preact/hooks';

marked.setOptions({ gfm: true, breaks: false });

// Custom code-block renderer → matches the ported .code-block-wrapper CSS.
const renderer = new marked.Renderer();
renderer.code = function (code: string, infostring?: string) {
  const text = code ?? '';
  if (!text.trim()) return '';
  const lang = (infostring ?? '').split(/\s+/)[0] ?? '';
  // escape HTML in code (no hljs for v1)
  const esc = text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  return (
    `<div class="code-block-wrapper">` +
    `<pre><code class="${lang ? `language-${lang}` : ''}">${esc}</code></pre>` +
    `<button class="copy-button" type="button" data-copy="${encodeURIComponent(text)}">Copy</button>` +
    `</div>`
  );
};

export function Markdown({ content }: { content: string }) {
  const html = useMemo(() => {
    const raw = marked.parse(content ?? '', { renderer }) as string;
    // SECURITY: model output is untrusted — sanitize before injecting as HTML.
    return DOMPurify.sanitize(raw, { ADD_ATTR: ['data-copy'] });
  }, [content]);

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
