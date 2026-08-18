import test from 'node:test';
import assert from 'node:assert/strict';
import { artifactsByAssistantIndex } from './turnArtifacts.ts';

const tool = (name: string, args: unknown, status: 'done' | 'error' = 'done') => ({
  kind: 'tool' as const,
  tool: {
    id: `${name}-${JSON.stringify(args)}`,
    name,
    args: JSON.stringify(args),
    status,
    output: status === 'done' ? 'ok' : 'failed',
  },
});

test('collects only successful mutation paths for a complete assistant turn', () => {
  const result = artifactsByAssistantIndex([
    { role: 'user', parts: [{ kind: 'text', text: 'build it' }] },
    { role: 'assistant', parts: [tool('write_file', { file_path: 'docs/a.md' })] },
    { role: 'system', parts: [{ kind: 'notice', text: 'compacted' }] },
    { role: 'assistant', parts: [
      tool('edit_file', { file_path: 'docs/a.md' }),
      tool('write_file', { file_path: 'site/index.html' }),
      tool('write_file', { file_path: 'failed.txt' }, 'error'),
      tool('read_file', { file_path: 'input.txt' }),
    ] },
  ]);

  assert.deepEqual(result.get(3), [
    { path: 'docs/a.md', label: 'a.md' },
    { path: 'site/index.html', label: 'index.html' },
  ]);
});

test('collects only files actually changed by search_replace', () => {
  const changed = tool('search_replace', { path: 'src', search: 'old', replace: 'new' });
  changed.tool.output = [
    "Replaced 'old' → 'new': 3 replacements across 2 files.",
    '  /repo/src/a.ts (1 replacements)',
    '  C:\\repo\\src\\b.ts (2 replacements)',
  ].join('\n');
  const noMatches = tool('search_replace', { path: 'docs', search: 'old', replace: 'new' });
  noMatches.tool.output = "No matches for 'old' in /repo/docs (4 files scanned).";

  const result = artifactsByAssistantIndex([{
    role: 'assistant',
    parts: [changed, noMatches],
  }]);

  assert.deepEqual(result.get(0), [
    { path: '/repo/src/a.ts', label: 'a.ts' },
    { path: 'C:\\repo\\src\\b.ts', label: 'b.ts' },
  ]);
});

test('collects parallel edits and accepts Windows paths', () => {
  const result = artifactsByAssistantIndex([
    { role: 'assistant', parts: [tool('parallel_edit_files', {
      files: [{ path: 'src/a.ts' }, { path: 'C:\\repo\\b.ts' }],
    })] },
  ]);
  assert.deepEqual(result.get(0), [
    { path: 'src/a.ts', label: 'a.ts' },
    { path: 'C:\\repo\\b.ts', label: 'b.ts' },
  ]);
});

test('does not report a hydrated call that has no authoritative tool result', () => {
  const result = artifactsByAssistantIndex([{
    role: 'assistant',
    parts: [{
      kind: 'tool',
      tool: {
        id: 'orphan',
        name: 'write_file',
        args: JSON.stringify({ file_path: 'not-confirmed.md' }),
        status: 'done',
      },
    }],
  }]);
  assert.deepEqual(result.get(0), []);
});
