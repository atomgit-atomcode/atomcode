import type { UserInputQuestion, UserInputResponseBody } from '../api';

export const OTHER_SENTINEL = 'other';

/** Request-local drafts use option indexes so duplicate labels remain independent. */
export interface AnswerState {
  singleSelected: number | typeof OTHER_SENTINEL | null;
  multiSelected: Set<number>;
  otherChecked: boolean;
  freeText: string;
  replacements: Record<number, string>;
  editing: { option: number; text: string } | null;
}

export const emptyAnswer = (): AnswerState => ({
  singleSelected: null,
  multiSelected: new Set(),
  otherChecked: false,
  freeText: '',
  replacements: {},
  editing: null,
});

export const declinedAnswer = (): UserInputResponseBody => ({ declined: true, selected: [], text: null });

export function beginOptionEdit(q: UserInputQuestion, s: AnswerState, option: number): AnswerState {
  if (s.editing || q.mode === 'text' || q.custom === false || !q.options[option]) return s;
  // Match the TUI's single-line field; the DOM otherwise silently drops line breaks.
  const text = (s.replacements[option] ?? q.options[option].label).replace(/\p{Cc}/gu, ' ');
  return { ...s, editing: { option, text } };
}

export function saveOptionEdit(q: UserInputQuestion, s: AnswerState): AnswerState {
  if (!s.editing || !s.editing.text.trim()) return s;
  const { option, text } = s.editing;
  return {
    ...s,
    replacements: { ...s.replacements, [option]: text.trim() },
    singleSelected: q.mode === 'single' ? option : s.singleSelected,
    multiSelected: q.mode === 'multiple' ? new Set([...s.multiSelected, option]) : s.multiSelected,
    editing: null,
  };
}

export function cancelOptionEdit(s: AnswerState): AnswerState {
  return { ...s, editing: null };
}

export function answerReady(q: UserInputQuestion, s: AnswerState): boolean {
  if (s.editing) return false;
  if (q.mode === 'text') return s.freeText.trim() !== '';
  if (q.mode === 'single') {
    return s.singleSelected === OTHER_SENTINEL
      ? s.freeText.trim() !== ''
      : s.singleSelected !== null;
  }
  return s.multiSelected.size > 0 || (s.otherChecked && s.freeText.trim() !== '');
}

/** Keep edited answers as custom text; never claim the original option was picked. */
export function buildAnswer(q: UserInputQuestion, s: AnswerState): UserInputResponseBody {
  if (!answerReady(q, s)) return declinedAnswer();
  if (q.mode === 'text') return { declined: false, selected: [], text: s.freeText };
  const selected: string[] = [];
  const custom: string[] = [];
  q.options.forEach((option, i) => {
    const picked = q.mode === 'single' ? s.singleSelected === i : s.multiSelected.has(i);
    if (!picked) return;
    if (s.replacements[i] !== undefined) custom.push(s.replacements[i]);
    else selected.push(option.label);
  });
  if ((q.mode === 'single' ? s.singleSelected === OTHER_SENTINEL : s.otherChecked) && s.freeText.trim()) {
    custom.push(s.freeText.trim());
  }
  return { declined: false, selected, text: custom.length ? custom.join('\n') : null };
}
