import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SessionStore } from '../src/sessions.js';

test('sessionId 读写', () => {
  const s = new SessionStore();
  assert.equal(s.getSessionId('u'), undefined);
  s.setSessionId('u', 'sess-1');
  assert.equal(s.getSessionId('u'), 'sess-1');
});

test('contextToken 读写', () => {
  const s = new SessionStore();
  s.setContextToken('u', 'CTX');
  assert.equal(s.getContextToken('u'), 'CTX');
});

test('审批 pending 设置/读取/清除', () => {
  const s = new SessionStore();
  assert.equal(s.getPending('u'), null);
  s.setPending('u', { sessionId: 'sess-1' });
  assert.deepEqual(s.getPending('u'), { sessionId: 'sess-1' });
  s.clearPending('u');
  assert.equal(s.getPending('u'), null);
});
