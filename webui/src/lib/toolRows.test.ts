import { test } from 'node:test';
import assert from 'node:assert';
import { settlePendingToolParts, toolResultStatus, updateToolProgress, upsertToolPart, type MsgPart, type ToolRow } from './toolRows.ts';

const tool = (id: string, over: Partial<ToolRow> = {}): ToolRow => ({
  id,
  name: 'grep',
  args: '"foo"',
  status: 'pending',
  ...over,
});

test('a new tool id is appended as a tool part', () => {
  const parts: MsgPart[] = [{ kind: 'text', text: 'hi' }];
  const next = upsertToolPart(parts, tool('a'));
  assert.equal(next.length, 2);
  assert.deepEqual(next[1], { kind: 'tool', tool: tool('a') });
});

test('replaying the SAME tool id updates in place — no duplicate row', () => {
  // The webui bug: a duplicate /live subscription re-delivers the same
  // tool_start, which must NOT create a second row.
  const parts: MsgPart[] = [{ kind: 'tool', tool: tool('a') }];
  const again = upsertToolPart(parts, tool('a', { status: 'done', duration_ms: 12 }));
  assert.equal(again.length, 1, 'same id must not append a second row');
  assert.equal(again[0].kind, 'tool');
  const t = (again[0] as { kind: 'tool'; tool: ToolRow }).tool;
  assert.equal(t.status, 'done');
  assert.equal(t.duration_ms, 12);
  assert.equal(t.name, 'grep'); // prior fields preserved via merge
});

test('distinct tool ids each keep their own row (genuine repeat calls)', () => {
  // Different call_ids = different logical calls (e.g. across goal rounds) and
  // must stay as separate rows — dedup is by id, never by name+args.
  let parts: MsgPart[] = [];
  parts = upsertToolPart(parts, tool('a'));
  parts = upsertToolPart(parts, tool('b'));
  assert.equal(parts.length, 2);
});

test('upsertToolPart replaces live progress instead of accumulating it', () => {
  const first = updateToolProgress([{ kind: 'tool', tool: tool('a') }], 'a', 'round 1 · thinking');
  const second = updateToolProgress(first, 'a', 'round 2 · read_file');
  const current = second[0].kind === 'tool' ? second[0].tool : undefined;

  assert.equal(current?.progress, 'round 2 · read_file');
});

test('non-tool parts and ordering are preserved', () => {
  const parts: MsgPart[] = [
    { kind: 'text', text: 'before' },
    { kind: 'tool', tool: tool('a') },
  ];
  const next = upsertToolPart(parts, tool('a', { status: 'done' }));
  assert.equal(next.length, 2);
  assert.deepEqual(next[0], { kind: 'text', text: 'before' });
});

test('partial review result is incomplete rather than a clean success or generic failure', () => {
  assert.equal(toolResultStatus(false, 'Code review incomplete (MaxRounds)'), 'incomplete');
});

test('search progress updates only its pending card and cannot revive a finished call', () => {
  let parts: MsgPart[] = [{ kind: 'tool', tool: tool('search') }, { kind: 'tool', tool: tool('other') }];
  parts = updateToolProgress(parts, 'search', 'Searching · 1s elapsed · linked');
  assert.equal(parts[0].kind === 'tool' && parts[0].tool.progress, 'Searching · 1s elapsed · linked');
  assert.equal(parts[1].kind === 'tool' && parts[1].tool.progress, undefined);
  parts = upsertToolPart(parts, tool('search', { status: 'done', output: 'No matches' }));
  parts = updateToolProgress(parts, 'search', 'late search activity');
  assert.equal(parts[0].kind === 'tool' && parts[0].tool.progress, undefined);
  assert.equal(parts[0].kind === 'tool' && parts[0].tool.status, 'done');
});


test('turn termination settles only open tools and discards their activity', () => {
  const parts: MsgPart[] = [
    { kind: 'text', text: 'partial reply' },
    { kind: 'tool', tool: tool('open', { progress: 'Searching', output: 'partial output' }) },
    { kind: 'tool', tool: tool('approved', { status: 'waiting_approval' }) },
    { kind: 'tool', tool: tool('done', { status: 'done', output: 'result' }) },
  ];
  const settled = settlePendingToolParts(parts);
  assert.deepEqual(settled[0], parts[0]);
  for (const index of [1, 2]) {
    assert.equal(settled[index].kind === 'tool' && settled[index].tool.status, 'incomplete');
    assert.equal(settled[index].kind === 'tool' && settled[index].tool.progress, undefined);
  }
  assert.equal(settled[1].kind === 'tool' && settled[1].tool.output, 'partial output');
  assert.deepEqual(settled[3], parts[3]);
  assert.deepEqual(updateToolProgress(settled, 'open', 'late progress'), settled);
});
