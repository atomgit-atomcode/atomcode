import test from 'node:test';
import assert from 'node:assert/strict';
import { historyBoundary, historyMessages, historyWindow } from './inlineHistory.ts';
import type { TranscriptTurn } from '../api.ts';
const turn = (id: number, user = 'repeat'): TranscriptTurn => ({ turn_id: id, user, assistant: 'reply', undone: false, started_at: id * 1000, ts: id * 1000 + 500 });
const user = (text: string, ts?: number, retainedHistory = false) => ({ role: 'user' as const, ts, retainedHistory, parts: [{ kind: 'text' as const, text }] });

test('upward loading selects only the six most recent older records', () => {
  assert.deepEqual(historyWindow(76), { start: 70, end: 76 });
  assert.deepEqual(historyWindow(0), { start: 0, end: 0 });
});
test('first-turn navigation loads the first window directly without intermediate detail', () => {
  assert.deepEqual(historyWindow(76, 0), { start: 0, end: 6 });
  assert.deepEqual(historyWindow(76, 30), { start: 30, end: 36 });
});
test('timestamps distinguish identical prompts and skip retained compaction context', () => {
  assert.deepEqual(historyBoundary([turn(1), turn(2), turn(3)], [user('repeat', 1000, true), user('repeat', 3000)]),
    { end: 2, liveStart: 1, joined: true });
});
test('ambiguous old records never join the wrong repeated prompt', () => {
  const records = [turn(1), turn(2)].map(t => ({ ...t, started_at: undefined, ts: undefined }));
  assert.equal(historyBoundary(records, [user('repeat')]).joined, false);
  assert.equal(historyBoundary(records, [user('unknown')]).joined, false);
  assert.deepEqual(historyBoundary([{ ...turn(1), user: 'unique' }], [user('unique')]), { end: 0, liveStart: 0, joined: true });
});
test('an unfinished current turn can join after the last completed record', () => {
  assert.deepEqual(historyBoundary([turn(1)], [user('new', 2000)]), { end: 1, liveStart: 0, joined: true });
});
test('undone identity is never mistaken for a current turn', () => {
  assert.equal(historyBoundary([{ ...turn(1), undone: true }], [user('repeat', 1000)]).joined, false);
});
test('normal message conversion preserves interleaved assistant rounds and actual tool outcomes', () => {
  const record = { ...turn(1), steps: [
    { text: 'before', reasoning: 'thinking', tools: [{ id: 'x', name: 'Bash', args: '{}', result: null, is_error: null }] },
    { text: 'after', reasoning: '', tools: [{ id: 'y', name: 'ReadFile', args: '{}', result: 'no', is_error: true }] },
  ] };
  const [input, assistant] = historyMessages(record);
  assert.equal(input.ts, 1000); assert.equal(assistant.ts, 1500);
  assert.deepEqual(assistant.parts.map(p => p.kind === 'tool' ? [p.tool.id, p.tool.status] : p.kind === 'text' ? p.text : p.kind),
    ['before', ['x', 'incomplete'], 'after', ['y', 'error']]);
  assert.deepEqual(historyMessages({ ...turn(1), steps: [] })[1].parts, [], 'do not fabricate merged text for an explicitly empty projection');
});

test('a truncated index preview cannot identify a full legacy prompt', () => {
  assert.equal(historyBoundary([{ ...turn(1), started_at: undefined, user_truncated: true }], [user('repeat')]).joined, false);
});
