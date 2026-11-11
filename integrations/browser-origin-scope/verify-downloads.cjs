#!/usr/bin/env node
'use strict';
// Opt-in isolated MCP/Chrome acceptance; no model, credentials or personal profile.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const readline = require('node:readline');
const { spawn, spawnSync } = require('node:child_process');
const { once } = require('node:events');
const { createHash } = require('node:crypto');
const { loadBackend } = require('./backend.cjs');
const { CAPABILITY, POLICY_ENV } = require('./policy.cjs');

async function until(predicate) {
  const deadline = Date.now() + 10000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error('Acceptance deadline');
    await new Promise(resolve => setTimeout(resolve, 20));
  }
}
function alive(pid) { try { process.kill(pid, 0); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; } }
function tree(pid) {
  const rows = spawnSync('ps', ['-ax', '-o', 'pid=,ppid='], { encoding: 'utf8' });
  assert.equal(rows.status, 0);
  const pairs = rows.stdout.trim().split('\n').map(line => line.trim().split(/\s+/).map(Number));
  const ids = new Set([pid]);
  for (let i = 0; i < pairs.length; i++) for (const [child, parent] of pairs) if (ids.has(parent)) ids.add(child);
  return [...ids];
}
async function verify(root, reportPath) {
  assert.notEqual(process.platform, 'win32', 'Process acceptance currently uses ps');
  assert.equal(fs.existsSync(reportPath), false, 'report path must be new');
  const external = loadBackend(root);
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-download-wire-'));
  const report = { passed: false, platform: process.platform, node: process.version, source_hashes: external.sourceHashes, cases: [] };
  const output = path.join(temp, 'artifacts');
  const hits = [];
  let deniedHits = 0, stderr = '', child, exit, pending = new Map(), nextId = 0;
  const denied = http.createServer((_req, res) => { deniedHits++; res.end('DENIED'); });
  denied.listen(0, '127.0.0.1'); await once(denied, 'listening');
  const bad = `http://127.0.0.1:${denied.address().port}`;
  const body = Buffer.from('CONTROLLED_DOWNLOAD_FIXTURE');
  const server = http.createServer((req, res) => {
    hits.push(req.url);
    if (req.url === '/redirect') { res.writeHead(302, { Location: bad + '/denied' }); return res.end(); }
    if (req.url.startsWith('/hold')) { res.write('partial'); return; }
    if (req.url === '/page') { res.setHeader('Content-Type', 'text/html'); return res.end('<h1>DOWNLOAD_ACCEPTANCE</h1>'); }
    res.writeHead(200, { 'Content-Disposition': 'attachment; filename="../../unsafe"' }); res.end(body);
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const good = `http://127.0.0.1:${server.address().port}`;
  const send = message => child.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...message }) + '\n');
  function request(method, params) {
    const id = ++nextId;
    const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pending.delete(id); reject(new Error('MCP request timeout')); }, 15000);
      pending.set(id, { resolve: message => { clearTimeout(timer); resolve(message); } });
    });
    send({ id, method, params });
    return { id, promise };
  }
  const call = (name, args = {}) => request('tools/call', { name, arguments: args }).promise;
  try {
    child = spawn(process.execPath, [path.join(__dirname, 'backend.cjs'), '--backend-root', root, '--output-dir', output], {
      env: { ...process.env, [POLICY_ENV]: JSON.stringify({ version: 2, origins: {
        [good]: { access: 'allow', downloads: 'allow' }, [bad]: { access: 'allow', downloads: 'deny' },
      } }) }, stdio: ['pipe', 'pipe', 'pipe'],
    });
    exit = once(child, 'exit');
    child.stderr.on('data', chunk => { stderr += chunk; });
    child.stdin.on('error', () => {});
    readline.createInterface({ input: child.stdout }).on('line', line => {
      const message = JSON.parse(line);
      const waiter = pending.get(message.id);
      if (waiter && !message.method) { pending.delete(message.id); waiter.resolve(message); }
    });
    const initialized = await request('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'controlled-download-acceptance', version: '1' } }).promise;
    assert.equal(initialized.result.capabilities.experimental[CAPABILITY].version, 2);
    send({ method: 'notifications/initialized' });
    const tools = (await request('tools/list', {}).promise).result.tools;
    assert.equal(tools.filter(tool => tool.name === 'browser_download').length, 1);
    assert.equal(tools.find(tool => tool.name === 'browser_download').annotations.readOnlyHint, false);
    const saved = await call('browser_download', { url: good + '/file' });
    assert.equal(saved.result.isError, undefined);
    const artifact = JSON.parse(saved.result.content[0].text);
    assert.deepEqual(fs.readFileSync(artifact.path), body);
    assert.equal(artifact.sha256, createHash('sha256').update(body).digest('hex'));
    assert.equal(path.dirname(artifact.path), path.join(output, 'files'));
    report.cases.push('v2 capability, discovery and exact generated artifact');
    assert.equal((await call('browser_download', { url: good + '/redirect' })).result.isError, true);
    assert.equal((await call('browser_download', { url: bad + '/direct' })).result.isError, true);
    assert.equal(deniedHits, 0);
    assert.equal((await call('browser_download', { url: good + '/file', filename: '/tmp/unsafe' })).result.isError, true);
    report.cases.push('download-denied origin refused directly and after redirect before dispatch; caller paths refused');
    const running = request('tools/call', { name: 'browser_download', arguments: { url: good + '/hold-cancel' } });
    await until(() => hits.includes('/hold-cancel'));
    send({ method: 'notifications/cancelled', params: { requestId: running.id } });
    assert.equal((await running.promise).result.isError, true);
    assert.equal(hits.filter(value => value === '/hold-cancel').length, 1);
    report.cases.push('MCP cancellation has one error terminal and no replay');
    const navigated = await call('browser_navigate', { url: good + '/page' });
    assert.notEqual(navigated.result.isError, true);
    const captured = tree(child.pid);
    assert.ok(captured.length > 1, 'real isolated Chrome child tree');
    await call('browser_navigate', { url: good + '/implicit' });
    assert.equal(fs.readdirSync(path.join(output, 'files')).filter(name => name.startsWith('download-')).length, 1);
    report.cases.push('real Chrome navigation works while implicit attachment publication stays disabled');
    const held = request('tools/call', { name: 'browser_download', arguments: { url: good + '/hold-close' } });
    await until(() => hits.includes('/hold-close'));
    assert.notEqual((await call('browser_close')).result.isError, true);
    assert.equal((await held.promise).result.isError, true);
    await until(() => captured.slice(1).every(pid => !alive(pid)));
    assert.equal((await call('browser_download', { url: good + '/after-close' })).result.isError, undefined);
    report.cases.push('browser_close awaits transfer cancellation and Chrome cleanup; explicit subsequent transfer succeeds');
    const eof = request('tools/call', { name: 'browser_download', arguments: { url: good + '/hold-eof' } });
    await until(() => hits.includes('/hold-eof'));
    child.stdin.end();
    const exited = await Promise.race([exit, new Promise((_, reject) => setTimeout(() => reject(new Error('EOF cleanup deadline')), 7000))]);
    assert.equal(exited[0], 0);
    // EOF may prevent a terminal delivery; no replay and no publication is authoritative.
    pending.get(eof.id)?.resolve({ result: { isError: true } }); pending.delete(eof.id);
    assert.ok(captured.every(pid => !alive(pid)));
    assert.equal(fs.readdirSync(path.join(output, 'files')).filter(name => name.startsWith('download-')).length, 2);
    assert.deepEqual(fs.readdirSync(path.join(output, 'download-staging')), []);
    assert.equal(stderr, '');
    report.cases.push('pure stdin EOF drains transfer and exits zero, without signal fallback or staged remnants');
    report.denied_requests = deniedHits;
    report.owned_pids = captured;
    report.stderr_bytes = Buffer.byteLength(stderr);
    report.passed = true;
  } finally {
    if (child && child.exitCode === null && child.signalCode === null) { child.kill('SIGTERM'); await exit; }
    for (const waiter of pending.values()) waiter.resolve({ error: { message: 'acceptance cleanup' } });
    server.closeAllConnections(); denied.closeAllConnections();
    await Promise.all([new Promise(resolve => server.close(resolve)), new Promise(resolve => denied.close(resolve))]);
    fs.writeFileSync(reportPath, JSON.stringify(report, null, 2) + '\n', { flag: 'wx' });
    fs.rmSync(temp, { recursive: true, force: true });
  }
  console.log(JSON.stringify(report));
}
if (require.main === module) {
  const args = process.argv.slice(2);
  assert.equal(args.length, 4); assert.equal(args[0], '--backend-root'); assert.equal(args[2], '--report');
  assert.ok(path.isAbsolute(args[1]) && path.isAbsolute(args[3]));
  verify(args[1], args[3]).catch(error => { console.error(error); process.exitCode = 1; });
}
