import React, { useMemo } from 'react';
import { marked } from 'marked';
import DOMPurify from 'dompurify';
import { postMessage } from '../bridge';
import { escapeHtml, prepareMarkdownForRender } from './streamingMarkdown';

marked.setOptions({ gfm: true, breaks: false });

interface Props {
  content: string;
}

export function markdownToHtml(content: string): string {
  const renderer = new marked.Renderer();
  renderer.html = function (token: { text: string } | string) {
    const html = typeof token === 'object' ? token.text : token;
    return escapeHtml(html);
  };
  const source = prepareMarkdownForRender(content);
  return marked.parse(source, { renderer }) as string;
}

export const Markdown: React.FC<Props> = ({ content }) => {
  const html = useMemo(() => {
    const raw = markdownToHtml(content);
    return DOMPurify.sanitize(raw);
  }, [content]);

  return (
    <div
      className="markdown-body"
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
};
