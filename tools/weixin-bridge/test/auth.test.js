import { test } from 'node:test';
import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { mkdtempSync, rmSync } from 'node:fs';
import { loadToken, saveToken } from '../src/auth.js';

test('saveToken/loadToken 往返 + 自动建目录', () => {
  const dir = mkdtempSync(join(tmpdir(), 'wxbridge-'));
  try {
    const p = join(dir, 'nested', 'bot.json');
    assert.equal(loadToken(p), null);          // 不存在返回 null
    saveToken(p, 'TOK123');                     // 应自动建 nested/
    assert.equal(loadToken(p), 'TOK123');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

import { login } from '../src/auth.js';

test('login 渲染 qrcode_img_content(URL)、用 qrcode(id) 轮询，确认后返回 bot_token', async () => {
  const calls = { rendered: null, polledWith: [] };
  let n = 0;
  const ilink = {
    baseUrl: 'https://x',
    getBotQrcode: async () => ({ qrcode: 'POLLID', qrcode_img_content: 'https://scan/url', ret: 0 }),
    getQrcodeStatus: async (id) => {
      calls.polledWith.push(id);
      n++;
      return n < 2
        ? { ret: 0, status: 'wait' }
        : { ret: 0, status: 'confirmed', bot_token: 'TOKEN', baseurl: 'https://b' };
    },
  };
  const token = await login(ilink, { render: (s) => { calls.rendered = s; }, pollIntervalMs: 0 });
  assert.equal(calls.rendered, 'https://scan/url');        // 渲染 img_content，不是 poll id
  assert.deepEqual(calls.polledWith, ['POLLID', 'POLLID']); // 轮询用 qrcode(id)
  assert.equal(token, 'TOKEN');
  assert.equal(ilink.baseUrl, 'https://b');
});
