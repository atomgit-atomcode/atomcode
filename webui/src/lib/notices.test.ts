import { test } from 'node:test';
import assert from 'node:assert';
import { isDuplicateTrailingNotice, type NoticeMessageLike } from './notices.ts';

const notice = (text: string): NoticeMessageLike => ({
  role: 'system',
  parts: [{ kind: 'notice', text }],
});
const user = (text: string): NoticeMessageLike => ({ role: 'user', parts: [{ kind: 'text', text }] });
const assistant = (text: string): NoticeMessageLike => ({
  role: 'assistant',
  parts: [{ kind: 'text', text }],
});

const BLOCKED = '无法确认当前回合已结束，已锁定发送和自动队列。请重试「停止」或切换会话。';

test('a notice identical to the trailing one is a duplicate (blocked-send spam)', () => {
  const msgs = [user('hi'), assistant('hello'), notice(BLOCKED)];
  assert.equal(isDuplicateTrailingNotice(msgs, BLOCKED, false), true);
});

test('an A,B,A,B recovery pair does not restack — A is still in the trailing notice run', () => {
  const A = 'no confirm cancel';
  const B = 'no confirm active';
  const msgs = [user('hi'), notice(A), notice(B)];
  // Pushing A again: the trailing run is [A, B], A is present → duplicate.
  assert.equal(isDuplicateTrailingNotice(msgs, A, false), true);
  assert.equal(isDuplicateTrailingNotice(msgs, B, false), true);
});

test('the first notice (empty trailing run) is not a duplicate', () => {
  assert.equal(isDuplicateTrailingNotice([user('hi'), assistant('hello')], BLOCKED, false), false);
  assert.equal(isDuplicateTrailingNotice([], BLOCKED, false), false);
});

test('a different notice text is not suppressed', () => {
  const msgs = [notice('something else')];
  assert.equal(isDuplicateTrailingNotice(msgs, BLOCKED, false), false);
});

test('real content after a notice resets the run so a genuine later repeat shows', () => {
  const msgs = [notice(BLOCKED), user('别锁了'), assistant('ok')];
  // The trailing run is empty (last two are non-notice) → not a duplicate.
  assert.equal(isDuplicateTrailingNotice(msgs, BLOCKED, false), false);
});

test('insertBeforeBusyAssistant scans the notices just before the streaming tail', () => {
  // While busy, the streaming assistant stays last and notices are inserted before it.
  const msgs = [user('hi'), notice(BLOCKED), assistant('streaming…')];
  assert.equal(isDuplicateTrailingNotice(msgs, BLOCKED, true), true);
  // Without the busy flag the assistant tail ends the run, so it is NOT seen as dup.
  assert.equal(isDuplicateTrailingNotice(msgs, BLOCKED, false), false);
});
