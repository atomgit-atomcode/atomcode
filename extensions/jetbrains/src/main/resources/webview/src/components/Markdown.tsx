import React, { useMemo } from 'react';
import { marked } from 'marked';
import DOMPurify from 'dompurify';
import { postMessage } from '../bridge';

marked.setOptions({ gfm: true, breaks: false });

interface Props {
  content: string;
}

export const Markdown: React.FC<Props> = ({ content }) => {
  const html = useMemo(() => {
    const raw = marked.parse(content) as string;
    return DOMPurify.sanitize(raw);
  }, [content]);

  return (
    <div
      className="markdown-body"
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
};
