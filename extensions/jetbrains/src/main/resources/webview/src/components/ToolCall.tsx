import React, { useState } from 'react';
import type { ToolCallViewModel } from '../state/types';

interface Props { tool: ToolCallViewModel }

const icons: Record<string, string> = { queued: '○', running: '◌', success: '✓', error: '✗' };

function formatArgs(json: string): string {
  try { return JSON.stringify(JSON.parse(json), null, 2); } catch { return json; }
}

export const ToolCall: React.FC<Props> = ({ tool }) => {
  const [expanded, setExpanded] = useState(false);
  const cls = `tool-status-${tool.status}`;

  return (
    <div className="tool-call">
      <div className="tool-call-header" onClick={() => setExpanded(!expanded)}>
        <span className={cls}>{icons[tool.status]}</span>
        <span className="tool-name">{tool.name}</span>
        <span className="tool-duration">{tool.duration_ms > 0 ? `${tool.duration_ms}ms` : ''}</span>
        <span className="tool-toggle">{expanded ? '▾' : '▸'}</span>
      </div>
      {expanded && (
        <div className="tool-call-body">
          <div className="tool-section">
            <div className="tool-section-title">Input</div>
            <pre className="tool-code">{formatArgs(tool.arguments)}</pre>
          </div>
          {tool.output && (
            <div className="tool-section">
              <div className="tool-section-title">Output</div>
              <pre className="tool-code">{tool.output}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
};
