import React, { useCallback } from 'react';
import { PermissionRequestData } from '../state/types';
import { useChatContext } from '../state/ChatProvider';
import { postMessage } from '../vscode';
import { formatToolArgs } from '../utils/format';

interface PermissionRequestProps {
  request: PermissionRequestData;
}

export function PermissionRequest({ request }: PermissionRequestProps) {
  const { dispatch } = useChatContext();

  const handleRespond = useCallback((allowed: boolean) => {
    dispatch({ type: 'PERMISSION_RESPOND', id: request.id, allowed });
    postMessage({ type: 'permissionResponse', id: request.id, allowed });
  }, [request.id, dispatch]);

  if (request.status !== 'pending') return null;

  const secondary = formatToolArgs(request.toolName, request.args);
  const isBash = request.toolName.toLowerCase().includes('bash');
  let command = '';
  if (isBash) {
    try {
      const parsed = JSON.parse(request.args) as Record<string, string>;
      command = parsed.command ?? '';
    } catch { /* ignore */ }
  }

  return (
    <div
      className="tool-body"
      style={request.isDestructive ? { borderColor: '#c74e3966' } : undefined}
    >
      <div style={{ padding: '12px' }}>
        {isBash && command ? (
          <div style={{ display: 'flex', alignItems: 'flex-start', gap: 8, marginBottom: 12 }}>
            <span style={{ fontFamily: 'var(--app-monospace-font-family)', color: 'var(--app-secondary-foreground)', flexShrink: 0 }}>$</span>
            <span style={{ fontFamily: 'var(--app-monospace-font-family)', fontSize: 'var(--app-monospace-font-size)', whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>{command}</span>
          </div>
        ) : (
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 12 }}>
            <span className="tool-name">{request.toolName}</span>
            {secondary && <span className="tool-name-secondary">{secondary}</span>}
          </div>
        )}
        {request.isDestructive && (
          <div style={{ marginBottom: 12 }}>
            <span className="tool-annotation destructive">destructive</span>
          </div>
        )}
        <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end' }}>
          <button
            onClick={() => handleRespond(false)}
            style={{
              background: 'transparent',
              border: '1px solid var(--app-input-border)',
              color: 'var(--app-primary-foreground)',
              borderRadius: 5, padding: '4px 14px', fontSize: 12, cursor: 'pointer',
            }}
          >
            Deny
          </button>
          <button
            onClick={() => handleRespond(true)}
            style={{
              background: request.isDestructive ? '#c74e39' : 'var(--app-brand-button)',
              border: 'none',
              color: 'var(--app-brand-ivory)',
              borderRadius: 5, padding: '4px 14px', fontSize: 12, cursor: 'pointer',
            }}
          >
            Allow
          </button>
        </div>
      </div>
    </div>
  );
}
