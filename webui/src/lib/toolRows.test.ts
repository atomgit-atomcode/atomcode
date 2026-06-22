import { test } from 'node:test';
import assert from 'node:assert';
import { upsertToolPart, type MsgPart, type ToolRow } from './toolRows.ts';

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

test('non-tool parts and ordering are preserved', () => {
  const parts: MsgPart[] = [
    { kind: 'text', text: 'before' },
    { kind: 'tool', tool: tool('a') },
  ];
  const next = upsertToolPart(parts, tool('a', { status: 'done' }));
  assert.equal(next.length, 2);
  assert.deepEqual(next[0], { kind: 'text', text: 'before' });
});
