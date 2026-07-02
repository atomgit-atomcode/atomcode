import React, { useRef, useEffect, useCallback, useMemo } from 'react';
import { marked } from 'marked';
import DOMPurify from 'dompurify';
import { postMessage } from '../vscode';
import { renderCodeBlockHtml } from './codeBlockRendering';
import { prepareMarkdownForRender } from './streamingMarkdown';
import { useT } from '../i18n';

marked.setOptions({
  gfm: true,
  breaks: false,
});

interface MarkdownProps {
  content: string;
  streaming?: boolean;
}

export function Markdown({ content, streaming = false }: MarkdownProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const t = useT();

  const handleActions = useCallback((e: MouseEvent) => {
    const target = e.target as HTMLElement;
    const btn = target.closest('.copy-button') as HTMLElement | null;
    if (!btn) return;
    const wrapper = btn.closest('.code-block-wrapper') as HTMLElement | null;
    if (!wrapper) return;
    const codeEl = wrapper.querySelector('pre code');
    if (!codeEl) return;
    const code = wrapper.dataset.rawCode ?? codeEl.textContent ?? '';
    const action = btn.dataset.action;

    if (action === 'copy') {
      navigator.clipboard.writeText(code).then(() => {
        btn.title = t('tool.copied');
        setTimeout(() => { btn.title = t('assistant.copy'); }, 2000);
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
    const renderer = new marked.Renderer();
    renderer.code = function (code: string, infostring?: string) {
      return renderCodeBlockHtml(code, infostring, { copy: t('assistant.copy') });
    };
    const source = prepareMarkdownForRender(content, streaming);
    const raw = marked.parse(source, { renderer }) as string;
    return DOMPurify.sanitize(raw);
  }, [content, streaming, t]);

  return (
    <div ref={containerRef} className="markdown-root" dangerouslySetInnerHTML={{ __html: html }} />
  );
}
