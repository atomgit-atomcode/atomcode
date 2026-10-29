import { test } from 'node:test';
import assert from 'node:assert/strict';
import { backgroundPresentation, groupEmptyPolls } from './backgroundTools.ts';
import type { MsgPart, ToolRow } from './toolRows.ts';

const poll = (id: string, overrides: Partial<ToolRow> = {}): ToolRow => ({
  id, name: 'bash_poll', args: '{"job_id":"bg-9","wait_ms":240000}', status: 'done', output: 'status: running', ...overrides,
});
const assistant = (...tools: ToolRow[]) => ({ role: 'assistant', parts: tools.map(tool => ({ kind: 'tool' as const, tool })) as MsgPart[] });

test('a completed poll reports a running process without claiming tool failure or task success', () => {
  const tool = poll('a');
  assert.deepEqual(backgroundPresentation(tool), { title: 'tool.backgroundPoll', detail: 'bg-9', cls: 'background', label: 'tool.jobRunning' });
  assert.equal(tool.status, 'done', 'presentation must not change the persisted call lifecycle');
  assert.equal(backgroundPresentation(poll('a', { status: 'pending', output: undefined }))?.label, 'tool.pollWaiting');
});

test('poll presentation distinguishes process exit, cancellation, and live descendants', () => {
  for (const [output, cls, label] of [
    ['test passed\nstatus: exited (code 0)', 'success', 'tool.jobExited'],
    ['compiler failed\nstatus: exited (code 2)', 'error', 'tool.jobExited'],
    ['status: exited (code -1)', 'error', 'tool.jobExited'],
    ['status: killed', 'warning', 'tool.jobStopped'],
    ['status: exited (code 0); what it started is still running and writing here — keep polling, or stop it with bash_kill', 'background', 'tool.jobChildrenRunning'],
  ]) {
    const view = backgroundPresentation(poll('a', { output }));
    assert.equal(view?.cls, cls);
    assert.equal(view?.label, label);
  }
  assert.deepEqual(backgroundPresentation(poll('a', { output: 'status: exited (code 2)' }))?.params, { code: '2' });
});

test('unknown or ambiguous poll output is neutral; arbitrary tools are never parsed as jobs', () => {
  for (const output of ['', 'status: running\nthis is not a final status', 'unknown job projection']) {
    assert.equal(backgroundPresentation(poll('a', { output }))?.label, 'tool.pollReturned');
    assert.equal(backgroundPresentation(poll('a', { output }))?.cls, 'neutral');
  }
  for (const name of ['read_file', 'bash', 'mcp__server__bash_poll', 'task']) {
    assert.equal(backgroundPresentation(poll('a', { name })), undefined);
  }
  for (const status of ['error', 'incomplete', 'waiting_approval'] as const) {
    assert.equal(backgroundPresentation(poll('a', { status })), undefined, 'retain the actual failure or approval state');
  }
});

test('Bash and PowerShell start acknowledgements do not imply task completion', () => {
  for (const name of ['bash_start', 'pwsh_start']) {
    const view = backgroundPresentation(poll('a', { name, args: '{"command":"build project"}', output: 'Started background job bg-9. Collect output with bash_poll {"job_id":"bg-9"}' }));
    assert.equal(view?.label, 'tool.jobStarted');
    assert.equal(view?.cls, 'neutral');
    assert.equal(view?.detail, 'bg-9 · build project');
  }
  assert.equal(backgroundPresentation(poll('a', { name: 'bash_start', output: 'unrecognized result' }))?.label, 'tool.requestProcessed');
  assert.equal(backgroundPresentation(poll('a', { name: 'bash_kill' }))?.label, 'tool.stopRequestProcessed');
});

test('team dispatch and message acknowledgements are operation-specific', () => {
  for (const [action, label] of [['delegate', 'tool.teamDelegated'], ['tell', 'tool.teamSent'], ['status', 'tool.pollReturned'], ['stop', 'tool.stopRequestProcessed']]) {
    const view = backgroundPresentation(poll('a', { name: 'team', args: JSON.stringify({ action, name: 'reviewer' }) }));
    assert.equal(view?.label, label);
    assert.equal(view?.cls, 'neutral');
    assert.equal(view?.detail, `${action} · reviewer`);
  }
  assert.equal(backgroundPresentation(poll('a', { name: 'team', args: '{"action":"constructor"}' })), undefined);
});

test('adjacent empty polls across assistant rounds share a stable first-call anchor', () => {
  const first = poll('a'); const second = poll('b'); const pending = poll('c', { status: 'pending', output: undefined });
  const original = [assistant(first), assistant(second), assistant(pending)];
  const before = JSON.stringify(original);
  const grouped = groupEmptyPolls(original);
  assert.deepEqual([...grouped.byFirstId.keys()], ['a']);
  assert.deepEqual(grouped.byFirstId.get('a'), [first, second, pending]);
  assert.deepEqual([...grouped.hidden], ['b', 'c']);
  assert.equal(JSON.stringify(original), before);
  const updated = groupEmptyPolls([assistant(first), assistant(second), assistant(poll('c'))]);
  assert.deepEqual([...updated.byFirstId.keys()], ['a']);
});

test('text, notices, user turns, system records, and other tools break a poll streak', () => {
  const separators: { role: string; parts: MsgPart[] }[] = [
    { role: 'assistant', parts: [{ kind: 'text', text: 'Check this result next' }] },
    { role: 'assistant', parts: [{ kind: 'notice', text: 'Cancelled' }] },
    { role: 'assistant', parts: [{ kind: 'rate_limited', text: 'Retry later' }] },
    { role: 'user', parts: [{ kind: 'text', text: 'New request' }] },
    { role: 'system', parts: [] },
    assistant(poll('other', { name: 'bash', args: '{"command":"check files"}' })),
  ];
  for (const separator of separators) {
    const grouped = groupEmptyPolls([assistant(poll('a')), separator, assistant(poll('b'))]);
    assert.equal(grouped.hidden.size, 0);
  }
});

test('new output, progress, failures, job changes, and completion remain separate', () => {
  for (const middle of [
    poll('middle', { output: 'Downloaded 80 MB\nstatus: running' }),
    poll('middle', { output: 'status: exited (code 0)' }),
    poll('middle', { status: 'error' }),
    poll('middle', { progress: 'New stderr output' }),
    poll('middle', { args: '{"job_id":"bg-10"}' }),
  ]) {
    const grouped = groupEmptyPolls([assistant(poll('a'), middle, poll('b'))]);
    assert.equal(grouped.hidden.size, 0);
  }
});

test('an empty placeholder is transparent but missing or malformed identities never group', () => {
  const grouped = groupEmptyPolls([assistant(poll('a')), { role: 'assistant', parts: [{ kind: 'text' as const, text: '' }] }, assistant(poll('b'))]);
  assert.deepEqual([...grouped.hidden], ['b']);
  for (const args of ['{broken', 'null', '[]', '{}', '{"job_id":9}', '{"job_id":""}']) {
    assert.equal(groupEmptyPolls([assistant(poll('a', { args }), poll('b', { args }))]).hidden.size, 0);
  }
});

test('a completed poll with output leaves the previous empty-wait group intact', () => {
  const tools = [poll('a'), poll('b'), poll('c', { output: 'All done\nstatus: exited (code 0)' })];
  const grouped = groupEmptyPolls([assistant(...tools)]);
  assert.deepEqual([...grouped.hidden], ['b']);
  assert.equal(grouped.hidden.has('c'), false);
  assert.deepEqual(grouped.byFirstId.get('a'), tools.slice(0, 2));
});
