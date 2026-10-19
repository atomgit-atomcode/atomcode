import assert from 'node:assert/strict';
import test from 'node:test';
import { rankSessionReferences, sessionReferenceToken, sessionReferenceTokens } from './sessionReferences.ts';
import { applyAtMentionSelection, detectAtMentionRange } from './atMention.ts';

const session = (id: string, cwd = '/project', updated = 1, bucket = '0123456789abcdef') => ({ id, name: '类型 修复 [x] @test', project_hash: bucket, working_dir: cwd, created_at: 1, updated_at: updated, message_count: 2 });

test('session references bind bucket/id and sanitize the display label', () => {
  const token = sessionReferenceToken(session('one'));
  assert.equal(token, '@[类型 修复 x test](atomcode-session:0123456789abcdef/one)');
  assert.deepEqual(sessionReferenceTokens(`参考 ${token} 再看看`), [{ start: 3, end: 3 + token.length, label: '类型 修复 x test', bucket: '0123456789abcdef', id: 'one' }]);
  assert.equal(sessionReferenceTokens('email@[a](atomcode-session:0123456789abcdef/one)').length, 0);
  assert.equal(sessionReferenceTokens('@[x](atomcode-session:0123456789abcdef/../secret)').length, 0);
});

test('reference removal preserves surrounding text and multiple references', () => {
  const token = sessionReferenceToken(session('one'));
  const text = `before ${token} after ${sessionReferenceToken(session('two'))}`;
  const ref = sessionReferenceTokens(text)[0];
  const removed = text.slice(0, ref.start) + text.slice(ref.end);
  assert.ok(removed.startsWith('before  after '));
  assert.equal(sessionReferenceTokens(removed)[0].id, 'two');
});

test('project ranking, stable deduplication and current session exclusion', () => {
  const rows = rankSessionReferences([session('other', '/else', 99), session('local'), session('local'), session('current')], '/project', 'current');
  assert.deepEqual(rows.map(s => s.id), ['local', 'other']);
  assert.equal(rankSessionReferences([session('one'), session('one', '/else', 1, 'fedcba9876543210')], '/project', null).length, 2);
});

test('file completion keeps existing replacement and directory contracts', () => {
  const text = 'read @src/old.ts after';
  const range = detectAtMentionRange(text, 10)!;
  assert.equal(applyAtMentionSelection(text, range, 'src/new.ts', false).text, 'read @src/new.ts after');
  assert.equal(applyAtMentionSelection('@sr', detectAtMentionRange('@sr', 3)!, 'src/', true).keepOpen, true);
});

test('only the current bucket/id is excluded when another project has the same id', () => {
  const rows = rankSessionReferences([session('current'), session('current', '/other', 9, 'fedcba9876543210')], '/project', 'current', '0123456789abcdef');
  assert.equal(rows.length, 1);
  assert.equal(rows[0].project_hash, 'fedcba9876543210');
});
