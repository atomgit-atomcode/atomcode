import React from 'react';
import type { PermissionViewModel } from '../state/types';
import { postMessage } from '../bridge';

interface Props { permission: PermissionViewModel }

export const PermissionInline: React.FC<Props> = ({ permission }) => {
  const cls = permission.severity === 'critical' ? 'permission-critical'
    : permission.severity === 'destructive' ? 'permission-destructive' : 'permission-safe';

  const decide = (decision: 'allow' | 'deny' | 'always_allow' | 'allow_persist') => {
    postMessage({ type: 'permission_decision', call_id: permission.call_id, decision });
  };

  return (
    <div className={`permission-inline ${cls}`}>
      <div className="permission-header">⚠ Permission Required</div>
      <div className="permission-tool">{permission.tool_name}</div>
      <div className="permission-reason">{permission.reason}</div>
      <div className="permission-actions">
        <button className="permission-btn deny" onClick={() => decide('deny')}>Deny</button>
        <button className="permission-btn allow" onClick={() => decide('allow')}>Allow Once</button>
        <button className="permission-btn always" onClick={() => decide('always_allow')}>Always Allow</button>
        {permission.allow_persist && (
          <button className="permission-btn persist" onClick={() => decide('allow_persist')}>Allow & Persist</button>
        )}
      </div>
    </div>
  );
};
