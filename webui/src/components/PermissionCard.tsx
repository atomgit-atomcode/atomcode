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
  /** Optional override for the decision action. When provided, replaces the
   *  default respondPermission call. Used by live-session approval (POST /live/permission)
   *  so the non-sync /chat path is unchanged. */
  onDecide?: (decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist', toolName?: string) => Promise<void>;
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

export function PermissionCard({ req, onDone, onDecide }: PermissionCardProps) {
  const t = useT();
  const [loading, setLoading] = useState(false);

  async function decide(decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist') {
    if (loading) return;
    setLoading(true);
    try {
      if (onDecide) {
        await onDecide(decision, req.tool_name);
      } else {
        await respondPermission(req.session_id, decision, req.tool_name);
      }
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
      <div class="modal-card permission-card">
        <div class="modal-header permission-header">
          <span class="permission-logo" aria-hidden="true">
            <svg width="22" height="22" viewBox="0 0 24 24" fill="none">
              <rect
                x="6.4"
                y="6.4"
                width="11.2"
                height="11.2"
                rx="2.6"
                transform="rotate(45 12 12)"
                stroke="currentColor"
                stroke-width="1.8"
              />
            </svg>
          </span>
          <h3 class="permission-title">{t('perm.title')}</h3>
          <span class="modal-tag permission-tag">{req.tool_name}</span>
        </div>

        <div class="modal-body">
          {req.reason && <p class="permission-lead">{req.reason}</p>}
          <div class="field-group">
            <span class="modal-label">{t('perm.args')}</span>
            <pre class="tool-body-row-content">{argsDisplay}</pre>
          </div>
        </div>

        <div class="modal-footer permission-footer">
          <button class="btn" disabled={loading} onClick={() => decide('deny')}>
            {t('perm.deny')}
          </button>
          <button class="btn btn-primary" disabled={loading} onClick={() => decide('allow')}>
            {t('perm.approve')}
          </button>
          <button class="btn" disabled={loading} onClick={() => decide('always_allow')}>
            {t('perm.alwaysAllow')}
          </button>
          {req.tool_name.startsWith('mcp__') && (
            <button class="btn" disabled={loading} onClick={() => decide('allow_persist')}>
              {t('perm.allowPersist')}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
