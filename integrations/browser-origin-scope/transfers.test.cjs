'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const { createHash } = require('node:crypto');
const { ControlledDownloads } = require('./transfers.cjs');

async function fixture(t, handler, options = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-transfer-test-'));
  const files = path.join(root, 'files');
  fs.mkdirSync(files);
  let requests = 0;
  const server = http.createServer((req, res) => { requests++; handler(req, res); });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;
  const owner = new ControlledDownloads({ files, authorize: candidate => candidate === origin, ...options });
  t.after(async () => {
    await owner.close();
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    fs.rmSync(root, { recursive: true });
  });
  return { owner, files, root, origin, requests: () => requests };
}

test('publishes exact complete bytes with generated name and hash', async t => {
  const body = Buffer.from('download fixture\0');
  const f = await fixture(t, (_req, res) => {
    res.writeHead(200, { 'Content-Disposition': 'attachment; filename="../../unsafe"' });
    res.end(body);
  });
  const result = await f.owner.execute(1, { url: `${f.origin}/file` });
  assert.deepEqual(fs.readFileSync(result.path), body);
  assert.equal(result.bytes, body.length);
  assert.equal(result.sha256, createHash('sha256').update(body).digest('hex'));
  assert.match(path.basename(result.path), /^download-[\da-f-]+\.bin$/);
  assert.deepEqual(fs.readdirSync(path.join(f.root, 'download-staging')), []);
  f.owner.cancel(1);
  assert.equal(fs.existsSync(result.path), true);
});

test('refuses denied origin and caller paths before network dispatch', async t => {
  const f = await fixture(t, (_req, res) => res.end('unexpected'), { authorize: () => false });
  await assert.rejects(f.owner.execute(1, { url: f.origin }));
  await assert.rejects(f.owner.execute(2, { url: f.origin, filename: '/tmp/unsafe' }));
  assert.equal(f.requests(), 0);
  assert.deepEqual(fs.readdirSync(f.files), []);
});

test('checks every redirect before dispatch and never follows denied destination', async t => {
  const denied = await fixture(t, (_req, res) => res.end('forbidden'));
  const f = await fixture(t, (_req, res) => {
    res.writeHead(302, { Location: denied.origin }); res.end();
  });
  await assert.rejects(f.owner.execute(1, { url: f.origin }));
  assert.equal(f.requests(), 1);
  assert.equal(denied.requests(), 0);
});

test('follows permitted redirects, refuses loops without replay', async t => {
  const f = await fixture(t, (req, res) => {
    if (req.url === '/good') res.end('ok');
    else { res.writeHead(302, { Location: req.url === '/loop' ? '/loop' : '/good' }); res.end(); }
  });
  const result = await f.owner.execute(1, { url: f.origin });
  assert.equal(fs.readFileSync(result.path, 'utf8'), 'ok');
  await assert.rejects(f.owner.execute(2, { url: `${f.origin}/loop` }));
  assert.equal(f.requests(), 3);
});

for (const mode of ['length', 'stream', 'encoding', 'truncated']) {
  test(`refuses ${mode} response without publishing or retaining staging`, async t => {
    const f = await fixture(t, (_req, res) => {
      if (mode === 'length') res.writeHead(200, { 'Content-Length': '100' });
      if (mode === 'stream') { res.writeHead(200); res.write('01234'); res.end('56789'); return; }
      if (mode === 'encoding') res.writeHead(200, { 'Content-Encoding': 'gzip' });
      if (mode === 'truncated') {
        res.writeHead(200, { 'Content-Length': '5' }); res.flushHeaders();
        res.write('a'); setImmediate(() => res.destroy()); return;
      }
      res.end('0123456789');
    }, { maxBytes: mode === 'truncated' ? 100 : 8 });
    await assert.rejects(f.owner.execute(1, { url: f.origin }));
    assert.deepEqual(fs.readdirSync(f.files), []);
    assert.deepEqual(fs.readdirSync(path.join(f.root, 'download-staging')), []);
    assert.equal(f.requests(), 1);
  });
}

test('cancellation drains a partial transfer and shutdown refuses new work', async t => {
  let started;
  const ready = new Promise(resolve => { started = resolve; });
  const f = await fixture(t, (_req, res) => { res.write('partial'); started(); });
  const result = f.owner.execute(1, { url: `${f.origin}/secret?token=hidden` });
  const rejected = assert.rejects(result, error => !error.message.includes('hidden'));
  await ready;
  f.owner.cancel(1);
  await rejected;
  await f.owner.close();
  await assert.rejects(f.owner.execute(2, { url: f.origin }));
  assert.equal(f.requests(), 1);
  assert.deepEqual(fs.readdirSync(f.files), []);
  assert.deepEqual(fs.readdirSync(path.join(f.root, 'download-staging')), []);
});

test('shutdown aborts pending response headers and waits for owned jobs', async t => {
  let started;
  const ready = new Promise(resolve => { started = resolve; });
  const f = await fixture(t, () => started());
  const result = f.owner.execute(1, { url: f.origin });
  const rejected = assert.rejects(result);
  await ready;
  await f.owner.close();
  await rejected;
  assert.equal(f.owner.jobs.size, 0);
  assert.deepEqual(fs.readdirSync(f.files), []);
});


