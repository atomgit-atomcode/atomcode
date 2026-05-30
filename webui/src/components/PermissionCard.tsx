// Task 14 — Tool approval modal card

import { useState } from 'preact/hooks';
import { respondPermission } from '../api';

interface PermissionRequest {
  session_id: string;
  tool_name: string;
  reason: string;
  call_id: string;
  arguments: unknown;
}

interface PermissionCardProps {
  req: PermissionRequest;
  onDone: () => void;
}

function formatArgs(args: unknown): string {
  if (typeof args === 'string') {
    // Try to pretty-print if it looks like JSON
    try {
      const parsed = JSON.parse(args);
      return JSON.stringify(parsed, null, 2);
    } catch {
      return args;
    }
  }
  try {
    return JSON.stringify(args, null, 2);
  } catch {
    return String(args);
  }
}

export function PermissionCard({ req, onDone }: PermissionCardProps) {
  const [loading, setLoading] = useState(false);

  async function decide(decision: 'allow' | 'deny' | 'always_allow') {
    if (loading) return;
    setLoading(true);
    try {
      await respondPermission(req.session_id, decision);
    } catch {
      // Best-effort; proceed to dismiss regardless
    } finally {
      setLoading(false);
      onDone();
    }
  }

  const argsDisplay = formatArgs(req.arguments);

  return (
    <div class="modal-overlay">
      <div class="modal-card">
        <div class="modal-header">
          <span>⚠</span>
          <h3>工具请求批准</h3>
          <span class="modal-tag">{req.tool_name}</span>
        </div>

        <div class="modal-body">
          {req.reason && <p class="field-hint">{req.reason}</p>}
          <div class="field-group">
            <span class="modal-label">参数</span>
            <pre class="tool-body-row-content">{argsDisplay}</pre>
          </div>
        </div>

        <div class="modal-footer">
          <button class="btn" disabled={loading} onClick={() => decide('deny')}>
            拒绝
          </button>
          <button class="btn btn-primary" disabled={loading} onClick={() => decide('allow')}>
            批准
          </button>
          <button class="btn btn-success" disabled={loading} onClick={() => decide('always_allow')}>
            本会话总是允许
          </button>
        </div>
      </div>
    </div>
  );
}
