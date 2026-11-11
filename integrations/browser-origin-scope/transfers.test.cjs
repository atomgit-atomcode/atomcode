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
