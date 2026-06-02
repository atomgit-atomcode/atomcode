// Rename + delete dialogs for a session (triggered from the sidebar item menu).

import { useEffect, useRef, useState } from 'preact/hooks';
import { renameSession, deleteSession, SessionMetaWithProject } from '../api';
import { useT } from '../settings';

interface RenameDialogProps {
  session: SessionMetaWithProject;
  onClose: () => void;
  /** Called with the new name after a successful rename. */
  onDone: (name: string) => void;
}

export function RenameDialog({ session, onClose, onDone }: RenameDialogProps) {
  const t = useT();
  const [name, setName] = useState(session.name || '');
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  async function submit() {
    const n = name.trim();
    if (!n || busy) return;
    setBusy(true);
    try {
      await renameSession(session.project_hash, session.id, n);
      onDone(n);
      onClose();
    } catch {
      setBusy(false);
    }
  }

  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class="modal-card modal-card-sm">
        <div class="modal-header">
          <span>✎</span>
          <h3>{t('rename.title')}</h3>
          <button class="ghost-btn modal-close" onClick={onClose} aria-label={t('common.cancel')}>
            ×
          </button>
        </div>
        <div class="modal-body">
          <input
            ref={inputRef}
            class="menu-input"
            type="text"
            value={name}
            placeholder={t('rename.placeholder')}
            onInput={(e) => setName((e.target as HTMLInputElement).value)}
            onKeyDown={(e) => {
              // 忽略输入法组字阶段的回车（选词确认），避免误触发保存。
              if (e.isComposing) return;
              if (e.key === 'Enter') submit();
            }}
          />
        </div>
        <div class="modal-footer">
          <button class="btn" onClick={onClose}>
            {t('common.cancel')}
          </button>
          <button class="btn btn-primary" onClick={submit} disabled={busy || !name.trim()}>
            {t('rename.confirm')}
          </button>
        </div>
      </div>
    </div>
  );
}

interface DeleteDialogProps {
  session: SessionMetaWithProject;
  onClose: () => void;
  /** Called after a successful delete. */
  onDone: () => void;
}

export function DeleteDialog({ session, onClose, onDone }: DeleteDialogProps) {
  const t = useT();
  const [busy, setBusy] = useState(false);
  const name = session.name || session.id.slice(0, 8);

  async function confirm() {
    if (busy) return;
    setBusy(true);
    try {
      await deleteSession(session.project_hash, session.id);
      onDone();
      onClose();
    } catch {
      setBusy(false);
    }
  }

  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div class="modal-card modal-card-sm">
        <div class="modal-header">
          <span>🗑</span>
          <h3>{t('delete.title')}</h3>
          <button class="ghost-btn modal-close" onClick={onClose} aria-label={t('common.cancel')}>
            ×
          </button>
        </div>
        <div class="modal-body">
          <p class="field-hint">{t('delete.body', { name })}</p>
        </div>
        <div class="modal-footer">
          <button class="btn" onClick={onClose}>
            {t('common.cancel')}
          </button>
          <button class="btn btn-danger" onClick={confirm} disabled={busy}>
            {t('delete.confirm')}
          </button>
        </div>
      </div>
    </div>
  );
}
