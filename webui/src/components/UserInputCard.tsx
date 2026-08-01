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

interface UserInputCardProps {
  req: UserInputRequestEvent;
  onDone: () => void;
  submitAnswer: (body: UserInputAnswer) => Promise<{ accepted: boolean }>;
}

const OTHER_SENTINEL = '__other__';

/** Local editing state for one question. */
interface AnswerState {
  singleSelected: string | null;
  multiSelected: Set<string>;
  otherChecked: boolean;
  freeText: string;
}
const emptyAnswer = (): AnswerState => ({
  singleSelected: null,
  multiSelected: new Set(),
  otherChecked: false,
  freeText: '',
});

/** Build the wire answer for a question from its local state (pure). */
function buildAnswer(q: UserInputQuestion, s: AnswerState): UserInputResponseBody {
  if (q.mode === 'text') {
    return { declined: false, selected: [], text: s.freeText || null };
  }
  if (q.mode === 'single') {
    const chosen =
      s.singleSelected === OTHER_SENTINEL
        ? [s.freeText.trim()]
        : s.singleSelected
          ? [s.singleSelected]
          : [];
    return { declined: false, selected: chosen, text: null };
  }
  const chosen = Array.from(s.multiSelected);
  if (s.otherChecked && s.freeText.trim()) chosen.push(s.freeText.trim());
  return { declined: false, selected: chosen, text: null };
}

/** Whether the current state is a submittable answer (pure). */
function answerReady(q: UserInputQuestion, s: AnswerState): boolean {
  if (q.mode === 'text') return s.freeText.trim() !== '';
  if (q.mode === 'single') {
    if (s.singleSelected === null) return false;
    if (s.singleSelected === OTHER_SENTINEL) return s.freeText.trim() !== '';
    return true;
  }
  return s.multiSelected.size > 0 || (s.otherChecked && s.freeText.trim() !== '');
}

/** Re-derive editing state from a previously-committed answer (for the Back button). */
function hydrate(q: UserInputQuestion, a: UserInputResponseBody): AnswerState {
  const s = emptyAnswer();
  if (a.declined) return s;
  if (q.mode === 'text') {
    s.freeText = a.text ?? '';
    return s;
  }
  if (q.mode === 'single') {
    const sel = a.selected[0];
    if (sel === undefined) return s;
    if (q.options.some((o) => o.label === sel)) s.singleSelected = sel;
    else {
      s.singleSelected = OTHER_SENTINEL;
      s.freeText = sel;
    }
    return s;
  }
  const known = new Set(q.options.map((o) => o.label));
  a.selected.forEach((l) => {
    if (known.has(l)) s.multiSelected.add(l);
    else {
      s.otherChecked = true;
      s.freeText = l;
    }
  });
  return s;
}

const declinedAnswer = (): UserInputResponseBody => ({ declined: true, selected: [], text: null });

/** The options/text body for one question. Reused by the single card and the batch stepper. */
function QuestionBody({
  q,
  nameSuffix,
  state,
  setState,
  disabled,
}: {
  q: UserInputQuestion;
  nameSuffix: string;
  state: AnswerState;
  setState: (s: AnswerState) => void;
  disabled: boolean;
}) {
  const t = useT();
  const freeTextRef = useRef<HTMLInputElement>(null);
  // Offer the "type your own answer" row unless the question opts out (custom === false).
  const showOther = q.custom !== false;
  const singleOtherActive = q.mode === 'single' && state.singleSelected === OTHER_SENTINEL;
  const multiOtherActive = q.mode === 'multiple' && state.otherChecked;
  useEffect(() => {
    if ((singleOtherActive || multiOtherActive) && freeTextRef.current) freeTextRef.current.focus();
  }, [singleOtherActive, multiOtherActive]);

  const set = (patch: Partial<AnswerState>) => setState({ ...state, ...patch });
  const toggleMulti = (label: string) => {
    const next = new Set(state.multiSelected);
    if (next.has(label)) next.delete(label);
    else next.add(label);
    set({ multiSelected: next });
  };

  return (
    <>
      <p class="permission-lead">{q.question}</p>

      {q.mode === 'single' && q.options.length > 0 && (
        <div class="field-group">
          {q.options.map((opt) => (
            <label key={opt.label} class="user-input-option">
              <input
                type="radio"
                name={`uiq-${nameSuffix}`}
                value={opt.label}
                checked={state.singleSelected === opt.label}
                onChange={() => set({ singleSelected: opt.label })}
                disabled={disabled}
              />
              <span class="user-input-option-body">
                <span class="user-input-option-label">{opt.label}</span>
                {opt.description && <span class="user-input-option-desc">{opt.description}</span>}
              </span>
            </label>
          ))}
          {showOther && (
            <label class="user-input-option">
              <input
                type="radio"
                name={`uiq-${nameSuffix}`}
                value={OTHER_SENTINEL}
                checked={state.singleSelected === OTHER_SENTINEL}
                onChange={() => set({ singleSelected: OTHER_SENTINEL })}
                disabled={disabled}
              />
              <span class="user-input-option-body">
                <span class="user-input-option-label">{t('userInput.other')}</span>
              </span>
            </label>
          )}
          {singleOtherActive && (
            <input
              ref={freeTextRef}
              type="text"
              class="user-input-text"
              value={state.freeText}
              onInput={(e) => set({ freeText: (e.target as HTMLInputElement).value })}
              disabled={disabled}
              placeholder="输入自己的答案…"
            />
          )}
        </div>
      )}

      {q.mode === 'multiple' && q.options.length > 0 && (
        <div class="field-group">
          {q.options.map((opt) => (
            <label key={opt.label} class="user-input-option">
              <input
                type="checkbox"
                value={opt.label}
                checked={state.multiSelected.has(opt.label)}
                onChange={() => toggleMulti(opt.label)}
                disabled={disabled}
              />
              <span class="user-input-option-body">
                <span class="user-input-option-label">{opt.label}</span>
                {opt.description && <span class="user-input-option-desc">{opt.description}</span>}
              </span>
            </label>
          ))}
          {showOther && (
            <label class="user-input-option">
              <input
                type="checkbox"
                checked={state.otherChecked}
                onChange={() => set({ otherChecked: !state.otherChecked })}
                disabled={disabled}
              />
              <span class="user-input-option-body">
                <span class="user-input-option-label">{t('userInput.other')}</span>
              </span>
            </label>
          )}
          {multiOtherActive && (
            <input
              ref={freeTextRef}
              type="text"
              class="user-input-text"
              value={state.freeText}
              onInput={(e) => set({ freeText: (e.target as HTMLInputElement).value })}
              disabled={disabled}
              placeholder="输入自己的答案…"
            />
          )}
        </div>
      )}

      {q.mode === 'text' && (
        <div class="field-group">
          <textarea
            class="user-input-textarea"
            rows={4}
            value={state.freeText}
            onInput={(e) => set({ freeText: (e.target as HTMLTextAreaElement).value })}
            disabled={disabled}
            placeholder=""
          />
        </div>
      )}
    </>
  );
}

