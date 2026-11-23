import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { GitFileChange } from '../api.ts';
import {
  commitMessageProblem,
  commitSubject,
  entryOf,
  groupByDirectory,
  isConflicted,
  partitionChanges,
  relativeTime,
  selectionsOf,
  splitPath,
  statusLetter,
  sumStats,
} from './gitPanel.ts';

const change = (over: Partial<GitFileChange>): GitFileChange => ({
  path: 'a.txt',
  index: ' ',
  worktree: 'M',
  ...over,
});

test('a file changed on both sides appears in both source-control sections', () => {
  const { staged, unstaged } = partitionChanges([
    change({ path: 'both.txt', index: 'M', worktree: 'M' }),
    change({ path: 'staged.txt', index: 'A', worktree: ' ' }),
    change({ path: 'worktree.txt', index: ' ', worktree: 'M' }),
    change({ path: 'new.txt', index: '?', worktree: '?' }),
  ]);
  assert.deepEqual(staged.map((e) => e.path), ['both.txt', 'staged.txt']);
  assert.deepEqual(unstaged.map((e) => e.path), ['both.txt', 'worktree.txt', 'new.txt']);
  assert.equal(staged[1].kind, 'added');
  assert.equal(unstaged[2].kind, 'untracked');
});

test('a conflict is listed once, on the unstaged side, whatever the letters', () => {
  for (const letters of [
    { index: 'U', worktree: 'U' },
    { index: 'A', worktree: 'A' },
    { index: 'D', worktree: 'D' },
    { index: 'U', worktree: ' ' },
  ]) {
    const row = change({ path: 'clash.txt', ...letters });
    assert.ok(isConflicted(row), JSON.stringify(letters));
    const { staged, unstaged } = partitionChanges([row]);
    assert.equal(staged.length, 0);
    assert.equal(unstaged.length, 1);
    assert.equal(unstaged[0].kind, 'conflicted');
    assert.equal(statusLetter(unstaged[0].kind), '!');
  }
});

test('rename rows keep the original path so a Git action can address both sides', () => {
  const entry = entryOf(
    change({ path: 'src/new name.txt', original: 'src/old name.txt', index: 'R', worktree: ' ' }),
    'index',
  );
  assert.equal(entry.kind, 'renamed');
  assert.deepEqual(splitPath(entry.path), { dir: 'src', name: 'new name.txt' });
  assert.deepEqual(selectionsOf([entry]), [
    { path: 'src/new name.txt', original: 'src/old name.txt', untracked: false },
  ]);
});

test('untracked rows are marked so discarding removes the file', () => {
  const { unstaged } = partitionChanges([change({ path: 'draft.md', index: '?', worktree: '?' })]);
  assert.deepEqual(selectionsOf(unstaged), [
    { path: 'draft.md', original: null, untracked: true },
  ]);
});

test('groups follow the first directory segment with root files last', () => {
  const entries = [
    entryOf(change({ path: 'README.md' }), 'worktree'),
    entryOf(change({ path: 'webui/src/a.ts' }), 'worktree'),
    entryOf(change({ path: 'crates/b/src/lib.rs' }), 'worktree'),
    entryOf(change({ path: 'webui/package.json' }), 'worktree'),
  ];
  const groups = groupByDirectory(entries);
  assert.deepEqual(groups.map((g) => g.id), ['crates', 'webui', '']);
  assert.deepEqual(groups[1].entries.map((e) => e.path), ['webui/src/a.ts', 'webui/package.json']);
  assert.equal(groups[2].label, '.');
});

test('line stats add up and missing stats count as zero', () => {
  const entries = [
    entryOf(change({ stats: { additions: 3, deletions: 1 } }), 'worktree'),
    entryOf(change({ path: 'b.txt', stats: null }), 'worktree'),
    entryOf(change({ path: 'c.txt', stats: { additions: 2, deletions: 5 } }), 'worktree'),
  ];
  assert.deepEqual(sumStats(entries), { additions: 5, deletions: 6 });
  assert.deepEqual(sumStats([]), { additions: 0, deletions: 0 });
});

test('the commit button knows why a message is not committable', () => {
  assert.equal(commitMessageProblem('   '), 'empty');
  assert.equal(commitMessageProblem('fix: something'), null);
  assert.equal(commitMessageProblem('x'.repeat(4000)), null);
  assert.equal(commitMessageProblem('x'.repeat(4001)), 'too-long');
  // Counted as characters, not UTF-16 units: a CJK message keeps its budget.
  assert.equal(commitMessageProblem('中'.repeat(4000)), null);
});

test('relative history time stays readable and dates far commits', () => {
  const now = 1_700_000_000;
  assert.equal(relativeTime(now - 10, now, true), '刚刚');
  assert.equal(relativeTime(now - 10, now, false), 'just now');
  assert.equal(relativeTime(now - 5 * 60, now, true), '5 分钟前');
  assert.equal(relativeTime(now - 3 * 3600, now, false), '3 h ago');
  assert.equal(relativeTime(now - 2 * 86400, now, true), '2 天前');
  assert.match(relativeTime(now - 400 * 86400, now, false), /^\d{4}-\d{2}-\d{2}$/);
});

test('a commit subject stays one line', () => {
  assert.equal(commitSubject('  fix: panel  \nsecond line'), 'fix: panel');
  assert.equal(commitSubject(''), '(no message)');
});
