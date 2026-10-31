import test from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';

const source = readFileSync('src/components/SourceControl.tsx', 'utf8');

test('source control separates conflicts and mixed staged/worktree changes', () => {
  assert.match(source, /changes\.filter\(\(change\) => change\.conflicted\)/);
  assert.match(source, /changes\.filter\(\(change\) => change\.unstaged\)/);
  assert.match(source, /conflicts\.length > 0/);
  assert.match(source, /gitStage\(unstaged\.map/);
});

test('source control loads inline diffs and represents binary and truncation', () => {
  assert.match(source, /gitDiff\(path, kind\)/);
  assert.match(source, /result\.binary/);
  assert.match(source, /result\.truncated/);
  assert.match(source, /result\.patch/);
  assert.match(source, /kind: GitDiffKind/);
});
