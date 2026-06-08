import { test } from 'node:test';
import assert from 'node:assert/strict';
import { makeBridge } from '../src/bridge.js';
import { SessionStore } from '../src/sessions.js';

function mkIlink() {
  const sent = [];
  return { sent, sendMessage: (to, text, ctx) => { sent.push({ to, text, ctx }); } };
}

test('普通对话：累积 text 并在 done 时回复 + 存 sessionId', async () => {
  const sessions = new SessionStore();
  const ilink = mkIlink();
  const agent = {
    streamChat: async (req, h) => { h.onText('你'); h.onText('好'); h.onDone({ session_id: 's1' }); },
    sendPermission: async () => { throw new Error('不应被调用'); },
  };
  const { handleInbound } = makeBridge({ sessions, agent, ilink, allowlist: [], workingDir: '/w' });
  await handleInbound({ fromUserId: 'u', text: 'hi', contextToken: 'C1' });
  assert.deepEqual(ilink.sent, [{ to: 'u', text: '你好', ctx: 'C1' }]);
  assert.equal(sessions.getSessionId('u'), 's1');
});

test('allowlist 拦截非白名单用户', async () => {
  const ilink = mkIlink();
  let called = false;
  const agent = { streamChat: async () => { called = true; }, sendPermission: async () => {} };
  const { handleInbound } = makeBridge({
    sessions: new SessionStore(), agent, ilink, allowlist: ['ok'], workingDir: '/w',
  });
  await handleInbound({ fromUserId: 'bad', text: 'hi', contextToken: 'C' });
  assert.equal(called, false);
  assert.equal(ilink.sent.length, 0);
});

test('权限请求：设置 pending 并发微信审批提示', async () => {
  const sessions = new SessionStore();
  const ilink = mkIlink();
  const agent = {
    streamChat: async (req, h) => {
      h.onPermission({ session_id: 's1', tool_name: 'bash', arguments: 'ls -la' });
    },
    sendPermission: async () => {},
  };
  const { handleInbound } = makeBridge({ sessions, agent, ilink, allowlist: [], workingDir: '/w' });
  await handleInbound({ fromUserId: 'u', text: '跑个命令', contextToken: 'C1' });
  assert.deepEqual(sessions.getPending('u'), { sessionId: 's1' });
  assert.equal(ilink.sent.length, 1);
  assert.match(ilink.sent[0].text, /bash/);
  assert.match(ilink.sent[0].text, /y 同意/);
});

test('审批回复 y -> allow；n -> deny；并清除 pending', async () => {
  const sessions = new SessionStore();
  const ilink = mkIlink();
  const decisions = [];
  const agent = {
    streamChat: async () => { throw new Error('不应开新对话'); },
    sendPermission: async (sid, d) => { decisions.push([sid, d]); },
  };
  const { handleInbound } = makeBridge({ sessions, agent, ilink, allowlist: [], workingDir: '/w' });

  sessions.setPending('u', { sessionId: 's1' });
  await handleInbound({ fromUserId: 'u', text: 'y', contextToken: 'C2' });
  assert.deepEqual(decisions, [['s1', 'allow']]);
  assert.equal(sessions.getPending('u'), null);

  sessions.setPending('u', { sessionId: 's2' });
  await handleInbound({ fromUserId: 'u', text: 'n', contextToken: 'C3' });
  assert.deepEqual(decisions[1], ['s2', 'deny']);
});

test('error 事件转成微信错误消息', async () => {
  const sessions = new SessionStore();
  const ilink = mkIlink();
  const agent = {
    streamChat: async (req, h) => { h.onError('boom'); },
    sendPermission: async () => {},
  };
  const { handleInbound } = makeBridge({ sessions, agent, ilink, allowlist: [], workingDir: '/w' });
  await handleInbound({ fromUserId: 'u', text: 'hi', contextToken: 'C1' });
  assert.equal(ilink.sent.length, 1);
  assert.match(ilink.sent[0].text, /出错了/);
});
