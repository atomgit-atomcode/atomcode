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
const ANSI_PATTERN = /\x1b\[[0-9;]*m/g;
const ANSI_FG_CLASSES: Record<number, string> = {
  30: 'ansi-fg-black',
  31: 'ansi-fg-red',
  32: 'ansi-fg-green',
  33: 'ansi-fg-yellow',
  34: 'ansi-fg-blue',
  35: 'ansi-fg-magenta',
  36: 'ansi-fg-cyan',
  37: 'ansi-fg-white',
  90: 'ansi-fg-bright-black',
  91: 'ansi-fg-bright-red',
  92: 'ansi-fg-bright-green',
  93: 'ansi-fg-bright-yellow',
  94: 'ansi-fg-bright-blue',
  95: 'ansi-fg-bright-magenta',
  96: 'ansi-fg-bright-cyan',
  97: 'ansi-fg-bright-white',
};

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function ansiCodeToHtml(code: string): string {
  let output = '';
  let lastIndex = 0;
  let currentClass: string | null = null;
  ANSI_PATTERN.lastIndex = 0;

  const closeSpan = () => {
    if (currentClass) {
      output += '</span>';
      currentClass = null;
    }
  };

  for (const match of code.matchAll(ANSI_PATTERN)) {
    output += escapeHtml(code.slice(lastIndex, match.index));
    lastIndex = (match.index ?? 0) + match[0].length;

    const rawCodes = match[0].slice(2, -1);
    const codes = rawCodes.length > 0 ? rawCodes.split(';').map((value) => Number(value) || 0) : [0];
    for (const sgr of codes) {
      if (sgr === 0 || sgr === 39) {
        closeSpan();
      } else if (ANSI_FG_CLASSES[sgr]) {
        closeSpan();
        currentClass = ANSI_FG_CLASSES[sgr];
        output += `<span class="${currentClass}">`;
      }
    }
  }

  output += escapeHtml(code.slice(lastIndex));
  closeSpan();
  return output;
}

renderer.code = function (code: string, infostring?: string) {
  const text = code ?? '';
  if (!text.trim()) {
    return '';
  }
  const lang = (infostring ?? '').split(/\s+/)[0] ?? '';
  const language = lang && hljs.getLanguage(lang) ? lang : '';
  const hasAnsi = ANSI_PATTERN.test(text);
  ANSI_PATTERN.lastIndex = 0;
  const highlighted = hasAnsi
    ? ansiCodeToHtml(text)
    : language
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
