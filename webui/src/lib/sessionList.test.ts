import { test } from 'node:test';
import assert from 'node:assert';
import { mergeOptimisticSession, type SessionLike } from './sessionList.ts';

const s = (over: Partial<SessionLike> = {}): SessionLike => ({
  id: 'id-x',
  name: '你谁呢？跟cc有啥差别？',
  working_dir: '/w',
  ...over,
});

test('optimistic row is suppressed once the real session is in the list even when ids differ (same dir + shared name prefix)', () => {
  // The bug: the /live snapshot realigns the optimistic id to the LIVE session id
  // (capabilities store), but /sessions lists the CORE .json under a DIFFERENT id.
  // id-only dedup then fails, leaving a duplicate sidebar row until a full refresh.
  // Optimistic name = first 10 chars of the message; the persisted auto-name is the
  // fuller first message, so one is a prefix of the other.
  const optimistic = s({ id: 'optimistic-123', name: '你谁呢？跟cc有啥差' });
  const persisted = s({ id: '687a3a48', name: '你谁呢？跟cc有啥差别？' });
  const merged = mergeOptimisticSession(optimistic, [persisted]);
  assert.equal(merged.length, 1, 'duplicate optimistic row must be suppressed');
  assert.equal(merged[0].id, '687a3a48');
});

test('id match still suppresses the optimistic entry (existing behavior preserved)', () => {
  const optimistic = s({ id: 'same' });
  const merged = mergeOptimisticSession(optimistic, [s({ id: 'same' })]);
  assert.equal(merged.length, 1);
});

test('an unrelated session does not suppress the optimistic entry', () => {
  const optimistic = s({ id: 'opt', name: '你谁呢？跟cc有啥差', working_dir: '/w' });
  const other = s({ id: 'other', name: '完全不同的会话', working_dir: '/w' });
  const merged = mergeOptimisticSession(optimistic, [other]);
  assert.equal(merged.length, 2);
  assert.equal(merged[0].id, 'opt', 'optimistic stays pinned to the top');
});

test('shared name prefix but different working_dir does NOT suppress', () => {
  const optimistic = s({ id: 'opt', name: '你谁呢？跟cc有啥差', working_dir: '/a' });
  const persisted = s({ id: 'real', name: '你谁呢？跟cc有啥差别？', working_dir: '/b' });
  const merged = mergeOptimisticSession(optimistic, [persisted]);
  assert.equal(merged.length, 2);
});

test('trailing-slash differences in working_dir do not defeat the match', () => {
  const optimistic = s({ id: 'opt', name: '你谁呢？跟cc有啥差', working_dir: '/w/' });
  const persisted = s({ id: 'real', name: '你谁呢？跟cc有啥差别？', working_dir: '/w' });
  const merged = mergeOptimisticSession(optimistic, [persisted]);
  assert.equal(merged.length, 1);
});

test('a NEW session is not hidden just because its title extends an existing shorter session name in the same dir', () => {
  // Reverse-prefix false hide: the persisted auto-name must extend the optimistic
  // title, not the other way round. Here the optimistic (new) title is the LONGER
  // one, so it is a different session and must still show.
  const existing = s({ id: 'old', name: '你好', working_dir: '/w' });
  const optimistic = s({ id: 'opt', name: '你好啊今天天气怎么样', working_dir: '/w' });
  const merged = mergeOptimisticSession(optimistic, [existing]);
  assert.equal(merged.length, 2, 'reverse-prefix must not hide the new session');
  assert.equal(merged[0].id, 'opt');
});

test('a very short optimistic title falls back to id-only (too weak a signal to suppress)', () => {
  const persisted = s({ id: 'real', name: '在吗，帮我看下', working_dir: '/w' });
  const optimistic = s({ id: 'opt', name: '在吗', working_dir: '/w' });
  const merged = mergeOptimisticSession(optimistic, [persisted]);
  assert.equal(merged.length, 2, 'a 2-char title must not prefix-suppress an unrelated session');
});

test('null optimistic returns the list unchanged', () => {
  const list = [s({ id: 'a' }), s({ id: 'b' })];
  assert.deepEqual(mergeOptimisticSession(null, list), list);
});

test('empty optimistic name never matches by prefix (avoids hiding unrelated sessions)', () => {
  const optimistic = s({ id: 'opt', name: '', working_dir: '/w' });
  const persisted = s({ id: 'real', name: '任意会话', working_dir: '/w' });
  const merged = mergeOptimisticSession(optimistic, [persisted]);
  assert.equal(merged.length, 2, 'an empty name must not prefix-match everything');
});
