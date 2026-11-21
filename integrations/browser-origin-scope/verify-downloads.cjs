#!/usr/bin/env node
'use strict';
// Opt-in isolated MCP/Chrome acceptance; no model, credentials or personal profile.
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
  function certificate(name) {
    const key = path.join(temp, name + '.key'), cert = path.join(temp, name + '.pem');
    assert.equal(spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key,
      '-out', cert, '-days', '1', '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1'], { stdio: 'ignore' }).status, 0);
    return { cert, options: { key: fs.readFileSync(key), cert: fs.readFileSync(cert) } };
  }
  const trusted = certificate('trusted-fixture'), untrusted = certificate('untrusted-fixture');
  let secureRequests = 0, untrustedRequests = 0, unexpectedAuthorization = false, plaintextSecureCookieSeen = false;
  const secureHits = [];
  const tls = https.createServer(trusted.options, (req, res) => {
    secureRequests++; secureHits.push(req.url);
    unexpectedAuthorization ||= req.headers.authorization !== undefined;
    if (req.url === '/partition/final' || req.url === '/partition/delete') {
      if (req.headers.cookie !== 'partitionSession=rotated-fixture') { res.writeHead(401); return res.end('PARTITION_AUTH_REQUIRED'); }
      if (req.url === '/partition/delete') res.setHeader('Set-Cookie', 'partitionSession=deleted; Secure; HttpOnly; Partitioned; Path=/partition/; Max-Age=0');
      return res.end(req.url === '/partition/final' ? 'PARTITION_ROTATION_CONFIRMED' : 'PARTITION_DELETION_CONFIRMED');
    }
    if (req.url === '/partition/absent') {
      if (req.headers.cookie !== undefined) { res.writeHead(409); return res.end('PARTITION_COOKIE_REMAINS'); }
      return res.end('PARTITION_COOKIE_ABSENT');
    }
    if (req.url === '/tls/check') {
      if (req.headers.cookie !== 'secureRotation=https-fixture') { res.writeHead(401); return res.end('SECURE_COOKIE_MISSING'); }
      return res.end('TLS_COOKIE_ROTATION_CONFIRMED');
    }
    if (req.headers.cookie !== 'session=ephemeral-download-fixture') { res.writeHead(401); return res.end('AUTH_REQUIRED'); }
    unexpectedAuthorization ||= req.headers.authorization !== undefined;
    if (req.url === '/auth/partition-rotate') {
      res.writeHead(302, { 'Set-Cookie': 'partitionSession=rotated-fixture; Secure; HttpOnly; SameSite=Strict; Partitioned; Path=/partition/', Location: '/partition/final' }); return res.end();
    }
    if (req.url === '/auth/https-sync') res.setHeader('Set-Cookie', 'secureRotation=https-fixture; Secure; HttpOnly; Path=/tls/; SameSite=Strict');
    res.end('TLS_AUTHENTICATED_DOWNLOAD_FIXTURE');
  });
  const badTls = https.createServer(untrusted.options, (_req, res) => { untrustedRequests++; res.end('MUST_NOT_FETCH'); });

  const hits = [];
  let deniedHits = 0, stderr = '', child, exit, pending = new Map(), nextId = 0;
  const denied = http.createServer((_req, res) => { deniedHits++; res.end('DENIED'); });
  denied.listen(0, '127.0.0.1'); await once(denied, 'listening');
  const bad = `http://127.0.0.1:${denied.address().port}`;
  const body = Buffer.from('CONTROLLED_DOWNLOAD_FIXTURE');
  let crossOrigin, crossRequests = 0, crossHadCookie = false, crossHadAuthorization = false;
  const cross = http.createServer((req, res) => {
    crossRequests++;
    crossHadCookie ||= req.headers.cookie !== undefined;
    crossHadAuthorization ||= req.headers.authorization !== undefined;
    res.end('CROSS_HOST_DOWNLOAD_FIXTURE');
  });
  const server = http.createServer((req, res) => {
    hits.push(req.url);
    if (req.url === '/redirect') { res.writeHead(302, { Location: bad + '/denied' }); return res.end(); }
    if (req.url.startsWith('/hold')) { res.write('partial'); return; }
    if (req.url === '/login') {
      report.chrome_user_agent_version = String(req.headers['user-agent'] || '').match(/(?:HeadlessChrome|Chrome)\/([\d.]+)/)?.[1];

      res.setHeader('Set-Cookie', 'session=ephemeral-download-fixture; HttpOnly; Path=/auth/; SameSite=Strict');
      res.setHeader('Content-Type', 'text/html'); return res.end('<h1>OWNED_BROWSER_LOGIN_FIXTURE</h1>');
    }
    if (req.url === '/tls/check') {
      plaintextSecureCookieSeen ||= req.headers.cookie !== undefined; return res.end('SECURE_COOKIE_WITHHELD_FROM_HTTP');
    }
    if (req.url === '/auth/assert-logged-out') {
      res.setHeader('Content-Type', 'text/html');
      return res.end(req.headers.cookie === undefined ? '<h1>COOKIE_LOGGED_OUT</h1>' : '<h1>COOKIE_STILL_PRESENT</h1>');
    }
    if (req.url.startsWith('/auth/')) {
      if (!['session=ephemeral-download-fixture', 'session=rotated-sync-fixture', 'session=denied-sync-fixture', 'session=readonly-fixture'].includes(req.headers.cookie)) { res.writeHead(401); return res.end('AUTH_REQUIRED'); }
      if (req.url === '/auth/partitioned-response') {
        res.setHeader('Set-Cookie', 'session=partitioned-fixture; Secure; SameSite=None; Partitioned; Path=/auth/'); return res.end('PARTITIONED_UNSUPPORTED');
      }
      if (req.url === '/auth/assert-original') {
        res.setHeader('Content-Type', 'text/html'); return res.end(req.headers.cookie === 'session=ephemeral-download-fixture' ? '<h1>ORIGINAL_COOKIE_RETAINED</h1>' : '<h1>ORIGINAL_COOKIE_LOST</h1>');
      }
      if (req.url === '/auth/readonly-rotate') {
        res.writeHead(302, { 'Set-Cookie': 'session=readonly-fixture; Path=/auth/; HttpOnly', Location: '/auth/readonly-final' }); return res.end();
      }
      if (req.url === '/auth/readonly-final') {
        if (req.headers.cookie === 'session=readonly-fixture') return res.end('READ_ONLY_COOKIE_WAS_APPLIED');
        res.writeHead(401); return res.end('READ_ONLY_COOKIE_WAS_NOT_APPLIED');
      }
      if (req.url === '/auth/sync-rotate') {
        res.writeHead(302, { 'Set-Cookie': 'session=rotated-sync-fixture; Path=/auth/; HttpOnly; SameSite=Strict', Location: '/auth/sync-final' }); return res.end();
      }
      if (req.url === '/auth/sync-final') {
        if (req.headers.cookie !== 'session=rotated-sync-fixture') { res.writeHead(401); return res.end('COOKIE_NOT_ROTATED'); }
        return res.end('STATEFUL_DOWNLOAD_FIXTURE');
      }
      if (req.url === '/auth/sync-delete') {
        res.setHeader('Set-Cookie', 'session=deleted; Path=/auth/; Max-Age=0; HttpOnly'); return res.end('COOKIE_DELETION_FIXTURE');
      }
      if (req.url === '/auth/sync-denied') {
        res.writeHead(302, { 'Set-Cookie': 'session=denied-sync-fixture; Path=/auth/; HttpOnly', Location: bad + '/auth/forbidden' }); return res.end();
      }
      if (req.url === '/auth/assert-rotated' || req.url === '/auth/assert-denied-rotated') {
        const expected = req.url === '/auth/assert-rotated' ? 'session=rotated-sync-fixture' : 'session=denied-sync-fixture';
        res.setHeader('Content-Type', 'text/html'); return res.end(req.headers.cookie === expected ? '<h1>COOKIE_ROTATED_IN_BROWSER</h1>' : '<h1>COOKIE_NOT_ROTATED</h1>');
      }
      unexpectedAuthorization ||= req.headers.authorization !== undefined;
      if (req.url === '/auth/redirect') { res.writeHead(302, { Location: bad + '/auth/stolen' }); return res.end(); }
      if (req.url === '/auth/cross') { res.writeHead(302, { Location: crossOrigin + '/auth/target' }); return res.end(); }
      if (req.url.startsWith('/auth/hold')) { res.write('partial'); return; }
      return res.end('AUTHENTICATED_DOWNLOAD_FIXTURE');
    }
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
    tls.listen(0, '127.0.0.1'); await once(tls, 'listening');
    badTls.listen(0, '127.0.0.1'); await once(badTls, 'listening');
    const secureOrigin = `https://127.0.0.1:${tls.address().port}`;
    const untrustedOrigin = `https://127.0.0.1:${badTls.address().port}`;
    cross.listen(0, '::1'); await once(cross, 'listening');
    crossOrigin = `http://[::1]:${cross.address().port}`;
    child = spawn(process.execPath, [path.join(__dirname, 'backend.cjs'), '--backend-root', root, '--output-dir', output], {
      env: { ...process.env, NODE_EXTRA_CA_CERTS: trusted.cert, [POLICY_ENV]: JSON.stringify({ version: 2, origins: {
        [secureOrigin]: { access: 'allow', downloads: 'allow' }, [untrustedOrigin]: { access: 'allow', downloads: 'allow' },
        [good]: { access: 'allow', downloads: 'allow' }, [crossOrigin]: { access: 'allow', downloads: 'allow' }, [bad]: { access: 'allow', downloads: 'deny' },
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
    assert.equal((await call('browser_download', { url: good + '/auth/file', use_browser_context: true })).result.isError, true);
    assert.equal(tree(child.pid).length, 1, 'authentication never launches a missing browser');
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
    assert.notEqual((await call('browser_navigate', { url: good + '/login' })).result.isError, true);
    assert.ok(report.chrome_user_agent_version, 'actual Chrome login request');
    assert.equal((await call('browser_download', { url: good + '/auth/file' })).result.isError, true);
    const authenticated = await call('browser_download', { url: good + '/auth/file', use_browser_context: true });
    assert.equal(authenticated.result.isError, undefined);
    const protectedArtifact = JSON.parse(authenticated.result.content[0].text);
    assert.equal(fs.readFileSync(protectedArtifact.path, 'utf8'), 'AUTHENTICATED_DOWNLOAD_FIXTURE');
    assert.equal(JSON.stringify(authenticated).includes('ephemeral-download-fixture'), false);
    assert.equal((await call('browser_download', { url: good + '/auth/redirect', use_browser_context: true })).result.isError, true);
    assert.equal(deniedHits, 0);
    const crossed = await call('browser_download', { url: good + '/auth/cross', use_browser_context: true });
    assert.equal(crossed.result.isError, undefined);
    assert.equal(fs.readFileSync(JSON.parse(crossed.result.content[0].text).path, 'utf8'), 'CROSS_HOST_DOWNLOAD_FIXTURE');
    assert.equal(crossRequests, 1);
    assert.equal(crossHadCookie, false); assert.equal(crossHadAuthorization, false);
    report.cases.push('explicit HttpOnly browser-cookie authentication succeeds; default remains unauthenticated, denied redirect never dispatches, authorized other host receives no source cookie or Authorization header');
    const authHeld = request('tools/call', { name: 'browser_download', arguments: { url: good + '/auth/hold', use_browser_context: true } });
    await until(() => hits.includes('/auth/hold'));
    send({ method: 'notifications/cancelled', params: { requestId: authHeld.id } });
    assert.equal((await authHeld.promise).result.isError, true);
    assert.equal(hits.filter(value => value === '/auth/hold').length, 1);
    report.cases.push('authenticated in-flight cancellation has an error terminal without replay or publication');
    const secureDownload = await call('browser_download', { url: secureOrigin + '/auth/file', use_browser_context: true });
    assert.equal(secureDownload.result.isError, undefined);
    assert.equal(fs.readFileSync(JSON.parse(secureDownload.result.content[0].text).path, 'utf8'), 'TLS_AUTHENTICATED_DOWNLOAD_FIXTURE');
    assert.equal((await call('browser_download', { url: untrustedOrigin + '/auth/file', use_browser_context: true })).result.isError, true);
    assert.equal(secureRequests, 1); assert.equal(untrustedRequests, 0);
    assert.equal(unexpectedAuthorization, false);
    report.cases.push('HTTPS authenticated download uses only the generated fixture CA; separately untrusted certificate is refused before HTTP despite an origin grant');
    const secureSync = await call('browser_download', { url: secureOrigin + '/auth/https-sync', use_browser_context: true, sync_response_cookies: true });
    assert.equal(secureSync.result.isError, undefined);
    const secureCheck = await call('browser_download', { url: secureOrigin + '/tls/check', use_browser_context: true });
    assert.equal(secureCheck.result.isError, undefined);
    assert.equal(fs.readFileSync(JSON.parse(secureCheck.result.content[0].text).path, 'utf8'), 'TLS_COOKIE_ROTATION_CONFIRMED');
    const plaintextCheck = await call('browser_download', { url: good + '/tls/check', use_browser_context: true });
    assert.equal(plaintextCheck.result.isError, undefined); assert.equal(plaintextSecureCookieSeen, false);
    assert.equal(secureHits.filter(hit => hit === '/auth/https-sync').length, 1);
    assert.equal(secureHits.filter(hit => hit === '/tls/check').length, 1);
    report.cases.push('HTTPS response Cookie sync retains Secure/HttpOnly/path semantics, reaches only HTTPS and never repeats the remote GET');

    const partitionTabs = (await call('browser_tabs', { action: 'list' })).result.content;
    const partitionRotated = await call('browser_download', { url: secureOrigin + '/auth/partition-rotate', use_browser_context: true, sync_response_cookies: true });
    assert.equal(partitionRotated.result.isError, undefined);
    const partitionArtifact = JSON.parse(partitionRotated.result.content[0].text);
    const partitionBody = Buffer.from('PARTITION_ROTATION_CONFIRMED');
    assert.deepEqual(fs.readFileSync(partitionArtifact.path), partitionBody);
    assert.equal(partitionArtifact.sha256, createHash('sha256').update(partitionBody).digest('hex'));
    assert.equal(JSON.stringify(partitionRotated).includes('rotated-fixture'), false);
    const partitionDeleted = await call('browser_download', { url: secureOrigin + '/partition/delete', use_browser_context: true, sync_response_cookies: true });
    assert.equal(partitionDeleted.result.isError, undefined);
    assert.equal(fs.readFileSync(JSON.parse(partitionDeleted.result.content[0].text).path, 'utf8'), 'PARTITION_DELETION_CONFIRMED');
    const partitionAbsent = await call('browser_download', { url: secureOrigin + '/partition/absent', use_browser_context: true });
    assert.equal(partitionAbsent.result.isError, undefined);
    assert.equal(fs.readFileSync(JSON.parse(partitionAbsent.result.content[0].text).path, 'utf8'), 'PARTITION_COOKIE_ABSENT');
    assert.deepEqual((await call('browser_tabs', { action: 'list' })).result.content, partitionTabs);
    for (const endpoint of ['/auth/partition-rotate', '/partition/final', '/partition/delete', '/partition/absent'])
      assert.equal(secureHits.filter(hit => hit === endpoint).length, 1, 'Partitioned MCP calls never replay a remote GET');
    report.cases.push('actual MCP HTTPS Partitioned redirect rotation, exact artifact/hash, deletion and subsequent credential absence; unchanged original tabs and one remote GET per hop');

    assert.equal((await call('browser_download', { url: good + '/auth/partitioned-response', use_browser_context: true, sync_response_cookies: true })).result.isError, true);
    await call('browser_navigate', { url: good + '/auth/assert-original' });
    assert.ok(JSON.stringify((await call('browser_snapshot')).result).includes('ORIGINAL_COOKIE_RETAINED'));
    assert.equal(hits.filter(hit => hit === '/auth/partitioned-response').length, 1);
    report.cases.push('HTTP Partitioned sync response fails before cookie mutation/publication and is not replayed');
    assert.equal((await call('browser_download', { url: good + '/auth/readonly-rotate', use_browser_context: true })).result.isError, true);
    const beforeTabs = (await call('browser_tabs', { action: 'list' })).result.content;
    const synced = await call('browser_download', { url: good + '/auth/sync-rotate', use_browser_context: true, sync_response_cookies: true });
    assert.equal(synced.result.isError, undefined);
    assert.equal(fs.readFileSync(JSON.parse(synced.result.content[0].text).path, 'utf8'), 'STATEFUL_DOWNLOAD_FIXTURE');
    assert.deepEqual((await call('browser_tabs', { action: 'list' })).result.content, beforeTabs);
    await call('browser_navigate', { url: good + '/auth/assert-rotated' });
    assert.ok(JSON.stringify((await call('browser_snapshot')).result).includes('COOKIE_ROTATED_IN_BROWSER'));
    const deleted = await call('browser_download', { url: good + '/auth/sync-delete', use_browser_context: true, sync_response_cookies: true });
    assert.equal(deleted.result.isError, undefined);
    await call('browser_navigate', { url: good + '/auth/assert-logged-out' });
    assert.ok(JSON.stringify((await call('browser_snapshot')).result).includes('COOKIE_LOGGED_OUT'));
    await call('browser_navigate', { url: good + '/login' });
    const deniedSync = await call('browser_download', { url: good + '/auth/sync-denied', use_browser_context: true, sync_response_cookies: true });
    assert.equal(deniedSync.result.isError, true); assert.equal(deniedHits, 0);
    await call('browser_navigate', { url: good + '/auth/assert-denied-rotated' });
    assert.ok(JSON.stringify((await call('browser_snapshot')).result).includes('COOKIE_ROTATED_IN_BROWSER'));
    await call('browser_navigate', { url: good + '/login' });
    for (const value of ['/auth/sync-rotate', '/auth/sync-final', '/auth/sync-delete', '/auth/sync-denied'])
      assert.equal(hits.filter(hit => hit === value).length, 1, 'no second remote cookie-sync GET');
    report.cases.push('explicit response-cookie sync rotates a redirect session, updates original browser state and deletes it, with unchanged tab list and one remote GET per hop; read-only default stays read-only');
    report.cases.push('a refused redirect can retain its authorized source cookie effect; no forbidden request and no automatic replay');



    const held = request('tools/call', { name: 'browser_download', arguments: { url: good + '/auth/hold-close', use_browser_context: true } });
    await until(() => hits.includes('/auth/hold-close'));
    assert.notEqual((await call('browser_close')).result.isError, true);
    assert.equal((await held.promise).result.isError, true);
    await until(() => captured.slice(1).every(pid => !alive(pid)));
    assert.equal((await call('browser_download', { url: good + '/auth/file', use_browser_context: true })).result.isError, true);
    assert.equal(tree(child.pid).length, 1, 'closed authentication cannot reopen a browser');
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
    assert.equal(fs.readdirSync(path.join(output, 'files')).filter(name => name.startsWith('download-')).length, 13);
    assert.deepEqual(fs.readdirSync(path.join(output, 'download-staging')), []);
    assert.equal(stderr, '');
    report.cases.push('pure stdin EOF drains transfer and exits zero, without signal fallback or staged remnants');
    report.denied_requests = deniedHits;
    report.untrusted_tls_http_requests = untrustedRequests;
    report.cross_host_cookie_seen = crossHadCookie;
    report.secure_cookie_over_plaintext_seen = plaintextSecureCookieSeen;
    report.authorization_header_seen = unexpectedAuthorization || crossHadAuthorization;
    report.owned_pids = captured;
    report.stderr_bytes = Buffer.byteLength(stderr);
    report.completed_at = new Date().toISOString();
    report.passed = true;
  } finally {
    if (child && child.exitCode === null && child.signalCode === null) { child.kill('SIGTERM'); await exit; }
    for (const waiter of pending.values()) waiter.resolve({ error: { message: 'acceptance cleanup' } });
    server.closeAllConnections(); denied.closeAllConnections(); cross.closeAllConnections(); tls.closeAllConnections(); badTls.closeAllConnections();
    await Promise.all([new Promise(resolve => server.close(resolve)), new Promise(resolve => denied.close(resolve)), new Promise(resolve => cross.close(resolve)), new Promise(resolve => tls.close(resolve)), new Promise(resolve => badTls.close(resolve))]);
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
