import test from 'node:test';
import assert from 'node:assert/strict';
import { adjacentTimelineStart, archiveCandidates, archivedMessage, targetTimelineStart, boundedTimelineRange, residentStart, timelineWeight, replaceArchived, assistantTurnRange, cacheSession, segmentMessages, timelineLimits, timelineRange, type TimelineMessage } from './timelineMemory.ts';
const tool = (id: number, status: 'done' | 'pending' | 'waiting_approval' = 'done') => ({ kind: 'tool' as const,
  tool: { id: String(id), name: 'bash', args: '{}', output: 'result ' + id, status } });

test('a thousand-tool single turn becomes small lossless chronological display segments', () => {
  const parts = Array.from({ length: 1000 }, (_, i) => tool(i));
  const input: TimelineMessage[] = [{ role: 'user', ts: 1, parts: [{ kind: 'text', text: 'run' }] }, { role: 'assistant', ts: 2, parts }];
  const output = segmentMessages(input);
  assert.deepEqual(output.slice(1).flatMap(m => m.parts), parts);
  assert.ok(output.slice(1).every(m => m.parts.length <= 8));
  assert.equal(output.filter(m => m.ts === 2).length, 1);
  assert.equal(output.at(-1)?.ts, 2);
  assert.equal(segmentMessages(output), output);
  assert.equal(input[1].parts.length, 1000);
});

test('parallel pending and approval calls remain in the mutable last segment', () => {
  const parts = [...Array.from({ length: 50 }, (_, i) => tool(i)), tool(50, 'waiting_approval'), tool(51, 'pending'),
    ...Array.from({ length: 30 }, (_, i) => tool(52 + i)), { kind: 'text' as const, text: 'streaming' }];
  const output = segmentMessages([{ role: 'assistant' as const, parts }]);
  assert.deepEqual(output.flatMap(m => m.parts), parts);
  assert.equal(output.at(-1)?.parts.filter(p => p.kind === 'tool' && p.tool.status !== 'done').length, 2);
  assert.equal(output.at(-1)?.parts.at(-1), parts.at(-1));
});

test('cold metadata drops images/output while preserving navigation and original payload', () => {
  const input = { role: 'user' as const, ts: 12, pendingSteerId: 'pending', images: [{ data: 'big' }], parts: [{ kind: 'text' as const, text: 'a'.repeat(10000) }] };
  const stub = archivedMessage(input, 'unique-key');
  assert.equal(stub.parts[0].kind === 'text' && stub.parts[0].text.length, 240);
  assert.equal(stub.images, undefined);
  assert.equal(stub.ts, 12);
  assert.equal(input.images[0].data, 'big');
  assert.equal(input.parts[0].text.length, 10000);
});

test('both device sizes have bounded ranges; explicit old reading stays put as data arrives', () => {
  for (const compact of [false, true]) {
    const limits = timelineLimits(compact);
    assert.ok(limits.mounted < limits.resident);
    assert.equal(timelineRange(10000, limits.mounted, null).end, 10000);
    assert.deepEqual(timelineRange(10000, limits.mounted, 40), timelineRange(20000, limits.mounted, 40));
  }
  assert.ok(timelineLimits(true).mounted < timelineLimits(false).mounted);
});

test('copy ranges include every segment and transparent notice, stopping at real user boundaries', () => {
  const messages: TimelineMessage[] = [
    { role: 'user', parts: [] }, { role: 'assistant', parts: [] }, { role: 'system', parts: [] },
    { role: 'assistant', parts: [] }, { role: 'user', parts: [] }, { role: 'assistant', parts: [] },
  ];
  assert.deepEqual(assistantTurnRange(messages, 3), { start: 1, end: 4 });
  assert.deepEqual(assistantTurnRange(messages, 5), { start: 5, end: 6 });
});

test('session cache eviction retains latest sessions and never modifies payloads', () => {
  const cache = new Map<string, number>();
  for (let i = 0; i < 1000; i++) cacheSession(cache, String(i), i);
  assert.deepEqual([...cache.values()], [997, 998, 999]);
  cacheSession(cache, '997', 4); cacheSession(cache, 'new', 5);
  assert.deepEqual([...cache.keys()], ['999', '997', 'new']);
});


