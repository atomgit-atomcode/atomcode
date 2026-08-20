import { test } from 'node:test';
import assert from 'node:assert';
import { randomId } from './randomId.ts';

const V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

test('uses native randomUUID when available (secure context)', () => {
  const fake = {
    randomUUID: () => '11111111-2222-4333-8444-555555555555',
    getRandomValues: () => {
      throw new Error('should not be called when randomUUID exists');
    },
  } as unknown as Crypto;
  assert.equal(randomId(fake), '11111111-2222-4333-8444-555555555555');
});

test('falls back to getRandomValues when randomUUID is missing (LAN HTTP / non-secure context)', () => {
  // 模拟局域网明文 HTTP：randomUUID 不存在，只有 getRandomValues。
  const fake = {
    getRandomValues: (arr: Uint8Array) => {
      for (let i = 0; i < arr.length; i++) arr[i] = i;
      return arr;
    },
  } as unknown as Crypto;
  const id = randomId(fake);
  assert.match(id, V4, `expected v4 uuid, got ${id}`);
});

test('falls back when crypto is entirely absent', () => {
  const id = randomId(undefined);
  assert.match(id, V4, `expected v4 uuid, got ${id}`);
});

test('produces distinct ids across calls', () => {
  assert.notEqual(randomId(), randomId());
});
