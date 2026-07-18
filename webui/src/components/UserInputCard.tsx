// User input request card — mirrors PermissionCard structure/styling.
// Shown when the daemon emits a `user_input_request` SSE event on the live stream.

import { useEffect, useRef, useState } from 'preact/hooks';
import { postLiveUserInput, UserInputRequestEvent } from '../api';
import { useT } from '../settings';

interface UserInputCardProps {
  req: UserInputRequestEvent;
  onDone: () => void;
}

const OTHER_SENTINEL = '__other__';

export function UserInputCard({ req, onDone }: UserInputCardProps) {
  const t = useT();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // single: selected label, or OTHER_SENTINEL when the "Other" radio is chosen
  const [singleSelected, setSingleSelected] = useState<string | null>(null);
  // multiple: set of selected concrete labels
  const [multiSelected, setMultiSelected] = useState<Set<string>>(new Set());
  // whether the "Other" checkbox is checked (multiple mode)
  const [otherChecked, setOtherChecked] = useState(false);
  // free-text value shared by both single-Other and multiple-Other paths
  const [freeText, setFreeText] = useState('');

  // Autofocus the free-text input when it becomes visible
  const freeTextRef = useRef<HTMLInputElement>(null);
  const singleOtherActive = req.mode === 'single' && singleSelected === OTHER_SENTINEL;
  const multiOtherActive  = req.mode === 'multiple' && otherChecked;

  useEffect(() => {
    if ((singleOtherActive || multiOtherActive) && freeTextRef.current) {
      freeTextRef.current.focus();
    }
  }, [singleOtherActive, multiOtherActive]);

  function toggleMulti(label: string) {
    setMultiSelected((prev) => {
      const next = new Set(prev);
      if (next.has(label)) next.delete(label); else next.add(label);
      return next;
    });
  }

  // Derive whether Submit should be disabled
  const submitDisabled: boolean = (() => {
    if (loading) return true;
    if (req.mode === 'text') return freeText.trim() === '';
    if (req.mode === 'single') {
      if (singleSelected === null) return true;                       // nothing picked
      if (singleSelected === OTHER_SENTINEL) return freeText.trim() === ''; // Other but empty
      return false;
    }
    // multiple
    const hasConcreteChecked = multiSelected.size > 0;
    const hasOtherText = otherChecked && freeText.trim() !== '';
    return !hasConcreteChecked && !hasOtherText;
  })();

  async function submit() {
    if (loading || submitDisabled) return;
    setLoading(true);
    setError(null);
    try {
      let body: Parameters<typeof postLiveUserInput>[0];
      if (req.mode === 'text') {
        body = { request_id: req.request_id, declined: false, selected: [], text: freeText || null };
      } else if (req.mode === 'single') {
        const chosen: string[] =
          singleSelected === OTHER_SENTINEL
            ? [freeText.trim()]
            : singleSelected
              ? [singleSelected]
              : [];
        body = { request_id: req.request_id, declined: false, selected: chosen, text: null };
      } else {
        // multiple
        const chosen = Array.from(multiSelected);
        if (otherChecked && freeText.trim()) chosen.push(freeText.trim());
        body = { request_id: req.request_id, declined: false, selected: chosen, text: null };
      }
      await postLiveUserInput(body);
      onDone();
    } catch {
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
                  <span class="user-input-option-body">
                    <span class="user-input-option-label">{opt.label}</span>
                    {opt.description && (
                      <span class="user-input-option-desc">{opt.description}</span>
                    )}
                  </span>
                </label>
              ))}
              {/* "Other" as the last radio in the same group */}
              <label class="user-input-option">
                <input
                  type="radio"
                  name={`uiq-${req.request_id}`}
                  value={OTHER_SENTINEL}
                  checked={singleSelected === OTHER_SENTINEL}
                  onChange={() => setSingleSelected(OTHER_SENTINEL)}
                  disabled={loading}
                />
                <span class="user-input-option-body">
                  <span class="user-input-option-label">{t('userInput.other')}</span>
                </span>
              </label>
              {singleOtherActive && (
                <input
                  ref={freeTextRef}
                  type="text"
                  class="user-input-text"
                  value={freeText}
                  onInput={(e) => setFreeText((e.target as HTMLInputElement).value)}
                  disabled={loading}
                  placeholder="输入自己的答案…"
                />
              )}
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
                  <span class="user-input-option-body">
                    <span class="user-input-option-label">{opt.label}</span>
                    {opt.description && (
                      <span class="user-input-option-desc">{opt.description}</span>
                    )}
                  </span>
                </label>
              ))}
              {/* "Other" as the last checkbox */}
              <label class="user-input-option">
                <input
                  type="checkbox"
                  checked={otherChecked}
                  onChange={() => setOtherChecked((v) => !v)}
                  disabled={loading}
                />
                <span class="user-input-option-body">
                  <span class="user-input-option-label">{t('userInput.other')}</span>
                </span>
              </label>
              {multiOtherActive && (
                <input
                  ref={freeTextRef}
                  type="text"
                  class="user-input-text"
                  value={freeText}
                  onInput={(e) => setFreeText((e.target as HTMLInputElement).value)}
                  disabled={loading}
                  placeholder="输入自己的答案…"
                />
              )}
            </div>
          )}

          {req.mode === 'text' && (
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
          )}

          {error && (
            <p class="user-input-error">{error}</p>
          )}
        </div>

        <div class="modal-footer permission-footer">
          <button class="btn" disabled={loading} onClick={skip}>
            {t('userInput.skip')}
          </button>
          <button class="btn btn-primary" disabled={submitDisabled} onClick={submit}>
            {t('userInput.submit')}
          </button>
        </div>
      </div>
    </div>
  );
}
