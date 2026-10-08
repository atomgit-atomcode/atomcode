import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fill, openSignIn } from './signIn.ts';

test('a blocked pop-up is told apart from one that opened, and an opened one is cut loose', () => {
  assert.equal(openSignIn('https://x/auth', () => null), false, 'blocked');
  const tab = { opener: {} as unknown };
  assert.equal(openSignIn('https://x/auth', () => tab), true, 'opened');
  assert.equal(tab.opener, null, 'the sign-in tab cannot reach back into this page');
  // Asked without `noopener`: with it the browser returns null either way.
  let target = '';
  openSignIn('https://x/auth', (_u, t) => {
    target = t;
    return null;
  });
  assert.equal(target, '_blank');
});

test('placeholders are filled, and an unknown one does not leak through', () => {
  assert.equal(fill('登录没有完成（{status}），请重试。', { status: '已超时' }), '登录没有完成（已超时），请重试。');
  assert.equal(fill('a {missing} b', {}), 'a  b');
});
