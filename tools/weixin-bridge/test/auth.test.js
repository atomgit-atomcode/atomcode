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
