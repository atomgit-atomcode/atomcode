#!/usr/bin/env node
'use strict';
// Opt-in real stdio MCP acceptance. All uploaded bytes are disposable fixtures.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const https = require('node:https');
const readline = require('node:readline');
const { spawn, spawnSync } = require('node:child_process');
const { once } = require('node:events');
const { createHash } = require('node:crypto');
const { prepareOutput, loadBackend } = require('./backend.cjs');
const { stageFile } = require('./upload-store.cjs');
const { CAPABILITY, POLICY_ENV } = require('./policy.cjs');
async function verify(root, output) {
  assert.ok(path.isAbsolute(root) && path.isAbsolute(output)); assert.equal(fs.existsSync(output), false);
  const external = loadBackend(root), temp = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-upload-mcp-'));
  const report = { passed: false, platform: process.platform, source_hashes: external.sourceHashes, cases: [], integration_hashes: {} };
  for (const name of ['backend.cjs', 'policy.cjs', 'uploads.cjs', 'upload-store.cjs', 'verify-uploads.cjs'])
    report.integration_hashes[name] = createHash('sha256').update(fs.readFileSync(path.join(__dirname, name))).digest('hex');
  const artifacts = path.join(temp, 'artifacts'); prepareOutput(artifacts);
  const source = path.join(temp, 'fixture.bin'), body = Buffer.from('SYNTHETIC_UPLOAD_MCP_FIXTURE\0\xff', 'latin1'); fs.writeFileSync(source, body);
  const selected = await stageFile(path.join(artifacts, 'uploads'), source);
  function certificate(name) {
    const key = path.join(temp, name + '.key'), cert = path.join(temp, name + '.pem');
    assert.equal(spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', cert, '-days', '1',
      '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1'], { stdio: 'ignore' }).status, 0);
    return { cert, options: { key: fs.readFileSync(key), cert: fs.readFileSync(cert) } };
  }
  const trusted = certificate('trusted'), untrusted = certificate('untrusted');
  const hits = [], held = new Map(); let deniedHits = 0, badTlsHits = 0, credentials = false;
  const handler = (req, res) => {
    hits.push(req.url); credentials ||= req.headers.cookie !== undefined || req.headers.authorization !== undefined;
    const chunks = []; req.on('data', chunk => chunks.push(chunk));
    req.on('end', () => {
      const boundary = req.headers['content-type'].split('boundary=')[1];
      const expected = Buffer.concat([Buffer.from(`--${boundary}\r\nContent-Disposition: form-data; name="file"; filename="fixture.bin"\r\nContent-Type: application/octet-stream\r\n\r\n`), body, Buffer.from(`\r\n--${boundary}--\r\n`)]);
      assert.equal(req.method, 'POST'); assert.deepEqual(Buffer.concat(chunks), expected);
      if (req.url === '/redirect') { res.writeHead(308, { Location: '/must-not-resubmit' }); return res.end(); }
      if (req.url.startsWith('/held')) { res.write('partial'); held.set(req.url, true); return; }
      res.end('RESPONSE_BODY_MUST_NOT_BE_RETURNED');
    });
  };
  const allowed = http.createServer(handler), tls = https.createServer(trusted.options, handler);
  const denied = http.createServer((_req, res) => { deniedHits++; res.end(); });
  const badTls = https.createServer(untrusted.options, (_req, res) => { badTlsHits++; res.end(); });
  for (const server of [allowed, tls, denied, badTls]) { server.listen(0, '127.0.0.1'); await once(server, 'listening'); }
  const origin = `http://127.0.0.1:${allowed.address().port}`, secure = `https://127.0.0.1:${tls.address().port}`;
  const forbidden = `http://127.0.0.1:${denied.address().port}`, badSecure = `https://127.0.0.1:${badTls.address().port}`;
  let child, exited, stderr = '', next = 0; const pending = new Map();
  const send = message => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...message }) + '\n');
  function request(method, params) {
    const id = ++next; const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pending.delete(id); reject(new Error('MCP acceptance deadline')); }, 20000);
      pending.set(id, { resolve: value => { clearTimeout(timer); resolve(value); } });
    }); send({ id, method, params }); return { id, promise };
  }
  const args = url => ({ url, artifact_id: selected.artifact_id, name: selected.name, bytes: selected.bytes, sha256: selected.sha256 });
  const call = value => request('tools/call', { name: 'browser_upload', arguments: value });
  async function observed(name) {
    const deadline = Date.now() + 5000;
    while (!held.has(name)) { assert.ok(Date.now() < deadline); await new Promise(resolve => setTimeout(resolve, 10)); }
  }
  try {
    const policy = { version: 3, origins: {
      [origin]: { access: 'allow', uploads: 'allow' }, [secure]: { access: 'allow', uploads: 'allow' },
      [badSecure]: { access: 'allow', uploads: 'allow' }, [forbidden]: { access: 'allow', uploads: 'deny' },
    } };
    child = spawn(process.execPath, [path.join(__dirname, 'backend.cjs'), '--backend-root', root, '--output-dir', artifacts], {
      env: { ...process.env, NODE_EXTRA_CA_CERTS: trusted.cert, [POLICY_ENV]: JSON.stringify(policy) }, stdio: ['pipe', 'pipe', 'pipe'],
    }); exited = once(child, 'exit'); child.stderr.on('data', chunk => { stderr += chunk; }); child.stdin.on('error', () => {});
    readline.createInterface({ input: child.stdout }).on('line', line => {
      const message = JSON.parse(line), waiter = pending.get(message.id);
      if (waiter && !message.method) { pending.delete(message.id); waiter.resolve(message); }
    });
    const init = await request('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'upload-fixture', version: '1' } }).promise;
    assert.equal(init.result.capabilities.experimental[CAPABILITY].version, 3); send({ method: 'notifications/initialized' });
    const tools = (await request('tools/list', {}).promise).result.tools;
    assert.ok(tools.some(tool => tool.name === 'browser_upload')); assert.ok(!tools.some(tool => ['browser_file_upload', 'browser_cdp', 'browser_download'].includes(tool.name)));
    report.cases.push('actual v3 capability/discovery exposes only granted controlled uploads; file-input/CDP/download tools absent');
    const uploaded = await call(args(origin + '/upload')).promise; assert.equal(uploaded.result.isError, undefined);
    const result = JSON.parse(uploaded.result.content[0].text); assert.equal(result.sha256, selected.sha256); assert.equal(result.bytes, body.length); assert.equal(result.status, 200);
    assert.equal(JSON.stringify(uploaded).includes('RESPONSE_BODY_MUST_NOT_BE_RETURNED'), false);
    assert.equal((await call(args(secure + '/tls-upload')).promise).result.isError, undefined);
    assert.equal((await call(args(badSecure + '/untrusted')).promise).result.isError, true); assert.equal(badTlsHits, 0);
    report.cases.push('exact HTTP/HTTPS multipart bytes and selected identity; generated fixture CA trusted, other certificate refused before HTTP');
    assert.equal((await call(args(forbidden + '/denied')).promise).result.isError, true); assert.equal(deniedHits, 0);
    assert.equal((await call({ ...args(origin + '/bad'), name: 'different' }).promise).result.isError, true);
    assert.equal((await call({ ...args(origin + '/bad'), file_path: source }).promise).result.isError, true);
    assert.ok(!hits.includes('/bad')); report.cases.push('upload-denied access-allowed origin and mismatched file identity/path arguments cause zero dispatch');
    assert.equal((await call(args(origin + '/redirect')).promise).result.isError, true); assert.ok(!hits.includes('/must-not-resubmit'));
    report.cases.push('HTTP 308 is refused after one POST with no automatic resubmission');
    const cancel = call(args(origin + '/held-cancel')); await observed('/held-cancel');
    send({ method: 'notifications/cancelled', params: { requestId: cancel.id } }); assert.equal((await cancel.promise).result.isError, true);
    assert.equal(hits.filter(hit => hit === '/held-cancel').length, 1); report.cases.push('actual MCP cancellation settles held upload once without replay');
    const processRows = spawnSync('ps', ['-axo', 'pid=,ppid='], { encoding: 'utf8' });
    assert.equal(processRows.status, 0);
    assert.ok(!processRows.stdout.trim().split('\n').some(line => Number(line.trim().split(/\s+/)[1]) === child.pid), 'controlled HTTP uploads must not launch a browser or helper');
    report.backend_pid = child.pid;
    const eof = call(args(origin + '/held-eof')); await observed('/held-eof'); child.stdin.end();
    let deadline;
    const terminal = await Promise.race([exited, new Promise((_, reject) => { deadline = setTimeout(() => reject(new Error('EOF deadline')), 7000); })]).finally(() => clearTimeout(deadline));
    assert.equal(terminal[0], 0); report.backend_exited = true; assert.equal(hits.filter(hit => hit === '/held-eof').length, 1);
    pending.get(eof.id)?.resolve({ error: true }); pending.delete(eof.id);
    assert.equal(credentials, false); assert.equal(stderr, ''); report.cases.push('pure stdin EOF drains upload, exits zero and sends no Cookie/Authorization');
    report.requests = hits; report.denied_requests = deniedHits; report.untrusted_tls_http_requests = badTlsHits; report.stderr_bytes = Buffer.byteLength(stderr); report.passed = true;
  } finally {
    if (child && child.exitCode === null && child.signalCode === null) { child.kill('SIGTERM'); await exited; }
    for (const waiter of pending.values()) waiter.resolve({ error: true }); pending.clear();
    for (const server of [allowed, tls, denied, badTls]) { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
    fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n', { flag: 'wx' }); fs.rmSync(temp, { recursive: true, force: true });
  }
  assert.equal(report.passed, true); console.log(JSON.stringify(report));
}
if (require.main === module) {
  const args = process.argv.slice(2); assert.equal(args.length, 4); assert.equal(args[0], '--backend-root'); assert.equal(args[2], '--report');
  verify(args[1], args[3]).catch(error => { console.error(error); process.exitCode = 1; });
}