test('late archive writes cannot evict newer streaming content or a replacement session', () => {
  const original: TimelineMessage = { role: 'assistant', parts: [tool(1)] };
  const updated: TimelineMessage = { ...original, parts: [...original.parts, tool(2)] };
  const entries = [{ key: 'archive-1', value: original }];
  assert.equal(replaceArchived([updated], entries)[0], updated);
  const replacement: TimelineMessage = { role: 'assistant', parts: [tool(3)] };
  assert.equal(replaceArchived([replacement], entries)[0], replacement);
  const archived = replaceArchived([original, updated], entries);
  assert.equal(archived[0].archiveKey, 'archive-1');
  assert.equal(archived[1], updated);
});


test('payload byte budgets bound large-output caches and page hydration, not just row counts', () => {
  const messages = Array.from({ length: 100 }, () => ({ role: 'assistant' as const,
    parts: [{ kind: 'text' as const, text: 'x'.repeat(10000) }] }));
  const weight = timelineWeight(messages[0]);
  assert.equal(residentStart(messages, 96, weight * 3), 97);
  const archived = messages.map((message, i) => archivedMessage(message, String(i)));
  assert.deepEqual(boundedTimelineRange(archived, 32, null, weight * 2), { start: 98, end: 100 });
  assert.deepEqual(boundedTimelineRange(archived, 32, 20, weight * 2), { start: 20, end: 22 });
  assert.deepEqual(boundedTimelineRange(messages, 32, 0, 1), { start: 0, end: 1 }, 'an oversized single entry remains readable');
  assert.equal(residentStart(messages, 96, 1), 99, 'the streaming tail is never discarded');
});


test('archive transactions use a byte budget and skip already archived entries', () => {
  const messages = Array.from({ length: 100 }, () => ({ role: 'assistant' as const, parts: [{ kind: 'text' as const, text: 'x'.repeat(10000) }] }));
  const weight = timelineWeight(messages[0]);
  assert.equal(archiveCandidates(messages, 100, weight * 2).length, 2);
  assert.equal(archiveCandidates(messages, 100, 1).length, 1);
  const stubs = messages.map((message, i) => archivedMessage(message, String(i)));
  assert.equal(archiveCandidates(stubs, 100).length, 0);
});


test('automatic paging visits every heavy entry and progresses near the end', () => {
  const messages = Array.from({ length: 100 }, () => ({ role: 'assistant' as const, parts: [{ kind: 'text' as const, text: 'x'.repeat(10000) }] }));
  const budget = timelineWeight(messages[0]);
  for (let index = 1; index < 100; index++) {
    const range = boundedTimelineRange(messages, 32, index, budget);
    const previous = adjacentTimelineStart(messages, range, -1, 32, budget);
    assert.equal(previous, index - 1);
    if (index < 99) assert.equal(adjacentTimelineStart(messages, range, 1, 32, budget), index + 1);
  }
  const target = 75;
  const start = targetTimelineStart(messages, target, 32, budget);
  const range = boundedTimelineRange(messages, 32, start, budget);
  assert.ok(range.start <= target && range.end > target);
});

test('previous pages meet the current page even with mixed large and small entries', () => {
  const messages = Array.from({ length: 60 }, (_, i) => ({ role: 'assistant' as const, parts: [{ kind: 'text' as const, text: 'x'.repeat(i < 30 ? 100000 : 1) }] }));
  const budget = timelineWeight(messages[0]);
  for (let index = 1; index < 60; index++) {
    const range = boundedTimelineRange(messages, 12, index, budget);
    const previous = adjacentTimelineStart(messages, range, -1, 12, budget);
    const before = boundedTimelineRange(messages, 12, previous, budget);
    assert.ok(before.start < range.start);
    assert.ok(before.end >= range.start, `gap before message ${index}`);
  }
});
