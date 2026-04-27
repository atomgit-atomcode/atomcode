import React, { useState, useCallback } from 'react';
import { postMessage } from '../vscode';

interface CodeBlockProps {
  language: string;
  code: string;
}

export function CodeBlock({ language, code }: CodeBlockProps) {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [code]);

  const handleApply = useCallback(() => {
    postMessage({ type: 'applyCode', code, language });
  }, [code, language]);

  const handleInsert = useCallback(() => {
    postMessage({ type: 'insertCode', code, language });
  }, [code, language]);

  return (
    <div className="code-block">
      <div className="code-block-header">
        <span className="code-block-lang">{language || 'code'}</span>
        <div className="code-block-actions">
          <button className="code-action-btn" onClick={handleCopy} title="Copy code">
            {copied ? 'Copied!' : 'Copy'}
          </button>
          <button className="code-action-btn" onClick={handleApply} title="Apply to file">
            Apply
          </button>
          <button className="code-action-btn" onClick={handleInsert} title="Insert at cursor">
            Insert
          </button>
        </div>
      </div>
      <div className="code-block-content">
        <pre>
          <code className={language ? `language-${language}` : ''}>{code}</code>
        </pre>
      </div>
    </div>
  );
}
