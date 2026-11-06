// User input request card — mirrors PermissionCard structure/styling.
// Shown when the daemon emits a `user_input_request` on `/chat` or `/live`.
//
// Single question → post the answer directly. Multi-question batch (`req.questions`)
// → a sequential stepper (one question at a time, reusing the same question body),
// accumulating answers locally and posting ONE batched response at the end. This is
// the webui fallback for the TUI's Tab-navigated batch form.

import { useEffect, useRef, useState } from 'preact/hooks';
import { isUserInputBatch, UserInputAnswer, UserInputQuestion, UserInputRequestEvent, UserInputResponseBody } from '../api';
import { useT } from '../settings';
import { InteractionDock } from './InteractionDock';
import { OTHER_SENTINEL, emptyAnswer, declinedAnswer, buildAnswer, answerReady, beginOptionEdit, saveOptionEdit, cancelOptionEdit, type AnswerState } from '../lib/userInput';

interface UserInputCardProps {
  req: UserInputRequestEvent;
  onDone: () => void;
  submitAnswer: (body: UserInputAnswer) => Promise<{ accepted: boolean }>;
}

/** The options/text body for one question. Reused by the single card and the batch stepper. */
function QuestionBody({ q, nameSuffix, state, setState, disabled, onConfirm }: {
  q: UserInputQuestion;
  nameSuffix: string;
  state: AnswerState;
  setState: (s: AnswerState) => void;
  disabled: boolean;
  onConfirm: (s: AnswerState) => void;
}) {
  const t = useT();
  const freeTextRef = useRef<HTMLInputElement>(null);
  const editRef = useRef<HTMLInputElement>(null);
  const editButtons = useRef<Array<HTMLButtonElement | null>>([]);
  const lastEdited = useRef<number | null>(null);
  const showOther = q.custom !== false;
  const otherActive = q.mode === 'single' ? state.singleSelected === OTHER_SENTINEL : state.otherChecked;
  useEffect(() => {
    if (otherActive) freeTextRef.current?.focus();
  }, [otherActive]);
  useEffect(() => {
    if (state.editing) {
      const input = editRef.current;
      input?.focus();
      input?.setSelectionRange(input.value.length, input.value.length);
    } else if (lastEdited.current !== null) {
      editButtons.current[lastEdited.current]?.focus();
    }
    lastEdited.current = state.editing?.option ?? null;
  }, [state.editing?.option]);

  const set = (patch: Partial<AnswerState>) => setState({ ...state, ...patch });
  function toggle(option: number) {
    if (q.mode === 'single') return set({ singleSelected: option });
    const next = new Set(state.multiSelected);
    if (next.has(option)) next.delete(option);
    else next.add(option);
    set({ multiSelected: next });
  }
  function cancelEdit() {
    setState(cancelOptionEdit(state));
  }
  function saveEdit() {
    const next = saveOptionEdit(q, state);
    if (next === state) return;
    setState(next);
    if (q.mode === 'single') onConfirm(next);
  }

  return (
    <>
      <p class="permission-lead">{q.question}</p>
      {q.mode !== 'text' && q.options.length > 0 && (
        <div class="field-group">
          {q.options.map((opt, i) => (
            <div key={i} class="user-input-option-row">
              <div class="user-input-option-main">
                <label class="user-input-option">
                  <input
                    type={q.mode === 'single' ? 'radio' : 'checkbox'}
                    name={`uiq-${nameSuffix}`}
                    checked={q.mode === 'single' ? state.singleSelected === i : state.multiSelected.has(i)}
                    onChange={() => toggle(i)}
                    disabled={disabled || state.editing !== null}
                  />
                  <span class="user-input-option-body">
                    <span class="user-input-option-label">{state.replacements[i] ?? opt.label}</span>
                    {opt.description && <span class="user-input-option-desc">{opt.description}</span>}
                  </span>
                </label>
                {showOther && (
                  <button
                    ref={(el) => { editButtons.current[i] = el; }}
                    type="button"
                    class="btn user-input-edit-button"
                    aria-label={`${t('userInput.edit')}: ${opt.label}`}
                    disabled={disabled || state.editing !== null}
                    onClick={() => setState(beginOptionEdit(q, state, i))}
                  >{t('userInput.edit')}</button>
                )}
              </div>
              {state.editing?.option === i && (
                <div class="user-input-option-editor">
                  <input
                    ref={editRef}
                    type="text"
                    class="user-input-text"
                    aria-label={`${t('userInput.edit')}: ${opt.label}`}
                    value={state.editing.text}
                    onInput={(e) => set({ editing: { option: i, text: e.currentTarget.value } })}
                    onKeyDown={(e) => {
                      if (e.isComposing || e.keyCode === 229) return;
                      if (e.key === 'Enter' || e.key === 'Escape') {
                        e.preventDefault();
                        e.stopPropagation();
                        if (e.key === 'Enter') saveEdit();
                        else cancelEdit();
                      }
                    }}
                    disabled={disabled}
                  />
                  <div class="user-input-edit-actions">
                    <button type="button" class="btn btn-primary" disabled={disabled || !state.editing.text.trim()} onClick={saveEdit}>
                      {q.mode === 'single' ? t('userInput.confirmEdit') : t('userInput.saveEdit')}
                    </button>
                    <button type="button" class="btn" disabled={disabled} onClick={cancelEdit}>{t('userInput.cancelEdit')}</button>
                  </div>
                </div>
              )}
            </div>
          ))}
          {showOther && (
            <label class="user-input-option">
              <input
                type={q.mode === 'single' ? 'radio' : 'checkbox'}
                name={`uiq-${nameSuffix}`}
                checked={otherActive}
                onChange={() => q.mode === 'single'
                  ? set({ singleSelected: OTHER_SENTINEL })
                  : set({ otherChecked: !state.otherChecked })}
                disabled={disabled || state.editing !== null}
              />
              <span class="user-input-option-body"><span class="user-input-option-label">{t('userInput.other')}</span></span>
            </label>
          )}
          {otherActive && (
            <input
              ref={freeTextRef}
              type="text"
              class="user-input-text"
              aria-label={t('userInput.other')}
              value={state.freeText}
              onInput={(e) => set({ freeText: e.currentTarget.value })}
              disabled={disabled || state.editing !== null}
              placeholder={t('userInput.placeholder')}
            />
          )}
        </div>
      )}
      {q.mode === 'text' && (
        <div class="field-group">
          <textarea
            data-interaction-autofocus
            autoFocus
            class="user-input-textarea"
            rows={4}
            value={state.freeText}
            onInput={(e) => set({ freeText: e.currentTarget.value })}
            disabled={disabled}
          />
        </div>
      )}
    </>
  );
}

