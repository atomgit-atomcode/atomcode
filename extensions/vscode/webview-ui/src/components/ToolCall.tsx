import React, { useState } from 'react';
import { ToolCallData } from '../state/types';
import { formatToolArgs } from '../utils/format';

interface ToolCallProps {
  tool: ToolCallData;
}

export function ToolCall({ tool }: ToolCallProps) {
  const [expanded, setExpanded] = useState(false);
  const secondary = formatToolArgs(tool.name, tool.args);

  const annotationClass =
    tool.status === 'error' ? 'error' :
    tool.status === 'done' ? 'success' : '';

  const annotationText =
    tool.status === 'running' ? undefined :
    tool.status === 'error' ? 'error' :
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
            <div className="tool-body-row-label">IN</div>
            <div className="tool-body-row-content clipped">{tool.args}</div>
          </div>
          {tool.output !== undefined && (
            <div className="tool-body-row">
              <div className="tool-body-row-label">OUT</div>
              <div className="tool-body-row-content clipped">{tool.output}</div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