test('deadline stops a stalled request without retry', async t => {
  const f = await fixture(t, () => {}, { timeoutMs: 20 });
  await assert.rejects(f.owner.execute(1, { url: f.origin }));
  assert.equal(f.requests(), 1);
  assert.equal(f.owner.jobs.size, 0);
  assert.deepEqual(fs.readdirSync(f.files), []);
});

test('symlinked staging and artifact directories are rejected', async t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-transfer-path-test-'));
  t.after(() => fs.rmSync(root, { recursive: true }));
  fs.mkdirSync(path.join(root, 'target'));
  fs.symlinkSync(path.join(root, 'target'), path.join(root, 'files'));
  assert.throws(() => new ControlledDownloads({ files: path.join(root, 'files'), authorize: () => true }));
  fs.unlinkSync(path.join(root, 'files'));
  fs.mkdirSync(path.join(root, 'files'));
  fs.symlinkSync(path.join(root, 'target'), path.join(root, 'download-staging'));
  assert.throws(() => new ControlledDownloads({ files: path.join(root, 'files'), authorize: () => true }));
});

test('duplicate active request IDs cannot replace the original transfer', async t => {
  let started;
  const ready = new Promise(resolve => { started = resolve; });
  const f = await fixture(t, () => started());
  const result = f.owner.execute('same', { url: f.origin });
  const rejected = assert.rejects(result);
  await ready;
  await assert.rejects(f.owner.execute('same', { url: f.origin }));
  f.owner.cancel('same');
  await rejected;
  assert.equal(f.requests(), 1);
});

test('credentials, non-HTTP URLs and malformed redirects never publish', async t => {
  const f = await fixture(t, (_req, res) => {
    res.writeHead(302, { Location: 'file:///tmp/forbidden' }); res.end();
  });
  for (const url of ['file:///tmp/forbidden', 'data:text/plain,hello',
    f.origin.replace('://', '://user:password@'), `${f.origin}/bad\\path`])
    await assert.rejects(f.owner.execute(url, { url }));
  assert.equal(f.requests(), 0);
  await assert.rejects(f.owner.execute('redirect', { url: f.origin }));
  assert.equal(f.requests(), 1);
  assert.deepEqual(fs.readdirSync(f.files), []);
});

test('browser-close drain fences replacement work until cleanup completes', async t => {
  const f = await fixture(t, (_req, res) => res.end('ok'));
  let release, entered;
  const ready = new Promise(resolve => { entered = resolve; });
  const cleanup = new Promise(resolve => { release = resolve; });
  const closing = f.owner.drain(async () => { entered(); await cleanup; });
  await ready;
  await assert.rejects(f.owner.execute(1, { url: f.origin }));
  release(); await closing;
  const result = await f.owner.execute(2, { url: f.origin });
  assert.equal(fs.readFileSync(result.path, 'utf8'), 'ok');
});

test('browser cookie header rechecks domain/path/expiry/secure/partition and keeps specific paths first', () => {
  const { cookieHeader } = require('./transfers.cjs');
  const base = { domain: 'example.test', path: '/', expires: -1, secure: false };
  const cookies = [
    { ...base, name: 'root', value: 'one' },
    { ...base, name: 'private', value: 'two', path: '/private' },
    { ...base, name: 'secure', value: 'three', secure: true },
    { ...base, name: 'shared', value: 'four', domain: '.example.test' },
    { ...base, name: 'wrong', value: 'never', domain: 'other.test' },
    { ...base, name: 'expired', value: 'never', expires: 99 },
    { ...base, name: 'partitioned', value: 'never', partitionKey: 'https://example.test' },
  ];
  assert.equal(cookieHeader(cookies, new URL('http://example.test/private/file'), 100), 'private=two; root=one; shared=four');
  assert.equal(cookieHeader(cookies, new URL('https://example.test/private-other'), 100), 'root=one; secure=three; shared=four');
  assert.equal(cookieHeader(cookies, new URL('https://sub.example.test/private/file'), 100), 'shared=four');
  for (const value of ['secret\r\nInjected: yes', 'bad;other=value', 'x'.repeat(16385)])
    assert.throws(() => cookieHeader([{ ...base, name: 'session', value }], new URL('https://example.test')));
});

test('only explicit browser context authentication reads cookies and publishes protected bytes', async t => {
  let lookups = 0;
  const f = await fixture(t, (req, res) => {
    if (req.headers.cookie !== 'session=fixture-token') { res.writeHead(401); res.end('denied'); return; }
    res.end('protected bytes');
  }, { getContext: async () => { lookups++; return { cookies: async url => [{
    domain: new URL(url).hostname, path: '/', expires: -1, secure: false, name: 'session', value: 'fixture-token',
  }] }; } });
  await assert.rejects(f.owner.execute(1, { url: f.origin }));
  await assert.rejects(f.owner.execute(2, { url: f.origin, use_browser_context: false }));
  assert.equal(lookups, 0);
  const result = await f.owner.execute(3, { url: f.origin, use_browser_context: true });
  assert.equal(lookups, 1);
  assert.equal(fs.readFileSync(result.path, 'utf8'), 'protected bytes');
  assert.equal(JSON.stringify(result).includes('fixture-token'), false);
});

