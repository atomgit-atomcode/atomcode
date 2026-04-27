import React, { useRef, useEffect, useCallback } from 'react';
import { renderMarkdown } from '../utils/markdown';
import { postMessage } from '../vscode';

interface MarkdownProps {
  content: string;
}

export function Markdown({ content }: MarkdownProps) {
  const containerRef = useRef<HTMLDivElement>(null);

  const handleCodeActions = useCallback((e: MouseEvent) => {
    const target = e.target as HTMLElement;
    if (!target.classList.contains('code-action-btn')) return;

    const action = target.dataset.action;
    const wrapper = target.closest('.code-block-wrapper') as HTMLElement | null;
    if (!wrapper) return;

    const codeEl = wrapper.querySelector('pre code');
    if (!codeEl) return;
    const code = codeEl.textContent ?? '';

    if (action === 'copy') {
      navigator.clipboard.writeText(code).then(() => {
        target.textContent = 'Copied!';
        setTimeout(() => {
          target.textContent = 'Copy';
        }, 2000);
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
    el.addEventListener('click', handleCodeActions as EventListener);
    return () => el.removeEventListener('click', handleCodeActions as EventListener);
  }, [handleCodeActions]);

  const html = renderMarkdown(content);

  return (
    <div
      ref={containerRef}
      className="markdown-body"
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
