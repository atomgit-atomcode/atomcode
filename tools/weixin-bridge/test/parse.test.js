import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseInbound } from '../src/parse.js';

test('parseInbound 提取文本消息', () => {
  const msg = {
    from_user_id: 'u@im.wechat',
    message_type: 1,
    context_token: 'CTX1',
    item_list: [{ type: 1, text_item: { text: '你好' } }],
  };
  assert.deepEqual(parseInbound(msg), {
    fromUserId: 'u@im.wechat', text: '你好', contextToken: 'CTX1',
  });
});

test('parseInbound 忽略非用户消息(message_type!==1)', () => {
  assert.equal(parseInbound({ message_type: 2, item_list: [] }), null);
});

test('parseInbound 忽略无文本项', () => {
  const msg = { from_user_id: 'u', message_type: 1, item_list: [{ type: 2 }] };
  assert.equal(parseInbound(msg), null);
});

test('parseInbound 容忍空输入', () => {
  assert.equal(parseInbound(null), null);
});
