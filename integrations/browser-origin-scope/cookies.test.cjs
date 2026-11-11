'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { synchronizeResponseCookies } = require('./cookies.cjs');

function fakePage(url, options = {}) {
  const frame = {};
  let handler, closed = false, fulfilled;
  const page = {
    mainFrame: () => frame,
    async route(pattern, callback) { assert.equal(pattern, '**/*'); handler = callback; },
    async goto(value) {
      assert.equal(value, url.href);
      if (options.goto) return options.goto();
      await handler({ request: () => ({ url: () => options.requestUrl || url.href, method: () => 'GET', isNavigationRequest: () => true, frame: () => frame }),
        async fulfill(response) { fulfilled = response; }, async abort() { throw new Error('unexpected abort'); } });
      return { status: () => 200, url: () => url.href };
    },
    async close() { closed = true; if (options.closeFails) throw new Error('close failed'); },
  };
  return { page, closed: () => closed, fulfilled: () => fulfilled };
}

test('no cookie headers creates no page and malformed headers fail before mutation', async () => {
  let created = 0;
  const context = { async newPage() { created++; } }, signal = new AbortController().signal;
  const url = new URL('https://example.test/private/file');
  await synchronizeResponseCookies(context, url, undefined, signal, async () => {});
  for (const headers of ['invalid', ['session=x\r\nInjected: y'], ['a=' + 'x'.repeat(17000)], ['session=x; Partitioned; Secure'], ['session=x; pArTiTiOnEd=true']])
    await assert.rejects(synchronizeResponseCookies(context, url, headers, signal, async () => {}));
  assert.equal(created, 0);
});

test('synchronization locally fulfills only cookie headers and closes the temporary page', async () => {
  const url = new URL('https://example.test/private/file'), state = fakePage(url);
  let disposed = 0;
  await synchronizeResponseCookies({ newPage: async () => state.page }, url,
    ['session=x; HttpOnly', 'other=y; Secure'], new AbortController().signal, async () => { disposed++; });
  assert.equal(state.closed(), true);
  assert.equal(disposed, 0);
  assert.equal(state.fulfilled().body, '');
  assert.equal(state.fulfilled().headers['set-cookie'], 'session=x; HttpOnly\nother=y; Secure');
  assert.equal(state.fulfilled().headers['content-type'], 'text/plain');
  assert.equal(state.fulfilled().headers['content-security-policy'], "default-src 'none'; sandbox");
});

test('cancel during page creation awaits captured-owner cleanup and never navigates late', async () => {
  let began, release, disposed = 0, navigations = 0, pageClosed = false;
  const ready = new Promise(resolve => { began = resolve; });
  const opening = new Promise(resolve => { release = resolve; });
  const controller = new AbortController();
  const pending = synchronizeResponseCookies({ newPage: () => { began(); return opening; } }, new URL('https://example.test/'),
    ['session=x'], controller.signal, async () => {
      disposed++;
      release({ async goto() { navigations++; }, async close() { pageClosed = true; } });
    });
  await ready; controller.abort();
  await assert.rejects(pending);
  assert.equal(disposed, 1); assert.equal(navigations, 0); assert.equal(pageClosed, true);
});

test('page close failure is not a successful sync and invalidates the captured browser', async () => {
  const url = new URL('https://example.test/'), state = fakePage(url, { closeFails: true });
  let disposed = 0;
  await assert.rejects(synchronizeResponseCookies({ newPage: async () => state.page }, url,
    ['session=x'], new AbortController().signal, async () => { disposed++; }));
  assert.equal(disposed, 1);
});


test('an unexpected native request is aborted and cannot commit cookie headers', async () => {
  const url = new URL('https://example.test/'), state = fakePage(url, { requestUrl: 'https://other.test/' });
  let disposed = 0;
  await assert.rejects(synchronizeResponseCookies({ newPage: async () => state.page }, url,
    ['session=x'], new AbortController().signal, async () => { disposed++; }));
  assert.equal(state.fulfilled(), undefined); assert.equal(state.closed(), true); assert.equal(disposed, 1);
});
