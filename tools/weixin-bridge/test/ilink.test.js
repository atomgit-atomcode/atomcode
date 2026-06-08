import { test } from 'node:test';
import assert from 'node:assert/strict';
import { randomUin, buildHeaders, IlinkClient } from '../src/ilink.js';

test('randomUin 是 base64(uint32 十进制字符串)', () => {
  const uin = randomUin();
  const n = Number(Buffer.from(uin, 'base64').toString('utf8'));
  assert.ok(Number.isInteger(n) && n >= 0 && n <= 0xFFFFFFFF);
});

test('buildHeaders 固定字段 + 仅在有 token 时带 Authorization', () => {
  const h0 = buildHeaders(null);
  assert.equal(h0['Content-Type'], 'application/json');
  assert.equal(h0['AuthorizationType'], 'ilink_bot_token');
  assert.ok(h0['X-WECHAT-UIN']);
  assert.equal(h0['Authorization'], undefined);
  const h1 = buildHeaders('TOK');
  assert.equal(h1['Authorization'], 'Bearer TOK');
});

test('getUpdates POST 正确 body', async () => {
  let captured;
  const fetchImpl = async (url, opts) => {
    captured = { url, opts };
    return { json: async () => ({ ret: 0, msgs: [], get_updates_buf: 'NEXT' }) };
  };
  const c = new IlinkClient({ baseUrl: 'https://x', fetchImpl, channelVersion: '1.0.2' });
  c.setToken('TOK');
  const r = await c.getUpdates('BUF');
  assert.equal(captured.url, 'https://x/ilink/bot/getupdates');
  assert.equal(captured.opts.method, 'POST');
  assert.deepEqual(JSON.parse(captured.opts.body), {
    get_updates_buf: 'BUF', base_info: { channel_version: '1.0.2' },
  });
  assert.equal(r.get_updates_buf, 'NEXT');
});

test('sendMessage 构造带 context_token 的 msg', async () => {
  let captured;
  const fetchImpl = async (url, opts) => { captured = { url, opts }; return { json: async () => ({ ret: 0 }) }; };
  const c = new IlinkClient({ baseUrl: 'https://x', fetchImpl });
  c.setToken('TOK');
  await c.sendMessage('u@im.wechat', '回复', 'CTX');
  assert.equal(captured.url, 'https://x/ilink/bot/sendmessage');
  assert.deepEqual(JSON.parse(captured.opts.body), {
    msg: {
      to_user_id: 'u@im.wechat',
      message_type: 2,
      message_state: 2,
      context_token: 'CTX',
      item_list: [{ type: 1, text_item: { text: '回复' } }],
    },
  });
});
