import { test } from 'node:test';
import assert from 'node:assert/strict';
import { groupedTimelinePage } from './groupedTimeline.ts';
import { processSummary, turnProcesses, type ProcessMessage } from './turnProcess.ts';
import { archivedMessage, segmentMessages } from './timelineMemory.ts';

const fixture = () => segmentMessages<ProcessMessage>([
  { role: 'user', ts: 1, parts: [{ kind: 'text', text: 'request' }] },
  { role: 'assistant', ts: 2, parts: [...Array.from({ length: 1000 }, (_, index) => ({ kind: 'tool' as const,
    tool: { id: String(index), name: 'read', args: '{}', status: 'done' as const } })), { kind: 'text', text: 'complete final answer' }] },
]);

test('a huge completed turn loads as one collapsed group at either raw page boundary', () => {
  const messages = fixture(), processes = turnProcesses(messages);
  for (const start of [null, 0, 40]) {
    const page = groupedTimelinePage(messages, 12, start, 1024 * 1024, processes, () => false);
    assert.deepEqual(page.range, { start: 0, end: messages.length });
    assert.deepEqual(page.indexes, [0, messages.length - 1]);
  }
});
test('archive metadata keeps a completed group foldable without hydrating its process', () => {
  const messages = fixture().map((message, index) => ({ ...archivedMessage(message, String(index)), processSummary: processSummary(message) }));
  const processes = turnProcesses(messages);
  assert.equal(processes.get(1)!.foldable, true);
  assert.equal(processes.get(1)!.tools, 1000);
  assert.deepEqual(processes.get(1)!.finalParts, [], 'preview text is never rendered as a final answer');
  assert.deepEqual(groupedTimelinePage(messages, 12, 40, 1024 * 1024, processes, () => false).indexes, [0, messages.length - 1]);
});
test('expanded huge processes remain bounded, pin the prompt and answer, and page forward', () => {
  const messages = fixture(), processes = turnProcesses(messages);
  const page = groupedTimelinePage(messages, 12, 40, 1024 * 1024, processes, () => true);
  assert.deepEqual(page.range, { start: 40, end: 52 });
  assert.equal(page.indexes.length, 14);
  assert.equal(page.indexes[0], 0);
  assert.equal(page.indexes[13], messages.length - 1);
  assert.equal(page.processIndexes.has(messages.length - 1), false);
});
test('active or failed processes are not hidden, and simple user-answer pairs stay together', () => {
  const messages = fixture();
  const page = groupedTimelinePage(messages, 12, null, 1024 * 1024, turnProcesses(messages, true), () => false);
  assert.ok(page.indexes.length > 2);
  const simple: ProcessMessage[] = [{ role: 'user', parts: [] }, { role: 'assistant', parts: [{ kind: 'text', text: 'answer' }] }];
  assert.deepEqual(groupedTimelinePage(simple, 1, null, 1024, turnProcesses(simple), () => false).indexes, [0, 1]);
});


test('retained user history and system notices stay visible inside a completed group', () => {
  const messages = fixture();
  messages.splice(2, 0, { role: 'user', retainedHistory: true, parts: [{ kind: 'text', text: 'retained specification' }] },
    { role: 'system', parts: [{ kind: 'notice', text: 'important notice' }] });
  const page = groupedTimelinePage(messages, 12, null, 1024 * 1024, turnProcesses(messages), () => false);
  assert.deepEqual(page.indexes, [0, 2, 3, messages.length - 1]);
});
