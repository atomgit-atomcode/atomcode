import test from 'node:test';
import assert from 'node:assert/strict';
import { emptyAnswer, beginOptionEdit, saveOptionEdit, cancelOptionEdit, buildAnswer, answerReady } from './userInput.ts';
import type { AnswerState } from './userInput.ts';
import type { UserInputQuestion } from '../api.ts';

const question = (mode: UserInputQuestion['mode'] = 'multiple'): UserInputQuestion => ({
  header: 'Plan', question: 'Which?', mode,
  options: [{ label: '用方案 A', description: '先验证' }, { label: '用方案 B' }, { label: 'C' }],
});
function edited(q: UserInputQuestion, state: AnswerState, option: number, text: string): AnswerState {
  const editing = beginOptionEdit(q, state, option);
  return saveOptionEdit(q, { ...editing, editing: { option, text } });
}

test('single edit starts from the label and submits only custom text', () => {
  const q = question('single');
  const original = structuredClone(q);
  const s = beginOptionEdit(q, emptyAnswer(), 0);
  assert.equal(s.editing?.text, '用方案 A');
  assert.equal(answerReady(q, s), false);
  const saved = saveOptionEdit(q, { ...s, editing: { option: 0, text: '用方案 A，但先只做第 1 步' } });
  assert.deepEqual(buildAnswer(q, saved), { declined: false, selected: [], text: '用方案 A，但先只做第 1 步' });
  assert.deepEqual(q, original);
});

test('several edited picks replace the originals without losing other picks or free text', () => {
  const q = question();
  let s = { ...emptyAnswer(), multiSelected: new Set([0, 1, 2]), otherChecked: true, freeText: '补充说明' };
  s = edited(q, s, 0, 'A 第一阶段');
  s = edited(q, s, 1, 'B 第二阶段');
  assert.deepEqual(buildAnswer(q, s), { declined: false, selected: ['C'], text: 'A 第一阶段\nB 第二阶段\n补充说明' });
  s = { ...s, multiSelected: new Set([1, 2]) };
  assert.deepEqual(buildAnswer(q, s), { declined: false, selected: ['C'], text: 'B 第二阶段\n补充说明' });
  assert.equal(beginOptionEdit(q, s, 0).editing?.text, 'A 第一阶段');
});

test('cancel restores selection and saved replacement; empty edits cannot be saved', () => {
  const q = question();
  const saved = edited(q, emptyAnswer(), 0, 'A 修改版');
  const s = beginOptionEdit(q, saved, 0);
  const blank = { ...s, editing: { option: 0, text: '  ' } };
  assert.equal(saveOptionEdit(q, blank), blank);
  assert.equal(answerReady(q, blank), false);
  assert.deepEqual(cancelOptionEdit(blank), saved);
  assert.deepEqual(cancelOptionEdit(beginOptionEdit(q, emptyAnswer(), 1)), emptyAnswer());
});

test('batch drafts remain independent, including duplicate labels and uncommitted edits', () => {
  const q = question();
  q.options[1].label = q.options[0].label;
  const drafts = [edited(q, emptyAnswer(), 0, '第一项'), edited(q, emptyAnswer(), 1, '第二项')];
  drafts[0] = beginOptionEdit(q, drafts[0], 1);
  assert.equal(answerReady(q, drafts[0]), false);
  assert.deepEqual(buildAnswer(q, drafts[1]), { declined: false, selected: [], text: '第二项' });
  drafts[0] = cancelOptionEdit(drafts[0]);
  assert.deepEqual(buildAnswer(q, drafts[0]), { declined: false, selected: [], text: '第一项' });
});

test('text-only and custom-disabled questions do not gain option editing', () => {
  const s = emptyAnswer();
  assert.equal(beginOptionEdit(question('text'), s, 0), s);
  assert.equal(beginOptionEdit({ ...question(), custom: false }, s, 0), s);
});

test('multiline labels become visible single-line words before editing and submission', () => {
  const q = question('single');
  q.options[0].label = 'A\nfirst\rstep\t1';
  const s = beginOptionEdit(q, emptyAnswer(), 0);
  assert.equal(s.editing?.text, 'A first step 1');
  assert.equal(buildAnswer(q, saveOptionEdit(q, s)).text, 'A first step 1');
  assert.equal(q.options[0].label, 'A\nfirst\rstep\t1');
});
