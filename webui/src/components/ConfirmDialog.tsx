// Generic confirmation modal. Replaces native window.confirm/alert so every
// destructive confirmation shares the webui modal style (matches DeleteDialog).

import { useEffect, useState } from 'preact/hooks';

interface ConfirmDialogProps {
  title: string;
  /** Already-translated body text (e.g. "确定删除模型「foo」？"). */
  body: string;
  confirmLabel: string;
  cancelLabel: string;
  /** Header glyph. Defaults to the trash icon. */
  icon?: string;
  /** Style the confirm button as destructive (red). Defaults to true. */
  danger?: boolean;
  /** Runs on confirm. If it throws, the message is shown inline and the
   *  dialog stays open; on success the dialog closes. */
  onConfirm: () => Promise<void> | void;
  onClose: () => void;
}

export function ConfirmDialog({
  title,
  body,
  confirmLabel,
  cancelLabel,
  icon = '🗑',
  danger = true,
  onConfirm,
  onClose,
}: ConfirmDialogProps) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape' && !busy) onClose();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [busy, onClose]);

  async function handleConfirm() {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
      onClose();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  }

  return (
    <div
      class="modal-overlay"
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div class="modal-card modal-card-sm">
        <div class="modal-header">
          <span>{icon}</span>
          <h3>{title}</h3>
          <button class="ghost-btn modal-close" onClick={onClose} aria-label={cancelLabel}>
            ×
          </button>
        </div>
        <div class="modal-body">
          <p class="field-hint">{body}</p>
          {error && <div class="modal-error">{error}</div>}
        </div>
        <div class="modal-footer">
          <button class="btn" onClick={onClose} disabled={busy}>
            {cancelLabel}
          </button>
          <button
            class={'btn ' + (danger ? 'btn-danger' : 'btn-primary')}
            onClick={handleConfirm}
            disabled={busy}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
