import React, { useState } from 'react';
import { ToolCallData } from '../state/types';
import { getToolIcon, formatToolArgs } from '../utils/format';

interface ToolCallProps {
  tool: ToolCallData;
}

export function ToolCall({ tool }: ToolCallProps) {
  const [expanded, setExpanded] = useState(false);

  const icon = getToolIcon(tool.name);
  const argsSummary = formatToolArgs(tool.name, tool.args);
  const statusIcon =
    tool.status === 'running' ? '⟳' : tool.status === 'error' ? '✗' : '✓';
  const statusClass = `tool-status tool-status-${tool.status}`;

  const duration =
    tool.durationMs !== undefined ? `${(tool.durationMs / 1000).toFixed(1)}s` : '';

  return (
    <div className={`tool-call${expanded ? ' expanded' : ''}`}>
      <button className="tool-call-header" onClick={() => setExpanded(!expanded)}>
        <span className="tool-call-icon">{icon}</span>
        <span className="tool-call-name">{tool.name}</span>
        {argsSummary && <span className="tool-call-args">{argsSummary}</span>}
        <span className="tool-call-spacer" />
        {duration && <span className="tool-call-duration">{duration}</span>}
        <span className={statusClass}>{statusIcon}</span>
        <span className="tool-call-chevron">{expanded ? '▴' : '▾'}</span>
      </button>
      {expanded && (
        <div className="tool-call-body">
          <pre className="tool-call-output">{tool.output ?? 'No output yet...'}</pre>
        </div>
      )}
    </div>
  );
}
