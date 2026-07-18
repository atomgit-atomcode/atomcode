// User input request card — mirrors PermissionCard structure/styling.
// Shown when the daemon emits a `user_input_request` SSE event on the live stream.

import { useState } from 'preact/hooks';
import { postLiveUserInput, UserInputRequestEvent } from '../api';
import { useT } from '../settings';

interface UserInputCardProps {
  req: UserInputRequestEvent;
  onDone: () => void;
}

export function UserInputCard({ req, onDone }: UserInputCardProps) {
  const t = useT();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // single: at most one selected label
  const [singleSelected, setSingleSelected] = useState<string | null>(null);
  // multiple: set of selected labels
  const [multiSelected, setMultiSelected] = useState<Set<string>>(new Set());
  // free-text input (used for 'text' mode AND as an "Other" addendum for single/multiple)
  const [freeText, setFreeText] = useState('');

  function toggleMulti(label: string) {
    setMultiSelected((prev) => {
      const next = new Set(prev);
      if (next.has(label)) next.delete(label); else next.add(label);
      return next;
    });
  }

  // FIX 4 (webui): text mode — disable Submit when textarea is empty.
  const textEmpty = req.mode === 'text' && freeText.trim() === '';

  async function submit() {
    if (loading) return;
    setLoading(true);
    setError(null);
    try {
      let body: Parameters<typeof postLiveUserInput>[0];
      if (req.mode === 'text') {
        body = { request_id: req.request_id, declined: false, selected: [], text: freeText || null };
      } else if (req.mode === 'single') {
        // FIX 5: enforce single semantics — free-text "Other" takes priority over radio.
        const chosen: string[] = freeText.trim()
          ? [freeText.trim()]
          : singleSelected
            ? [singleSelected]
            : [];
        body = { request_id: req.request_id, declined: false, selected: chosen, text: null };
      } else {
        // multiple
        const chosen = Array.from(multiSelected);
        if (freeText.trim()) chosen.push(freeText.trim());
        body = { request_id: req.request_id, declined: false, selected: chosen, text: null };
      }
      await postLiveUserInput(body);
      // FIX 2: only dismiss on success.
      onDone();
    } catch {
      // FIX 2: keep card open on failure, show inline error so user can retry.
      setLoading(false);
      setError(t('userInput.error'));
    }
  }

  async function skip() {
    if (loading) return;
    setLoading(true);
    try {
      await postLiveUserInput({ request_id: req.request_id, declined: true, selected: [], text: null });
    } catch {
      // Best-effort — Skip dismisses even on POST failure (declining is best-effort).
    } finally {
      setLoading(false);
      onDone();
    }
  }

  return (
    <div class="modal-overlay">
      <div class="modal-card permission-card">
        <div class="modal-header permission-header">
          <span class="permission-logo" aria-hidden="true">
            <svg width="22" height="22" viewBox="0 0 24 24" fill="none">
              <circle cx="12" cy="12" r="9" stroke="currentColor" stroke-width="1.8" />
              <line x1="12" y1="8" x2="12" y2="13" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
              <circle cx="12" cy="16" r="0.9" fill="currentColor" />
            </svg>
          </span>
          <h3 class="permission-title">{req.header}</h3>
        </div>

        <div class="modal-body">
          <p class="permission-lead">{req.question}</p>

          {req.mode === 'single' && req.options.length > 0 && (
            <div class="field-group">
              {req.options.map((opt) => (
                <label key={opt.label} class="user-input-option">
                  <input
                    type="radio"
                    name={`uiq-${req.request_id}`}
                    value={opt.label}
                    checked={singleSelected === opt.label}
                    onChange={() => setSingleSelected(opt.label)}
                    disabled={loading}
                  />
                  <span class="user-input-option-label">{opt.label}</span>
                  {opt.description && (
                    <span class="user-input-option-desc">{opt.description}</span>
                  )}
                </label>
              ))}
            </div>
          )}

          {req.mode === 'multiple' && req.options.length > 0 && (
            <div class="field-group">
              {req.options.map((opt) => (
                <label key={opt.label} class="user-input-option">
                  <input
                    type="checkbox"
                    value={opt.label}
                    checked={multiSelected.has(opt.label)}
                    onChange={() => toggleMulti(opt.label)}
                    disabled={loading}
                  />
                  <span class="user-input-option-label">{opt.label}</span>
                  {opt.description && (
                    <span class="user-input-option-desc">{opt.description}</span>
                  )}
                </label>
              ))}
            </div>
          )}

          {req.mode === 'text' ? (
            <div class="field-group">
              <textarea
                class="user-input-textarea"
                rows={4}
                value={freeText}
                onInput={(e) => setFreeText((e.target as HTMLTextAreaElement).value)}
                disabled={loading}
                placeholder=""
              />
            </div>
          ) : (
            <div class="field-group">
              <span class="modal-label">{t('userInput.other')}</span>
              <input
                type="text"
                class="user-input-text"
                value={freeText}
                onInput={(e) => setFreeText((e.target as HTMLInputElement).value)}
                disabled={loading}
              />
            </div>
          )}

          {error && (
            <p class="user-input-error">{error}</p>
          )}
        </div>

        <div class="modal-footer permission-footer">
          <button class="btn" disabled={loading} onClick={skip}>
            {t('userInput.skip')}
          </button>
          <button class="btn btn-primary" disabled={loading || textEmpty} onClick={submit}>
            {t('userInput.submit')}
          </button>
        </div>
      </div>
    </div>
  );
}