/** Composer-dock chrome shared by the single card and the batch stepper. */
function CardShell({
  title,
  children,
  footer,
  closing,
  onClose,
}: {
  title: string;
  children: preact.ComponentChildren;
  footer: preact.ComponentChildren;
  closing: boolean;
  onClose: () => void;
}) {
  const t = useT();
  return (
    <InteractionDock
      title={title}
      icon={
        <svg width="22" height="22" viewBox="0 0 24 24" fill="none">
          <circle cx="12" cy="12" r="9" stroke="currentColor" stroke-width="1.8" />
          <line x1="12" y1="8" x2="12" y2="13" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" />
          <circle cx="12" cy="16" r="0.9" fill="currentColor" />
        </svg>
      }
      close={{
        disabled: closing,
        onClick: onClose,
        label: t('userInput.close'),
      }}
      footer={footer}
    >
      {children}
    </InteractionDock>
  );
}

function SingleCard({ req, onDone, submitAnswer }: UserInputCardProps) {
  const t = useT();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [state, setState] = useState<AnswerState>(emptyAnswer());
  const q: UserInputQuestion = {
    header: req.header,
    question: req.question,
    mode: req.mode,
    options: req.options,
    custom: req.custom,
  };
  const submitDisabled = loading || !answerReady(q, state);

  async function submit(answer = state) {
    if (loading || !answerReady(q, answer)) return;
    setLoading(true);
    setError(null);
    try {
      const result = await submitAnswer({ request_id: req.request_id, ...buildAnswer(q, answer) });
      if (!result.accepted) throw new Error('user input response was not accepted');
      onDone();
    } catch {
      setLoading(false);
      setError(t('userInput.error'));
    }
  }
  async function skip() {
    if (loading) return;
    setLoading(true);
    setError(null);
    try {
      const result = await submitAnswer({ request_id: req.request_id, ...declinedAnswer() });
      if (!result.accepted) throw new Error('user input response was not accepted');
      onDone();
    } catch {
      setLoading(false);
      setError(t('userInput.error'));
    }
  }

  return (
    <CardShell
      title={q.header}
      closing={loading}
      onClose={() => void skip()}
      footer={
        <>
          <button class="btn" disabled={loading} onClick={skip}>
            {t('userInput.skip')}
          </button>
          <button class="btn btn-primary" disabled={submitDisabled} onClick={() => void submit()}>
            {t('userInput.submit')}
          </button>
        </>
      }
    >
      <QuestionBody q={q} nameSuffix={String(req.request_id)} state={state} setState={setState} disabled={loading} onConfirm={(answer) => void submit(answer)} />
      {error && <p class="user-input-error">{error}</p>}
    </CardShell>
  );
}

