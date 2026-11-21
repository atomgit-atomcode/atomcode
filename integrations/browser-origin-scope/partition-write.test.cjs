'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { withNativeCookieValidation } = require('./cookies.cjs');
const { synchronizeBrowserCookies } = require('./partition-cookies.cjs');

const source = new URL('https://example.test/private/rotate');
const expired = 'chips=value; Secure; Partitioned; Max-Age=0';
const candidate = { name: 'chips', value: 'value', domain: 'example.test', path: '/', secure: true,
  httpOnly: false, session: true, expires: -1, priority: 'Medium', sourceScheme: 'Secure', sourcePort: 443 };
function fakeNative(options = {}) {
  const events = [], commands = [], jars = new Map();
  let created = 0, closedOwner = 0;
  const makeContext = id => {
    jars.set(id, []);
    const context = {
      browser: () => browser, pages: () => [],
      async addCookies(values) { jars.set(id, [{ name: values[0].name, partitionKey: source.origin }]); },
      async cookies() { return jars.get(id); },
      async clearCookies() { events.push(id + ':clear'); jars.set(id, []); },
      async newPage() {
        let handler;
        const frame = {};
        const page = {
          mainFrame: () => frame,
          async route(_pattern, fn) { handler = fn; },
          async goto(url) {
            await handler({ request: () => ({ url: () => url, method: () => 'GET', isNavigationRequest: () => true, frame: () => frame }),
              async fulfill(response) {
                const header = response.headers['set-cookie'];
                if (header.startsWith('atomcode-cookie-validation-')) {
                  const name = header.slice(0, header.indexOf('='));
                  const value = { ...candidate, name, value: '1' };
                  if (options.changedControl) value.partitionKey = { topLevelSite: source.origin, hasCrossSiteAncestor: false };
                  jars.set(id, [value]);
                } else if (header === expired) {
                  if (!options.ignoreExpired) jars.set(id, []);
                } else jars.set(id, [{ ...candidate }]);
              }, async abort() {},
            });
            return { status: () => 200, url: () => url };
          },
          async close() { events.push(id + ':page-close'); },
        };
        return page;
      },
      async newCDPSession() { return {
        async send(method) { assert.equal(method, 'Target.getTargetInfo'); return { targetInfo: { browserContextId: options.reuseOriginal && id !== 'original' ? 'original' : id } }; },
        async detach() { events.push(id + ':target-detach'); },
      }; },
      async close() { events.push(id + ':close'); if (options.cleanupFails && id !== 'original') throw new Error('cleanup failed'); },
    };
    return context;
  };
  const browser = {
    async newContext() { return makeContext('scratch-' + ++created); },
    async newBrowserCDPSession() { return {
      async send(method, params) {
        commands.push({ method, params });
        if (method === 'Storage.setCookies') {
          assert.equal(params.browserContextId, 'original');
          const cookie = params.cookies[0];
          assert.deepEqual(cookie.partitionKey, { topLevelSite: source.origin, hasCrossSiteAncestor: false });
          if (!options.ignoreCommit) {
            const value = { ...cookie, session: cookie.expires === undefined, expires: cookie.expires ?? -1 };
            if (options.wrongKey) value.partitionKey = { topLevelSite: 'https://foreign.test', hasCrossSiteAncestor: false };
            if (options.missingExpiry) delete value.expires;
            jars.set('original', cookie.expires === 1 ? [] : [value]);
          }
          options.afterCommit?.();
          return {};
        }
        assert.equal(method, 'Storage.getCookies');
        return { cookies: jars.get(params.browserContextId) || [] };
      },
      async detach() { events.push('storage-detach'); },
    }; },
  };
  const context = makeContext('original');
  return { browser, context, events, commands, close: async () => { closedOwner++; }, closedOwner: () => closedOwner };
}

for (const ignored of [false, true]) {
  test(`native seed distinguishes ${ignored ? 'ignored malformed' : 'expired deletion'} from empty parse`, async () => {
    const f = fakeNative({ ignoreExpired: ignored });
    const result = await withNativeCookieValidation(f.browser, source, new AbortController().signal, 'original', parse => parse(expired));
    if (ignored) assert.equal(result, undefined);
    else { assert.equal(result.deleted, true); assert.equal(result.cookie.name, 'chips'); }
    assert.ok(f.events.includes('scratch-1:close'));
    assert.ok(!f.commands.some(command => command.method === 'Storage.setCookies'));
  });
}

test('validation refuses changed native partition semantics before invoking the writer', async () => {
  const f = fakeNative({ changedControl: true }); let invoked = false;
  await assert.rejects(withNativeCookieValidation(f.browser, source, new AbortController().signal, 'original', async () => { invoked = true; }));
  assert.equal(invoked, false); assert.ok(f.events.includes('scratch-1:close'));
});

test('validation refuses the original context identity before clearing or applying any cookie', async () => {
  const f = fakeNative({ reuseOriginal: true });
  await assert.rejects(withNativeCookieValidation(f.browser, source, new AbortController().signal, 'original', async () => {}));
  assert.ok(!f.events.some(event => event.endsWith(':clear')));
});

for (const option of ['ignoreCommit', 'wrongKey', 'missingExpiry']) {
  test(`protocol success with ${option} cannot be reported as a committed cookie`, async () => {
    const f = fakeNative({ [option]: true });
    await assert.rejects(synchronizeBrowserCookies(f.context, source, ['chips=value; Secure; Partitioned; Path=/'], new AbortController().signal, f.close));
    assert.equal(f.closedOwner(), 1);
    assert.equal(f.commands.filter(command => command.method === 'Storage.setCookies').length, 1);
    assert.ok(f.events.includes('scratch-2:close'));
  });
}

test('cancellation after a committed write is uncertain and closes only the captured owner without retry', async () => {
  const controller = new AbortController();
  const f = fakeNative({ afterCommit: () => controller.abort() });
  await assert.rejects(synchronizeBrowserCookies(f.context, source, ['chips=value; Secure; Partitioned; Path=/'], controller.signal, f.close));
  assert.equal(f.closedOwner(), 1);
  assert.equal(f.commands.filter(command => command.method === 'Storage.setCookies').length, 1);
});

test('Partitioned HTTP batches are refused before ordinary-cookie mutation', async () => {
  const f = fakeNative();
  await assert.rejects(synchronizeBrowserCookies(f.context, new URL('http://example.test/'),
    ['ordinary=value', 'chips=value; Secure; Partitioned'], new AbortController().signal, f.close));
  assert.deepEqual(f.events, []); assert.deepEqual(f.commands, []);
});
