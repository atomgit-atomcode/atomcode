'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const { once } = require('node:events');
const { stageFile } = require('./upload-store.cjs');
const { ControlledUploads } = require('./uploads.cjs');
async function fixture(t, handler) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-upload-wire-')), directory = path.join(root, 'uploads');
  const file = path.join(root, 'fixture.bin'), content = Buffer.from('UPLOAD_FIXTURE\0BINARY\xff', 'latin1'); fs.writeFileSync(file, content);
  const selected = await stageFile(directory, file), server = http.createServer(handler);
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const origin = `http://127.0.0.1:${server.address().port}`;
  const uploads = new ControlledUploads({ directory, authorize: target => target === origin });
  t.after(async () => { await uploads.drain(undefined, true); server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); fs.rmSync(root, { recursive: true, force: true }); });
  const args = { url: origin + '/upload', artifact_id: selected.artifact_id, name: selected.name, bytes: selected.bytes, sha256: selected.sha256 };
  return { root, directory, file, content, selected, server, origin, uploads, args };
}
test('one HTTP multipart request sends exact selected bytes with visible identity and no credentials/body reflection', async t => {
  let requests = 0, body, headers, method;
  const f = await fixture(t, (req, res) => {
    requests++; headers = req.headers; method = req.method; const chunks = [];
    req.on('data', chunk => chunks.push(chunk)); req.on('end', () => { body = Buffer.concat(chunks); res.end('MUST_NOT_RETURN_RESPONSE_BODY'); });
  });
  fs.writeFileSync(f.file, 'CHANGED_ORIGINAL');
  const result = await f.uploads.execute(1, { ...f.args, field_name: 'attachments[]' });
  assert.equal(requests, 1); assert.equal(method, 'POST'); assert.equal(result.status, 200);
  assert.equal(JSON.stringify(result).includes('MUST_NOT_RETURN_RESPONSE_BODY'), false);
  assert.equal(headers.cookie, undefined); assert.equal(headers.authorization, undefined);
  const boundary = headers['content-type'].split('boundary=')[1];
  const prefix = Buffer.from(`--${boundary}\r\nContent-Disposition: form-data; name="attachments[]"; filename="fixture.bin"\r\nContent-Type: application/octet-stream\r\n\r\n`);
  assert.deepEqual(body, Buffer.concat([prefix, f.content, Buffer.from(`\r\n--${boundary}--\r\n`)]));
  assert.equal(Number(headers['content-length']), body.length);
  assert.equal(result.sha256, f.selected.sha256); assert.equal(result.bytes, f.content.length);
});
test('denied URLs, identity mismatches, arbitrary paths/headers and bad fields cause zero requests', async t => {
  let requests = 0; const f = await fixture(t, (_req, res) => { requests++; res.end(); });
  for (const patch of [{ url: 'http://denied.test/' }, { url: 'file:///tmp/file' }, { url: f.origin.replace('http://', 'http://user:password@') },
    { file_path: f.file }, { headers: { Authorization: 'forbidden' } }, { name: 'other' }, { bytes: -1 }, { sha256: '0'.repeat(64) },
    { artifact_id: '../fixture.bin' }, { field_name: 'file\r\ninjected' }]) await assert.rejects(f.uploads.execute(1, { ...f.args, ...patch }));
  assert.equal(requests, 0);
});
test('redirects never resubmit the uploaded file, including 307/308', async t => {
  let requests = 0; const f = await fixture(t, (req, res) => { requests++; req.resume(); res.writeHead(307, { Location: '/other' }); res.end(); });
  await assert.rejects(f.uploads.execute(1, f.args)); assert.equal(requests, 1);
});
test('cancel and drain stop held requests with one uncertain terminal and no retry', async t => {
  let requests = 0, arrived; const ready = new Promise(resolve => { arrived = resolve; });
  const f = await fixture(t, (req, res) => { requests++; req.resume(); res.write('partial'); arrived(); });
  const job = f.uploads.execute('held', f.args); const failure = assert.rejects(job); await ready;
  assert.equal(f.uploads.has('held'), true); f.uploads.cancel('held'); await failure;
  assert.equal(f.uploads.has('held'), false); assert.equal(requests, 1);
  await f.uploads.drain(undefined, true); await assert.rejects(f.uploads.execute(2, f.args));
});
test('oversized/incomplete response cannot be accepted as successful upload', async t => {
  let requests = 0; const f = await fixture(t, (req, res) => { requests++; req.resume(); res.writeHead(200, { 'Content-Length': 9999999 }); res.end('short'); });
  await assert.rejects(f.uploads.execute(1, f.args)); assert.equal(requests, 1);
});

test('concurrency is bounded before snapshot loading and drain settles both active uploads', async t => {
  let requests = 0, arrived; const ready = new Promise(resolve => { arrived = resolve; });
  const f = await fixture(t, (req, res) => { requests++; req.resume(); res.write('partial'); if (requests === 2) arrived(); });
  const first = f.uploads.execute('first', f.args), second = f.uploads.execute('second', f.args);
  const failures = [assert.rejects(first), assert.rejects(second)];
  await ready; await assert.rejects(f.uploads.execute('third', f.args));
  await f.uploads.drain(); await Promise.all(failures);
  assert.equal(requests, 2); assert.equal(f.uploads.jobs.size, 0);
});
test('the configured job deadline rejects a withheld response without leaving an active job', async t => {
  const f = await fixture(t, (req, res) => { req.resume(); res.write('partial'); });
  f.uploads.timeoutMs = 20;
  await assert.rejects(f.uploads.execute('deadline', f.args));
  assert.equal(f.uploads.jobs.size, 0);
});
