import { useState } from 'preact/hooks';
import { deleteProject, updateProject, deleteSession, listProjectSessions, type ProjectInfo, type SessionMetaWithProject } from '../api';
import { clearWorkspaceSessions, WorkspaceClearError } from '../lib/workspaceActions';
import { useT } from '../settings';
import { ModalOverlay } from './ModalOverlay';

export function WorkspaceActionDialog({ project, action, runningSessionId, runningProjectHash, onDeleted, onDone, onClose }: {
  project: ProjectInfo;
  action: 'remove' | 'clear';
  runningSessionId?: string | null;
  runningProjectHash?: string;
  onDeleted: (session: SessionMetaWithProject) => void;
  onDone: () => void;
  onClose: () => void;
}) {
  const t = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleted, setDeleted] = useState(0);
  const label = t(action === 'remove' ? 'workspace.remove' : 'workspace.clear');
  async function confirm() {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      if (action === 'remove') await deleteProject(project.hash);
      else {
        if (runningSessionId && runningProjectHash === project.hash) {
          throw new WorkspaceClearError(0, 'This workspace has a running session.', true);
        }
        // Remember even an empty group after its last session is cleared.
        await updateProject(project.hash, {});
        await clearWorkspaceSessions(project.hash, runningSessionId, session => {
          setDeleted(n => n + 1);
          onDeleted(session);
        }, { list: listProjectSessions, remove: deleteSession });
      }
      onDone();
      onClose();
    } catch (cause) {
      setError(cause instanceof WorkspaceClearError && cause.busy
        ? t('workspace.running') : t('workspace.failed', { error: cause instanceof Error ? cause.message : String(cause) }));
    } finally { setBusy(false); }
  }
  return <ModalOverlay>
    <div class="modal-card modal-card-sm" role="dialog" aria-modal="true" aria-labelledby="workspace-action-title">
      <div class="modal-header">
        <h3 id="workspace-action-title">{label}</h3>
        <button class="ghost-btn modal-close" disabled={busy} onClick={onClose} aria-label={t('common.cancel')}>×</button>
      </div>
      <div class="modal-body">
        <p class="field-hint">{t(action === 'remove' ? 'workspace.removeBody' : 'workspace.clearBody', { name: project.name || project.working_dir })}</p>
        {action === 'clear' && <p role="status">{t('workspace.deleted', { n: deleted })}</p>}
        {error && <div class="modal-error" role="alert">{error}</div>}
      </div>
      <div class="modal-footer">
        <button class="btn" disabled={busy} onClick={onClose}>{t('common.cancel')}</button>
        <button class="btn btn-danger" disabled={busy} onClick={() => void confirm()}>{busy ? t('workspace.working') : label}</button>
      </div>
    </div>
  </ModalOverlay>;
}
