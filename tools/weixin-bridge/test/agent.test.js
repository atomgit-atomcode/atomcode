import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseSseLines, dispatch, AgentClient } from '../src/agent.js';

test('parseSseLines 解析完整事件并保留残块', () => {
  const buf =
    'data: {"type":"text","content":"hi"}\n\n' +
    'data: {"type":"done","session_id":"s1"}\n\n' +
    'data: {"type":"text","conte';
  const { events, rest } = parseSseLines(buf);
  assert.deepEqual(events, [
    { type: 'text', content: 'hi' },
    { type: 'done', session_id: 's1' },
  ]);
  assert.equal(rest, 'data: {"type":"text","conte');
});

test('dispatch 路由到对应 handler', () => {
  const calls = [];
  const h = {
    onText: (c) => calls.push(['text', c]),
    onPermission: (e) => calls.push(['perm', e.tool_name]),
    onDone: (e) => calls.push(['done', e.session_id]),
    onError: (m) => calls.push(['err', m]),
  };
  dispatch({ type: 'text', content: 'a' }, h);
  dispatch({ type: 'permission_request', tool_name: 'bash' }, h);
  dispatch({ type: 'done', session_id: 's1' }, h);
  dispatch({ type: 'error', message: 'boom' }, h);
  dispatch({ type: 'tokens', total: 1 }, h); // 未知类型忽略
  assert.deepEqual(calls, [
    ['text', 'a'], ['perm', 'bash'], ['done', 's1'], ['err', 'boom'],
  ]);
});

test('sendPermission POST 正确 body', async () => {
  let captured;
  const fetchImpl = async (url, opts) => { captured = { url, opts }; return { json: async () => ({ success: true }) }; };
  const c = new AgentClient({ baseUrl: 'http://d', fetchImpl });
  await c.sendPermission('s1', 'allow');
  assert.equal(captured.url, 'http://d/chat/permission');
  assert.deepEqual(JSON.parse(captured.opts.body), { session_id: 's1', decision: 'allow' });
});
