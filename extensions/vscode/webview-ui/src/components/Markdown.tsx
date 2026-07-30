import React, { useRef, useEffect, useCallback, useMemo } from 'react';
import { marked } from 'marked';
import DOMPurify from 'dompurify';
import { postMessage } from '../vscode';
import { escapeHtml, renderCodeBlockHtml } from './codeBlockRendering';
import { prepareMarkdownForRender } from './streamingMarkdown';
import { highlightHtml } from '../utils/search';
import { useT } from '../i18n';
import {
  openFileMessageFromVscodeUri,
  parseVscodeFileUri,
  renderVscodeFileAnchor,
  vscodeFileLinkExtension,
} from './vscodeFileLinks';

marked.setOptions({
  gfm: true,
  breaks: false,
});
marked.use({ extensions: [vscodeFileLinkExtension] });

interface MarkdownProps {
  content: string;
  streaming?: boolean;
  searchQuery?: string;
}

export function markdownToHtml(
  content: string,
  streaming = false,
  labels: { copy?: string } = {},
): string {
  const renderer = new marked.Renderer();
  renderer.code = function (code: string, infostring?: string) {
    return renderCodeBlockHtml(code, infostring, labels);
  };
  renderer.html = function (html: string) {
    return escapeHtml(html);
  };
  const renderDefaultLink = renderer.link.bind(renderer);
  renderer.link = function (href: string, title: string | null | undefined, text: string) {
    return parseVscodeFileUri(href)
      ? renderVscodeFileAnchor(href, text)
      : renderDefaultLink(href, title, text);
  };
  const source = prepareMarkdownForRender(content, streaming);
  return marked.parse(source, { renderer }) as string;
}

export function Markdown({ content, streaming = false, searchQuery }: MarkdownProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const t = useT();

  const handleActions = useCallback((e: MouseEvent) => {
    const target = e.target as HTMLElement;
    const fileLink = target.closest('a[data-vscode-file-uri]') as HTMLAnchorElement | null;
    if (fileLink) {
      e.preventDefault();
      const message = openFileMessageFromVscodeUri(fileLink.dataset.vscodeFileUri ?? '');
      if (message) postMessage(message);
      return;
    }
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
    const raw = markdownToHtml(content, streaming, { copy: t('assistant.copy') });
    const highlighted = searchQuery && searchQuery.trim()
      ? highlightHtml(raw, searchQuery)
      : raw;
    return DOMPurify.sanitize(highlighted);
  }, [content, streaming, t, searchQuery]);

  return (
    <div ref={containerRef} className="markdown-root" dangerouslySetInnerHTML={{ __html: html }} />
  );
}
