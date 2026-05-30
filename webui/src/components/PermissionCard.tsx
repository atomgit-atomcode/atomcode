// Task 14 — Tool approval modal card

import { useState } from 'preact/hooks';
import { respondPermission } from '../api';
import { useT } from '../settings';

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
  const t = useT();
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
          <h3>{t('perm.title')}</h3>
          <span class="modal-tag">{req.tool_name}</span>
        </div>

        <div class="modal-body">
          {req.reason && <p class="field-hint">{req.reason}</p>}
          <div class="field-group">
            <span class="modal-label">{t('perm.args')}</span>
            <pre class="tool-body-row-content">{argsDisplay}</pre>
          </div>
        </div>

        <div class="modal-footer">
          <button class="btn" disabled={loading} onClick={() => decide('deny')}>
            {t('perm.deny')}
          </button>
          <button class="btn btn-primary" disabled={loading} onClick={() => decide('allow')}>
            {t('perm.approve')}
          </button>
          <button class="btn btn-success" disabled={loading} onClick={() => decide('always_allow')}>
            {t('perm.alwaysAllow')}
          </button>
        </div>
      </div>
    </div>
  );
}