/** Modal chrome shared by the single card and the batch stepper. */
function CardShell({
  title,
  children,
  footer,
}: {
  title: string;
  children: preact.ComponentChildren;
  footer: preact.ComponentChildren;
}) {
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
          <h3 class="permission-title">{title}</h3>
        </div>
        <div class="modal-body">{children}</div>
        <div class="modal-footer permission-footer">{footer}</div>
      </div>
    </div>
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

  async function submit() {
    if (submitDisabled) return;
    setLoading(true);
    setError(null);
    try {
      await submitAnswer({ request_id: req.request_id, ...buildAnswer(q, state) });
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
      await submitAnswer({ request_id: req.request_id, ...declinedAnswer() });
      onDone();
    } catch {
      setLoading(false);
      setError(t('userInput.error'));
    }
  }

  return (
    <CardShell
      title={q.header}
      footer={
        <>
          <button class="btn" disabled={loading} onClick={skip}>
            {t('userInput.skip')}
          </button>
          <button class="btn btn-primary" disabled={submitDisabled} onClick={submit}>
            {t('userInput.submit')}
          </button>
        </>
      }
    >
      <QuestionBody q={q} nameSuffix={String(req.request_id)} state={state} setState={setState} disabled={loading} />
      {error && <p class="user-input-error">{error}</p>}
    </CardShell>
  );
}

function BatchCard({ req, onDone, submitAnswer }: UserInputCardProps) {
  const t = useT();
  const qs = req.questions as UserInputQuestion[];
  const [step, setStep] = useState(0);
  const [answers, setAnswers] = useState<UserInputResponseBody[]>(qs.map(declinedAnswer));
  const [state, setState] = useState<AnswerState>(emptyAnswer());
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const q = qs[step];
  const isLast = step === qs.length - 1;

  // Commit the current step's edits into the accumulated answers, returning the new array.
  function commitCurrent(): UserInputResponseBody[] {
    const a = answerReady(q, state) ? buildAnswer(q, state) : declinedAnswer();
    const next = answers.slice();
    next[step] = a;
    setAnswers(next);
    return next;
  }

  function goToStep(committed: UserInputResponseBody[], target: number) {
    setStep(target);
    setState(hydrate(qs[target], committed[target]));
  }

  async function postAll(all: UserInputResponseBody[]) {
    setLoading(true);
    setError(null);
    try {
      await submitAnswer({ request_id: req.request_id, responses: all });
      onDone();
    } catch {
      setLoading(false);
      setError(t('userInput.error'));
    }
  }

  function next() {
    if (loading) return;
    const committed = commitCurrent();
    if (isLast) void postAll(committed);
    else goToStep(committed, step + 1);
  }
  function back() {
    if (loading || step === 0) return;
    const committed = commitCurrent();
    goToStep(committed, step - 1);
  }
  async function skipAll() {
    if (loading) return;
    setLoading(true);
    setError(null);
    try {
      await submitAnswer({ request_id: req.request_id, responses: qs.map(declinedAnswer) });
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
          <button class="btn btn-primary" disabled={nextDisabled} onClick={next}>
            {isLast ? t('userInput.submit') : '→'}
          </button>
        </>
      }
    >
      <QuestionBody q={q} nameSuffix={`${req.request_id}-${step}`} state={state} setState={setState} disabled={loading} />
      {error && <p class="user-input-error">{error}</p>}
    </CardShell>
  );
}

export function UserInputCard({ req, onDone, submitAnswer }: UserInputCardProps) {
  const isBatch = isUserInputBatch(req);
  return isBatch
    ? <BatchCard req={req} onDone={onDone} submitAnswer={submitAnswer} />
    : <SingleCard req={req} onDone={onDone} submitAnswer={submitAnswer} />;
}
