import { ModalOverlay } from './ModalOverlay';
import { useT } from '../settings';

/**
 * What to do when the phone has no speech recogniser.
 *
 * The situation this exists for is not hypothetical and not obvious: a phone can
 * expose no `RecognitionService` at all while its keyboard dictates perfectly
 * well into any text field (measured on PLT140 — OPLUS / Android 16 with the
 * Baidu IME). The mic therefore failed with no explanation on a device where the
 * person could plainly dictate, which reads as a broken app.
 *
 * Three answers, in the order they cost: install a recogniser (the whole system
 * gains speech), install a local transcription package (private, free, but needs
 * a market install), or record here and send it to a model provider (works now,
 * costs a request). The dialog offers what is actually available and says why
 * when nothing is.
 */
export function VoiceFallbackDialog({
  plugin,
  model,
  onRecord,
  onOpenPlugins,
  onClose,
}: {
  /** Installed local transcription package, by command name. */
  plugin: string | null;
  /** A provider that offers the audio endpoint, when one is configured. */
  model: { provider: string; model: string } | null;
  onRecord: () => void;
  onOpenPlugins: () => void;
  onClose: () => void;
}) {
  const t = useT();
  const canRecord = plugin !== null || model !== null;
  const engine = plugin ?? (model ? `${model.provider} · ${model.model}` : null);

  return (
    <ModalOverlay>
      <div class="modal-card modal-card-sm" role="dialog" aria-modal="true">
        <div class="modal-header">
          <span>🎙</span>
          <h3>{t('chat.voiceFallbackTitle')}</h3>
          <button class="ghost-btn modal-close" onClick={onClose} aria-label={t('common.cancel')}>
            ×
          </button>
        </div>
        <div class="modal-body">
          <p class="field-hint">{t('chat.voiceFallbackBody')}</p>
          {canRecord ? (
            <p class="field-hint">{t('chat.voiceFallbackEngine', { engine: engine ?? '' })}</p>
          ) : (
            <div class="modal-error">{t('chat.voiceFallbackNone')}</div>
          )}
          <p class="field-hint">{t('chat.voiceFallbackRecognizerHint')}</p>
        </div>
        <div class="modal-footer">
          <button class="btn" onClick={onOpenPlugins}>
            {t('chat.voiceFallbackPlugin')}
          </button>
          <button class="btn btn-primary" onClick={onRecord} disabled={!canRecord}>
            {t('chat.voiceFallbackRecord')}
          </button>
        </div>
      </div>
    </ModalOverlay>
  );
}
