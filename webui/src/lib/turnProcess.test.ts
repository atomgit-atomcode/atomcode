import { test } from 'node:test';
import assert from 'node:assert/strict';
import { turnProcesses, type ProcessMessage } from './turnProcess.ts';
const fixture = (): ProcessMessage[] => [
  { role: 'user', ts: 1000, parts: [{ kind: 'text', text: 'implement' }] },
  { role: 'assistant', parts: [{ kind: 'text', text: 'checking' }, { kind: 'tool', tool: { id: 'read', name: 'read', args: '{}', status: 'done' } }] },
  { role: 'assistant', ts: 4000, parts: [{ kind: 'text', text: 'final answer' }] },
];
test('completed process has a separate final answer and duration, without mutating history', () => {
  const messages = fixture(); const before = JSON.stringify(messages);
  const group = turnProcesses(messages).get(1)!;
  assert.equal(group.foldable, true); assert.equal(group.last, 2);
  assert.equal(group.tools, 1); assert.equal(group.duration, 3000);
  assert.deepEqual(group.finalParts, [{ kind: 'text', text: 'final answer' }]);
  assert.equal(JSON.stringify(messages), before);
});
test('active, interrupted, failed, and unresolved turns stay expanded', () => {
  assert.equal(turnProcesses(fixture(), true).get(1)!.foldable, false);
  for (const stop of ['cancelled', 'timeout', 'error']) {
    const messages = fixture(); messages[2].turnStop = stop;
    assert.equal(turnProcesses(messages).get(1)!.foldable, false);
  }
  for (const status of ['pending', 'waiting_approval', 'incomplete'] as const) {
    const messages = fixture(); const part = messages[1].parts[1];
    if (part.kind === 'tool') part.tool.status = status;
    assert.equal(turnProcesses(messages).get(1)!.foldable, false);
  }
});
test('one message can contain both process and final answer; tool-only tails never fold', () => {
  const messages = fixture(); messages[1].parts.push(...messages.pop()!.parts);
  assert.deepEqual(turnProcesses(messages).get(1)!.finalParts, [{ kind: 'text', text: 'final answer' }]);
  messages[1].parts.pop();
  assert.equal(turnProcesses(messages).get(1)!.foldable, false);
  assert.equal(turnProcesses([{ role: 'assistant', parts: [{ kind: 'text', text: 'simple answer' }] }]).size, 0);
});
test('later user input finishes the earlier group while the current one stays live', () => {
  const messages = fixture(); messages.push(...fixture());
  const groups = turnProcesses(messages, true);
  assert.equal(groups.get(1)!.foldable, true); assert.equal(groups.get(4)!.foldable, false);
});

test('reused tool ids in different turns have independent disclosure identities', () => {
  const first = fixture(); first[0].sourceSeq = 2;
  const second = fixture(); second[0].sourceSeq = 9;
  const groups = turnProcesses([...first, ...second]);
  assert.notEqual(groups.get(1)!.key, groups.get(4)!.key);
});

test('warnings and archived answer previews cannot be mistaken for a complete final answer', () => {
  const warned = fixture(); warned[2].parts.unshift({ kind: 'notice', text: 'needs attention' });
  assert.equal(turnProcesses(warned).get(1)!.foldable, false);
  const archived = fixture(); archived[2].archiveKey = 'archived-answer';
  assert.equal(turnProcesses(archived).get(1)!.foldable, false);
});


test('steering input does not collapse an earlier part of the running turn', () => {
  const messages = fixture();
  messages[2].ts = undefined;
  messages.push({ role: 'user', parts: [{ kind: 'text', text: 'also check this' }] });
  messages.push({ role: 'assistant', parts: [{ kind: 'text', text: 'still working' }] });
  assert.equal(turnProcesses(messages, true).get(1)!.foldable, false);
});