function BatchCard({ req, onDone, submitAnswer }: UserInputCardProps) {
  const t = useT();
  const qs = req.questions as UserInputQuestion[];
  const [step, setStep] = useState(0);
  const [drafts, setDrafts] = useState<AnswerState[]>(() => qs.map(emptyAnswer));
  const state = drafts[step];
  const setState = (answer: AnswerState) => setDrafts((all) => all.map((old, i) => i === step ? answer : old));
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const q = qs[step];
  const isLast = step === qs.length - 1;

  async function postAll(all: UserInputResponseBody[]) {
    setLoading(true);
    setError(null);
    try {
      const result = await submitAnswer({ request_id: req.request_id, responses: all });
      if (!result.accepted) throw new Error('user input response was not accepted');
      onDone();
    } catch {
      setLoading(false);
      setError(t('userInput.error'));
    }
  }

  function next(answer = state) {
    if (loading || !answerReady(q, answer)) return;
    const nextDrafts = drafts.map((old, i) => i === step ? answer : old);
    setDrafts(nextDrafts);
    if (isLast) {
      const unfinished = nextDrafts.findIndex((draft) => draft.editing !== null);
      if (unfinished >= 0) setStep(unfinished);
      else void postAll(qs.map((question, i) => buildAnswer(question, nextDrafts[i])));
    } else setStep(step + 1);
  }
  function back() {
    if (!loading && step > 0) setStep(step - 1);
  }
  async function skipAll() {
    if (loading) return;
    setLoading(true);
    setError(null);
    try {
      const result = await submitAnswer({ request_id: req.request_id, responses: qs.map(declinedAnswer) });
      if (!result.accepted) throw new Error('user input response was not accepted');
      onDone();
    } catch {
      setLoading(false);
      setError(t('userInput.error'));
    }
  }

  const nextDisabled = loading || !answerReady(q, state);

  return (
    <CardShell
      title={`${q.header} (${step + 1}/${qs.length})`}
      closing={loading}
      onClose={() => void skipAll()}
      footer={
        <>
          <button class="btn" disabled={loading} onClick={skipAll}>
            {t('userInput.skip')}
          </button>
          {step > 0 && (
            <button class="btn" disabled={loading} onClick={back}>
              ←
            </button>
          )}
          <button class="btn btn-primary" disabled={nextDisabled} onClick={() => next()}>
            {isLast ? t('userInput.submit') : '→'}
          </button>
        </>
      }
    >
      <QuestionBody key={step} q={q} nameSuffix={`${req.request_id}-${step}`} state={state} setState={setState} disabled={loading} onConfirm={next} />
      {error && <p class="user-input-error">{error}</p>}
    </CardShell>
  );
}

export function UserInputCard({ req, onDone, submitAnswer }: UserInputCardProps) {
  const isBatch = isUserInputBatch(req);
  return isBatch
    ? <BatchCard key={`${req.session_id ?? ""}:${req.request_id}`} req={req} onDone={onDone} submitAnswer={submitAnswer} />
    : <SingleCard key={`${req.session_id ?? ""}:${req.request_id}`} req={req} onDone={onDone} submitAnswer={submitAnswer} />;
}
