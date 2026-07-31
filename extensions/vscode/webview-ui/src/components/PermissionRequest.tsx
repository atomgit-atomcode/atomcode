import React, { useCallback } from 'react';
import { PermissionDecision, PermissionRequestData } from '../state/types';
import { useChatContext } from '../state/ChatProvider';
import { postMessage } from '../vscode';
import { formatToolArgs } from '../utils/format';
import { useT } from '../i18n';

interface PermissionRequestProps {
  request: PermissionRequestData;
}

export function PermissionRequest({ request }: PermissionRequestProps) {
  const { dispatch } = useChatContext();
  const t = useT();

  const handleRespond = useCallback((decision: PermissionDecision) => {
    dispatch({ type: 'PERMISSION_RESPOND', id: request.id, decision });
    postMessage({
      type: 'permissionResponse',
      sessionId: request.sessionId,
      id: request.id,
      toolName: request.toolName,
      decision,
    });
  }, [request.id, request.sessionId, request.toolName, dispatch]);

  if (request.status === 'allowed' || request.status === 'denied') return null;

  const secondary = formatToolArgs(request.toolName, request.args);
  const isBash = request.toolName.toLowerCase().includes('bash');
  const canPersist = request.toolName.startsWith('mcp__');
  const submitting = request.status === 'submitting';
  const decisionLabel = (decision: PermissionDecision, label: string, submittingLabel: string) =>
    submitting && request.decision === decision ? submittingLabel : label;
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
        {request.reason && (
          <div style={{ marginBottom: 12, color: 'var(--app-secondary-foreground)', fontSize: 12 }}>
            {request.reason}
          </div>
        )}
        {request.error && (
          <div style={{ marginBottom: 12, color: 'var(--app-error-foreground, #c74e39)', fontSize: 12 }}>
            {request.error}
          </div>
        )}
        {request.isDestructive && (
          <div style={{ marginBottom: 12 }}>
            <span className="tool-annotation destructive">{t('tool.destructive')}</span>
          </div>
        )}
        <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end', flexWrap: 'wrap' }}>
          <button
            onClick={() => handleRespond('deny')}
            disabled={submitting}
            style={{
              background: 'transparent',
              border: '1px solid var(--app-input-border)',
              color: 'var(--app-primary-foreground)',
              borderRadius: 5, padding: '4px 14px', fontSize: 12, cursor: submitting ? 'default' : 'pointer',
              opacity: submitting ? 0.55 : 1,
            }}
          >
            {decisionLabel('deny', t('permission.deny'), t('permission.denying'))}
          </button>
          <button
            onClick={() => handleRespond('allow')}
            disabled={submitting}
            style={{
              background: request.isDestructive ? '#c74e39' : 'var(--app-brand-button)',
              border: 'none',
              color: 'var(--app-brand-ivory)',
              borderRadius: 5, padding: '4px 14px', fontSize: 12, cursor: submitting ? 'default' : 'pointer',
              opacity: submitting ? 0.7 : 1,
            }}
          >
            {decisionLabel('allow', t('permission.allow'), t('permission.allowing'))}
          </button>
          <button
            onClick={() => handleRespond('always_allow')}
            disabled={submitting}
            style={{
              background: 'transparent',
              border: '1px solid var(--app-input-border)',
              color: 'var(--app-primary-foreground)',
              borderRadius: 5, padding: '4px 14px', fontSize: 12, cursor: submitting ? 'default' : 'pointer',
              opacity: submitting ? 0.55 : 1,
            }}
          >
            {decisionLabel('always_allow', t('permission.alwaysAllow'), t('permission.alwaysAllowing'))}
          </button>
          {canPersist && (
            <button
              onClick={() => handleRespond('allow_persist')}
              disabled={submitting}
              style={{
                background: 'transparent',
                border: '1px solid var(--app-input-border)',
                color: 'var(--app-primary-foreground)',
                borderRadius: 5, padding: '4px 14px', fontSize: 12, cursor: submitting ? 'default' : 'pointer',
                opacity: submitting ? 0.55 : 1,
              }}
            >
              {decisionLabel('allow_persist', t('permission.allowPersist'), t('permission.persisting'))}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
