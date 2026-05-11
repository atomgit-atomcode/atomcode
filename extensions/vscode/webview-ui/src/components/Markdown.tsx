import React, { useRef, useEffect, useCallback, useMemo } from 'react';
import { marked } from 'marked';
import hljs from 'highlight.js';
import DOMPurify from 'dompurify';
import { postMessage } from '../vscode';

marked.setOptions({
  gfm: true,
  breaks: false,
});

const renderer = new marked.Renderer();

renderer.code = function (code: string, infostring?: string) {
  const text = code ?? '';
  if (!text.trim()) {
    return '';
  }
  const lang = (infostring ?? '').split(/\s+/)[0] ?? '';
  const language = lang && hljs.getLanguage(lang) ? lang : '';
  const highlighted = language
    ? hljs.highlight(text, { language }).value
    : hljs.highlightAuto(text).value;
  const id = `cb-${Math.random().toString(36).slice(2, 8)}`;
  return (
    `<div class="code-block-wrapper" data-code-id="${id}">` +
    `<pre><code class="hljs${language ? ` language-${language}` : ''}">${highlighted}</code></pre>` +
    `<button class="copy-button" data-action="copy" title="Copy">` +
    `<svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">` +
    `<path d="M4 4v8h8V4H4zm7 7H5V5h6v6zM2 2v8h1V3h7V2H2z"/>` +
    `</svg></button>` +
    `</div>`
  );
};

interface MarkdownProps {
  content: string;
}

export function Markdown({ content }: MarkdownProps) {
  const containerRef = useRef<HTMLDivElement>(null);

  const handleActions = useCallback((e: MouseEvent) => {
    const target = e.target as HTMLElement;
    const btn = target.closest('.copy-button') as HTMLElement | null;
    if (!btn) return;
    const wrapper = btn.closest('.code-block-wrapper') as HTMLElement | null;
    if (!wrapper) return;
    const codeEl = wrapper.querySelector('pre code');
    if (!codeEl) return;
    const code = codeEl.textContent ?? '';
    const action = btn.dataset.action;

    if (action === 'copy') {
      navigator.clipboard.writeText(code).then(() => {
        btn.title = 'Copied!';
        setTimeout(() => { btn.title = 'Copy'; }, 2000);
      });
    } else if (action === 'apply') {
      postMessage({ type: 'applyCode', code });
    } else if (action === 'insert') {
      postMessage({ type: 'insertCode', code });
    }
  }, []);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    el.addEventListener('click', handleActions);
    return () => el.removeEventListener('click', handleActions);
  }, [handleActions]);

  const html = useMemo(() => {
    const raw = marked.parse(content, { renderer }) as string;
    return DOMPurify.sanitize(raw);
  }, [content]);

  return (
    <div ref={containerRef} className="markdown-root" dangerouslySetInnerHTML={{ __html: html }} />
  );
}
