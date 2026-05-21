import React, { useState, useCallback } from 'react';
import { ToolCallData } from '../state/types';
import { formatToolArgs } from '../utils/format';
import { DiffView } from './DiffView';

interface ToolCallProps {
  tool: ToolCallData;
}

function isDiffContent(output: string): boolean {
  return output.includes('@@') && (output.includes('+') || output.includes('-'));
}

function CopyButton({ text, label }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback((e: React.MouseEvent) => {
    e.stopPropagation();
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [text]);

  return (
    <button className="tool-body-row-copy" onClick={handleCopy} title={copied ? 'Copied!' : `Copy ${label || ''}`}>
      {copied ? '✓' : '📋'}
    </button>
  );
}

export function ToolCall({ tool }: ToolCallProps) {
  const [expanded, setExpanded] = useState(false);
  const secondary = formatToolArgs(tool.name, tool.args);

  const isEditTool = /edit|write/i.test(tool.name);
  const showDiff = isEditTool && tool.output && isDiffContent(tool.output);

  const annotationClass =
    tool.status === 'error' ? 'error' :
    tool.status === 'done' ? 'success' :
    tool.status === 'queued' ? 'queued' : '';

  const annotationText =
    tool.status === 'queued' ? 'waiting' :
    tool.status === 'running' ? undefined :
    tool.status === 'error' ? 'error' :
    isEditTool && tool.status === 'done' ? 'applied' :
    tool.durationMs !== undefined ? `${(tool.durationMs / 1000).toFixed(1)}s` : 'done';

  return (
    <div className="tool-body">
      <div className="tool-header" onClick={() => setExpanded(!expanded)}>
        <span className="tool-name">{tool.name}</span>
        {secondary && <span className="tool-name-secondary">{secondary}</span>}
        {tool.status === 'running' && (
          <span className="tool-annotation" style={{ color: 'var(--app-spinner-foreground)' }}>
            <span style={{ display: 'inline-block', animation: 'spin 1.5s steps(30) infinite' }}>⟳</span>
          </span>
        )}
        {annotationText && (
          <span className={`tool-annotation ${annotationClass}`}>{annotationText}</span>
        )}
        <span className={`tool-chevron${expanded ? ' expanded' : ''}`}>▾</span>
      </div>
      {expanded && (
        <div className="tool-body-grid">
          <div className="tool-body-row">
            <div className="tool-body-row-label-row">
              <span className="tool-body-row-label">IN</span>
              <CopyButton text={tool.args} label="input" />
            </div>
            <div className="tool-body-row-content clipped">{tool.args}</div>
          </div>
          {tool.output !== undefined && (
            <div className="tool-body-row">
              <div className="tool-body-row-label-row">
                <span className="tool-body-row-label">OUT</span>
                <CopyButton text={tool.output} label="output" />
              </div>
              <div className="tool-body-row-content" style={showDiff ? { maxHeight: 'none' } : undefined}>
                {showDiff ? <DiffView content={tool.output} /> : tool.output}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
