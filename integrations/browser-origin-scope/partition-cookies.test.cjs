'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { firstPartyCookieHeader } = require('./partition-cookies.cjs');
const { cookieHeader } = require('./cookies.cjs');

function fixture(options = {}) {
  const events = [], commands = [];
  const site = 'https://example.test';
  const base = { domain: 'download.example.test', path: '/', secure: true, expires: -1 };
  const cookies = [
    { ...base, name: 'ordinary', value: 'ordinary-fixture' },
    { ...base, name: 'first', value: 'first-fixture', partitionKey: { topLevelSite: site, hasCrossSiteAncestor: false } },
    { ...base, name: 'ancestor', value: 'never', partitionKey: { topLevelSite: site, hasCrossSiteAncestor: true } },
    { ...base, name: 'foreign', value: 'never', partitionKey: { topLevelSite: 'https://foreign.test', hasCrossSiteAncestor: false } },
    { ...base, name: 'opaque', value: 'never', partitionKeyOpaque: true },
    { ...base, name: 'otherDomain', value: 'never', domain: 'foreign.test' },
  ];
  let probeCookie;
  const probe = {
    async addCookies(values) { probeCookie = values[0]; events.push('probe-set'); },
    async cookies() { return [{ name: probeCookie.name, partitionKey: options.site || site }]; },
    async close() { events.push('probe-close'); },
  };
  const page = { isClosed: () => false, async close() { events.push('page-close'); } };
  const browser = {
    async newContext(settings) { assert.deepEqual(settings, { acceptDownloads: false, serviceWorkers: 'block', permissions: [] }); events.push('probe-open'); return options.open ? options.open() : probe; },
    async newBrowserCDPSession() { return {
      async send(method, params) {
        commands.push({ method, params });
        assert.equal(method, 'Storage.getCookies'); assert.deepEqual(params, { browserContextId: 'captured-context' });
        if (options.readFails) throw new Error('private cookie fixture-token');
        return { cookies };
      },
      async detach() { events.push('storage-detach'); if (options.cleanupFails) throw new Error('cleanup failed'); },
    }; },
  };
  const context = {
    browser: () => browser,
    pages: () => [],
    async newPage() { events.push('page-open'); return page; },
    async newCDPSession(target) { assert.equal(target, page); return {
      async send(method) { commands.push({ method }); assert.equal(method, 'Target.getTargetInfo'); return { targetInfo: { browserContextId: options.invalidId ? '' : 'captured-context' } }; },
      async detach() { events.push('target-detach'); },
    }; },
  };
  const close = async () => { events.push('owner-close'); options.onClose?.(probe); };
  return { context, events, commands, close, cookies, site, page };
}

test('first-party lookup binds the original context ID and returns only scoped ordinary/matching keys', async () => {
  const f = fixture();
  const result = await firstPartyCookieHeader(f.context, new URL('https://download.example.test/private/file'), new AbortController().signal, f.close);
  assert.equal(result, 'ordinary=ordinary-fixture; first=first-fixture');
  assert.deepEqual(f.commands.map(command => command.method), ['Target.getTargetInfo', 'Storage.getCookies']);
  assert.deepEqual(f.events, ['probe-open', 'probe-set', 'probe-close', 'page-open', 'storage-detach', 'target-detach', 'page-close']);
});

test('public partition strings, wrong ancestry, opaque metadata and HTTP targets cannot become eligible', () => {
  const f = fixture();
  assert.equal(cookieHeader(f.cookies, new URL('https://download.example.test/'), 0), 'ordinary=ordinary-fixture');
  assert.equal(cookieHeader([{ ...f.cookies[1], partitionKey: f.site }], new URL('https://download.example.test/'), 0, f.site), '');
  assert.equal(cookieHeader([{ ...f.cookies[1], secure: false }], new URL('http://download.example.test/'), 0, f.site), '');
  assert.equal(cookieHeader([{ ...f.cookies[0], partitionKeyOpaque: 'unknown' }], new URL('https://download.example.test/'), 0, f.site), '');
});

for (const option of ['invalidId', 'readFails', 'cleanupFails']) {
  test(`${option} never returns credentials or fake success and closes the captured owner`, async () => {
    const f = fixture({ [option]: true });
    await assert.rejects(firstPartyCookieHeader(f.context, new URL('https://download.example.test/'), new AbortController().signal, f.close), error => !error.message.includes('fixture-token'));
    assert.ok(f.events.includes('owner-close')); assert.ok(f.events.includes('page-close')); assert.ok(f.events.includes('probe-close'));
    if (option === 'invalidId') assert.ok(!f.commands.some(command => command.method === 'Storage.getCookies'));
  });
}

test('cancelled probe creation awaits resource disposal and never reads original credentials', async () => {
  let began, release;
  const ready = new Promise(resolve => { began = resolve; });
  const pendingProbe = new Promise(resolve => { release = resolve; });
  const f = fixture({ open: () => { began(); return pendingProbe; }, onClose: probe => release(probe) });
  const controller = new AbortController();
  const pending = firstPartyCookieHeader(f.context, new URL('https://download.example.test/'), controller.signal, f.close);
  await ready; controller.abort(); await assert.rejects(pending);
  assert.deepEqual(f.commands, []); assert.ok(f.events.includes('probe-close'));
  assert.equal(f.events.filter(event => event === 'owner-close').length, 1);
});


test('metadata lookup borrows an existing tab without closing or navigating it', async () => {
  const f = fixture(); f.context.pages = () => [f.page];
  const result = await firstPartyCookieHeader(f.context, new URL('https://download.example.test/'), new AbortController().signal, f.close);
  assert.ok(result.includes('first=first-fixture'));
  assert.ok(!f.events.includes('page-open')); assert.ok(!f.events.includes('page-close'));
});