test('denied grants and absent owned contexts never dispatch authenticated downloads', async t => {
  let lookups = 0;
  const denied = await fixture(t, (_req, res) => res.end('unexpected'), {
    authorize: () => false, getContext: async () => { lookups++; throw new Error('unreachable'); },
  });
  await assert.rejects(denied.owner.execute(1, { url: denied.origin, use_browser_context: true }));
  assert.equal(lookups, 0); assert.equal(denied.requests(), 0);
  const absent = await fixture(t, (_req, res) => res.end('unexpected'));
  await assert.rejects(absent.owner.execute(2, { url: absent.origin, use_browser_context: true }));
  assert.equal(absent.requests(), 0);
  await assert.rejects(absent.owner.execute(3, { url: absent.origin, use_browser_context: 'true' }));
});

test('authenticated redirect recomputes cookies per destination and carries no Authorization header', async t => {
  const reads = [], received = [];
  const target = await fixture(t, (req, res) => { received.push(req.headers); res.end('target'); });
  const source = await fixture(t, (req, res) => {
    received.push(req.headers); res.writeHead(302, { Location: target.origin }); res.end();
  }, {
    authorize: () => true,
    getContext: async () => ({ cookies: async value => {
      const url = new URL(value); reads.push(url.origin);
      return [{ domain: url.hostname, path: '/', expires: -1, secure: false,
        name: 'session', value: url.origin === target.origin ? 'target-token' : 'source-token' }];
    } }),
  });
  await source.owner.execute(1, { url: source.origin, use_browser_context: true });
  assert.deepEqual(reads, [source.origin, target.origin]);
  assert.equal(received[0].cookie, 'session=source-token');
  assert.equal(received[1].cookie, 'session=target-token');
  assert.ok(received.every(headers => headers.authorization === undefined));
});

test('cancellation while credential lookup is pending drains without dispatch or late replay', async t => {
  let resolveRead, began;
  const ready = new Promise(resolve => { began = resolve; });
  const read = new Promise(resolve => { resolveRead = resolve; });
  const f = await fixture(t, (_req, res) => res.end('unexpected'), { getContext: async () => ({
    cookies: () => { began(); return read; },
  }) });
  const pending = f.owner.execute(1, { url: f.origin, use_browser_context: true });
  const rejected = assert.rejects(pending);
  await ready;
  await f.owner.close();
  await rejected;
  resolveRead([{ domain: '127.0.0.1', path: '/', expires: -1, name: 'session', value: 'late-token' }]);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(f.requests(), 0); assert.equal(f.owner.jobs.size, 0);
  assert.deepEqual(fs.readdirSync(f.files), []);
});

test('cancellation before an existing-context lookup completes cannot read cookies later', async t => {
  let release, began, cookieReads = 0;
  const ready = new Promise(resolve => { began = resolve; });
  const context = new Promise(resolve => { release = resolve; });
  const f = await fixture(t, (_req, res) => res.end('unexpected'), { getContext: () => { began(); return context; } });
  const pending = f.owner.execute(1, { url: f.origin, use_browser_context: true });
  const rejected = assert.rejects(pending);
  await ready; await f.owner.close(); await rejected;
  release({ cookies: async () => { cookieReads++; return []; } });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(cookieReads, 0); assert.equal(f.requests(), 0);
});

test('response synchronization requires explicit valid authentication flags before dispatch', async t => {
  const f = await fixture(t, (_req, res) => res.end('unexpected'));
  for (const args of [
    { url: f.origin, sync_response_cookies: true },
    { url: f.origin, use_browser_context: false, sync_response_cookies: true },
    { url: f.origin, use_browser_context: true, sync_response_cookies: 'true' },
  ]) await assert.rejects(f.owner.execute(JSON.stringify(args), args));
  assert.equal(f.requests(), 0);
});

test('read-only cookie mode stays read-only and failed native sync closes the captured context without publishing or retrying', async t => {
  let pages = 0, closed = 0, captured;
  const context = {
    async cookies() { return []; },
    async newPage() { pages++; return { async route() {}, async goto() { throw new Error('uncertain navigation'); }, async close() {} }; },
  };
  const f = await fixture(t, (_req, res) => { res.setHeader('Set-Cookie', 'session=fixture; HttpOnly'); res.end('body'); }, {
    getContext: async () => context, closeContext: async value => { captured = value; closed++; },
  });
  const saved = await f.owner.execute(1, { url: f.origin, use_browser_context: true });
  assert.equal(fs.readFileSync(saved.path, 'utf8'), 'body');
  assert.equal(pages, 0); assert.equal(closed, 0);
  await assert.rejects(f.owner.execute(2, { url: f.origin, use_browser_context: true, sync_response_cookies: true }));
  assert.equal(pages, 1); assert.equal(closed, 1); assert.equal(captured, context);
  assert.equal(f.requests(), 2); assert.equal(fs.readdirSync(f.files).length, 1);
});
