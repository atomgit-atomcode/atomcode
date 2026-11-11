#!/usr/bin/env node
'use strict';
// Explicit opt-in controlled Chrome acceptance. No personal profile is opened.
const assert = require('node:assert/strict');
const http = require('node:http');
const https = require('node:https');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { once } = require('node:events');
const { createRequire } = require('node:module');
const { loadBackend, ownedBrowser } = require('./backend.cjs');
const { CAPABILITY, POLICY_ENV, TOOLS } = require('./policy.cjs');

async function listen(server) { server.listen(0, '127.0.0.1'); await once(server, 'listening'); return server.address().port; }
function close(server) { server.closeAllConnections?.(); return new Promise(resolve => server.close(resolve)); }
async function waitFor(test, label) {
  const until = Date.now() + 10000;
  while (Date.now() < until) { if (await test()) return; await new Promise(resolve => setTimeout(resolve, 25)); }
  throw new Error(`Timed out waiting for ${label}`);
}
function alive(pid) { try { process.kill(pid, 0); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; } }
function descendants(pid) {
  if (process.platform === 'win32') return []; // Target-process acceptance is not claimed on Windows here.
  const result = spawnSync('ps', ['-ax', '-o', 'pid=,ppid='], { encoding: 'utf8' });
  assert.equal(result.status, 0);
  const pairs = result.stdout.trim().split('\n').map(line => line.trim().split(/\s+/).map(Number));
  const found = new Set([pid]);
  for (let i = 0; i < pairs.length; i++) {
    let changed = false;
    for (const [child, parent] of pairs) if (found.has(parent) && !found.has(child)) { found.add(child); changed = true; }
    if (!changed) break;
  }
  found.delete(pid); return [...found];
}

async function verify(root, reportPath) {
  const external = loadBackend(root);
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-origin-'));
  const sockets = new Set(), allowedHits = [], deniedHits = [];
  let deniedConnections = 0;
  const report = { backend: '0.0.85', playwright: '1.66.0-alpha-2026-11-09', node: process.version,
    platform: process.platform, source_hashes: external.sourceHashes, cases: [], passed: false };
  let owner, client, transport;
  const key = path.join(temp, 'key.pem'), cert = path.join(temp, 'cert.pem');
  const generated = spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key,
    '-out', cert, '-days', '1', '-subj', '/CN=localhost'], { stdio: 'ignore' });
  assert.equal(generated.status, 0, 'test certificate generation');
  const certificate = new crypto.X509Certificate(fs.readFileSync(cert));
  const pin = crypto.createHash('sha256').update(certificate.publicKey.export({type:'spki',format:'der'})).digest('base64');
  const denied = http.createServer((req, res) => { deniedHits.push(req.url); res.end('DENIED SERVER MUST NOT BE REACHED'); });
  denied.on('connection', () => { deniedConnections++; });
  denied.on('upgrade', (req, socket) => { deniedHits.push(req.url); socket.destroy(); });
  const deniedPort = await listen(denied), bad = `http://127.0.0.1:${deniedPort}`;
  let good, secure;
  function handler(req, res) {
    allowedHits.push(req.url);
    if (req.url === '/redirect' || req.url === '/redirect-307') { res.writeHead(req.url === '/redirect' ? 302 : 307, { Location: bad + '/redirected' }); return res.end(); }
    if (req.url === '/redirect-secure') { res.writeHead(302, {Location: bad.replace('http:', 'https:') + '/redirected-secure'}); return res.end(); }
    if (req.url === '/chain') { res.writeHead(302, { Location: '/redirect' }); return res.end(); }
    if (req.url === '/download') { res.writeHead(200, {'Content-Disposition':'attachment; filename=forbidden-download.txt'}); return res.end('DO NOT SAVE'); }
    if (req.url === '/nested-frame') { res.setHeader('Content-Type','text/html'); return res.end(`<iframe src="${bad}/nested-frame-denied"></iframe>`); }
    if (req.url === '/fixture') {
      res.setHeader('Content-Type', 'text/html');
      return res.end(`<!doctype html><title>Origin scope fixture</title><h1>ALLOWED_ORIGIN_FIXTURE</h1>
        <img src="${bad}/image"><iframe src="${bad}/frame"></iframe><iframe src="/nested-frame"></iframe>
        <form id="form" method="POST" action="${bad}/form" target="sink"><input name="marker" value="fixture"></form><iframe name="sink"></iframe>
        <script>
          window.completed = false;
          Promise.allSettled([
            fetch('${bad}/fetch', {mode:'no-cors'}),
            fetch('https://127.0.0.1:${deniedPort}/secure-denied'),
            fetch('/allowed-fetch'),
            new Promise(resolve => { const ws = new WebSocket('ws://127.0.0.1:${deniedPort}/denied-socket'); ws.onerror=resolve; ws.onopen=()=>{ws.close();resolve();}; }),
            new Promise(resolve => { const ws = new WebSocket('ws://127.0.0.1:'+location.port+'/allowed-socket'); ws.onmessage=e=>{window.allowedWebSocket=e.data;ws.close();resolve();}; ws.onerror=()=>{window.allowedWebSocket='error';resolve();}; })
          ]).then(()=>window.completed=true);
          document.getElementById('form').submit();
          navigator.serviceWorker.register('/worker.js').then(registration=>window.worker=registration?'unexpected':'blocked',()=>window.worker='blocked');
        </script>`);
    }
    res.setHeader('Content-Type', 'text/html'); res.end('<h1>ALLOWED_ORIGIN_FIXTURE</h1>');
  }
  const allowed = http.createServer(handler);
  function websocketFixture(req, socket) {
    allowedHits.push(req.url); sockets.add(socket); socket.on('error',()=>{}); socket.on('close',()=>sockets.delete(socket));
    const accept = crypto.createHash('sha1').update(req.headers['sec-websocket-key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    const text = Buffer.from('ALLOWED_WEBSOCKET'); socket.write(Buffer.concat([Buffer.from([0x81,text.length]),text]));
    socket.on('data', data => { if ((data[0] & 0x0f) === 8) socket.end(Buffer.from([0x88,0])); });
  }
  allowed.on('upgrade', websocketFixture);
  const tls = https.createServer({key:fs.readFileSync(key),cert:fs.readFileSync(cert)}, handler);
  tls.on('upgrade', websocketFixture);
  good = `http://127.0.0.1:${await listen(allowed)}`;
  secure = `https://127.0.0.1:${await listen(tls)}`;
  const origins = new Set([good, secure]);
  try {
    const doctorOutput = path.join(temp, 'doctor-must-not-create');
    const doctorArgs = [path.join(__dirname,'backend.cjs'),'--backend-root',root,'--doctor','--output-dir',doctorOutput];
    for (const policy of [undefined, '{', JSON.stringify({version:1,origins:[good],uploads:'allow'})]) {
      const env = { ...process.env }; delete env[POLICY_ENV];
      if (policy !== undefined) env[POLICY_ENV]=policy;
      const refused=spawnSync(process.execPath,doctorArgs,{env,encoding:'utf8',timeout:10000});
      assert.equal(refused.status,1); assert.equal(refused.stdout,''); assert.equal(fs.existsSync(doctorOutput),false);
    }
    const diagnosed=spawnSync(process.execPath,doctorArgs,{env:{...process.env,[POLICY_ENV]:JSON.stringify({version:1,origins:[good]})},encoding:'utf8',timeout:10000});
    assert.equal(diagnosed.status,0); assert.equal(JSON.parse(diagnosed.stdout).browser_launched,false);
    assert.equal(fs.existsSync(doctorOutput),false);
    report.cases.push('doctor and malformed/absent/unsupported policy paths create no output tree');
    // The only test-only launch change trusts this one ephemeral fixture cert.
    // Proxy, service worker, profile, downloads and protocol controls are unchanged.
    owner = ownedBrowser({ launch: options => external.chromium.launch({ ...options,
      args: [...options.args, `--ignore-certificate-errors-spki-list=${pin}`] }) }, origins, false);
    const context = await owner.get(), page = await context.newPage();
    report.chrome = context.browser().version();
    await page.goto(good + '/fixture');
    await page.waitForFunction(() => window.completed && window.worker === 'blocked', null, {timeout:10000}).catch(async error => {
      report.fixture_state = await page.evaluate(() => ({ completed:window.completed, worker:window.worker, websocket:window.allowedWebSocket, html:document.documentElement.outerHTML }));
      report.proxy = await owner.stats(); report.allowed_paths=allowedHits; report.denied_paths=deniedHits;
      throw error;
    });
    assert.equal(await page.evaluate(() => window.allowedWebSocket), 'ALLOWED_WEBSOCKET');
    await waitFor(() => allowedHits.includes('/nested-frame'), 'allowed nested frame');
    assert.equal(deniedHits.length, 0);
    assert.equal(context.serviceWorkers().length, 0);
    assert.ok(!allowedHits.includes('/worker.js'));
    report.cases.push('HTTP iframe/nested-frame/image/fetch/form/WebSocket refused before denied server; allowed fetch and WebSocket work; service worker blocked');
    for (const origin of [good, secure]) {
      for (const route of ['/redirect','/chain','/redirect-secure']) {
        // A refused HTTPS load can finish on Chrome's error document later.
        // Isolate each probe; never let a preceding failure race the next case.
        const probe = await context.newPage();
        try {
          const result = await probe.goto(origin + route).catch(() => null);
          if (result) assert.equal(result.status(), 403);
          assert.equal(deniedHits.length, 0);
        } finally { await probe.close(); }
      }
      const probe = await context.newPage();
      try {
        await probe.goto(origin + '/clean');
        const popupPromise = context.waitForEvent('page');
        await probe.evaluate(url => window.open(url), bad + '/popup');
        const popup = await popupPromise;
        await popup.waitForLoadState().catch(()=>{}); await popup.close();
        assert.equal(deniedHits.length, 0);
      } finally { await probe.close(); }
    }
    const postPage = await context.newPage();
    try {
      await postPage.goto(good + '/clean');
      await postPage.evaluate(url => { const form=document.createElement('form');form.method='POST';form.action=url;document.body.append(form);form.submit(); }, good + '/redirect-307');
      await postPage.waitForURL(bad + '/redirected').catch(()=>{});
      assert.equal(deniedHits.length, 0);
    } finally { await postPage.close(); }
    report.cases.push('HTTP/HTTPS 302, chained redirects, POST 307 and popup first requests refused at denied target');
    await page.goto(secure + '/clean');
    const secureSocket = await page.evaluate(url => new Promise(resolve => {
      const socket = new WebSocket(url);
      socket.onmessage = event => { socket.close(); resolve(event.data); };
      socket.onerror = () => resolve('error');
    }), secure.replace('https:', 'wss:') + '/allowed-socket');
    assert.equal(secureSocket, 'ALLOWED_WEBSOCKET');
    report.cases.push('allowed WSS handshake traverses the exact HTTPS tunnel with a fixture-only certificate pin');
    const beforeFileNavigation = page.url();
    await page.evaluate(file => { const a=document.createElement('a');a.href='file://'+file;a.textContent='file-link';document.body.append(a); }, cert);
    const fileRefusal = page.waitForEvent('console', {predicate: message=>message.text().includes('Not allowed to load local resource')});
    await page.getByText('file-link').click();
    await fileRefusal;
    assert.equal(page.url(), beforeFileNavigation);
    assert.ok(!await page.content().then(html=>html.includes('BEGIN CERTIFICATE')));
    report.cases.push('allowed remote page cannot navigate to a local file through a normal link');
    const downloadPromise = page.waitForEvent('download');
    await page.evaluate(() => { const a=document.createElement('a');a.href='/download';a.click(); });
    const download = await downloadPromise;
    assert.ok(await download.failure());
    const saved = path.join(temp, 'download-must-not-exist.txt');
    await assert.rejects(download.saveAs(saved)); assert.equal(fs.existsSync(saved), false);
    const blobPromise = page.waitForEvent('download');
    await page.evaluate(() => { const a=document.createElement('a');a.href=URL.createObjectURL(new Blob(['local blob']));a.download='blob.txt';a.click(); });
    const blob = await blobPromise; assert.ok(await blob.failure());
    await assert.rejects(blob.saveAs(saved)); assert.equal(fs.existsSync(saved), false);
    report.cases.push('HTTPS attachment and blob downloads fail persistence with acceptDownloads:false');
    const stats = await owner.stats();
    assert.ok(stats.httpDenied >= 8, JSON.stringify(stats));
    assert.ok(stats.connectDenied >= 1, JSON.stringify(stats));
    assert.ok(stats.upgradeAllowed >= 1, JSON.stringify(stats));
    assert.ok(stats.connectAllowed >= 1, JSON.stringify(stats));
    report.proxy = stats;
    await owner.close(true); owner = undefined;

    const requireBackend = createRequire(path.join(root, 'package.json'));
    const { Client, StdioClientTransport } = requireBackend('playwright-core/lib/utilsBundle');
    const outputDir = path.join(temp, 'mcp-artifacts');
    transport = new StdioClientTransport({ command: process.execPath,
      args: [path.join(__dirname,'backend.cjs'),'--backend-root',root,'--output-dir',outputDir],
      env: { ...process.env, [POLICY_ENV]: JSON.stringify({version:1,origins:[good]}),
        PLAYWRIGHT_MCP_CDP_ENDPOINT: bad + '/must-not-attach',
        PLAYWRIGHT_MCP_CONFIG: path.join(temp, 'invalid-config-must-not-load'),
        PLAYWRIGHT_MCP_ALLOW_UNRESTRICTED_FILE_ACCESS: 'true',
      }, stderr: 'pipe' });
    let stderr = ''; transport.stderr?.on('data', data => { stderr += data.toString(); });
    client = new Client({name:'atomcode-origin-acceptance',version:'1'}, {capabilities:{}});
    await client.connect(transport);
    const pid = transport.pid;
    assert.equal(client.getServerCapabilities().experimental[CAPABILITY].version, 1);
    const catalog = await client.listTools();
    assert.equal(catalog.tools.length, TOOLS.size);
    assert.ok(catalog.tools.every(tool=>TOOLS.has(tool.name)));
    assert.deepEqual(descendants(pid), [], 'metadata must not launch a browser');
    for (const name of ['browser_evaluate','browser_run_code','browser_file_upload','browser_cdp','browser_route'])
      await assert.rejects(client.callTool({name,arguments:{}}), /policy refused/);
    await assert.rejects(client.callTool({name:'browser_navigate',arguments:{url:bad+'/direct'}}), /policy refused/);
    await assert.rejects(client.callTool({name:'browser_navigate',arguments:{url:'file://'+cert}}), /policy refused/);
    await assert.rejects(client.callTool({name:'browser_take_screenshot',arguments:{filename:saved}}), /policy refused/);
    assert.deepEqual(descendants(pid), [], 'refused calls must not launch a browser');
    const result = await client.callTool({name:'browser_navigate',arguments:{url:good+'/clean'}});
    assert.ok(!result.isError, JSON.stringify(result));
    const snapshot = await client.callTool({name:'browser_snapshot',arguments:{}});
    assert.match(JSON.stringify(snapshot), /ALLOWED_ORIGIN_FIXTURE/);
    const image = await client.callTool({name:'browser_take_screenshot',arguments:{type:'png'}});
    assert.ok(image.content.some(item=>item.type==='image' && item.data.length>100));
    const imageBlock = image.content.find(item=>item.type==='image');
    fs.writeFileSync(reportPath + '.png', Buffer.from(imageBlock.data,'base64'));
    const childPids = descendants(pid); assert.ok(childPids.length > 0, 'real Chrome descendants');
    const closed = await client.callTool({name:'browser_close',arguments:{}});
    assert.ok(!closed.isError);
    await waitFor(() => childPids.every(pid=>!alive(pid)), 'browser_close child cleanup');
    const reopened = await client.callTool({name:'browser_navigate',arguments:{url:good+'/clean'}});
    assert.ok(!reopened.isError);
    const reopenedPids = descendants(pid); assert.ok(reopenedPids.length > 0);
    await client.close(); client = undefined;
    await waitFor(() => [pid,...reopenedPids].every(pid=>!alive(pid)), 'stdio EOF browser cleanup');
    transport = new StdioClientTransport({ command: process.execPath,
      args: [path.join(__dirname,'backend.cjs'),'--backend-root',root,'--output-dir',outputDir],
      env: {...process.env,[POLICY_ENV]:JSON.stringify({version:1,origins:[good]})},stderr:'pipe' });
    transport.stderr?.on('data',data=>{stderr+=data.toString();});
    client = new Client({name:'atomcode-origin-signal-acceptance',version:'1'},{capabilities:{}});
    await client.connect(transport);
    const signalPid=transport.pid;
    const signalNavigation=await client.callTool({name:'browser_navigate',arguments:{url:good+'/clean'}});
    assert.ok(!signalNavigation.isError);
    const signalChildren=descendants(signalPid); assert.ok(signalChildren.length>0);
    process.kill(signalPid,'SIGTERM');
    await waitFor(()=>[signalPid,...signalChildren].every(pid=>!alive(pid)),'SIGTERM owned browser cleanup');
    await client.close();client=undefined;
    assert.equal(deniedHits.length, 0);
    report.cases.push('actual MCP capability/discovery/direct refusal; no launch on metadata/refusal; inherited attachment config ignored; allowed navigate/snapshot/image; close/reopen, EOF and SIGTERM reap observed owned descendants');
    report.mcp_stderr_bytes = Buffer.byteLength(stderr);
    assert.equal(deniedConnections, 0);
    report.denied_server_connections = deniedConnections;
    report.denied_server_hits = deniedHits.length;
    report.allowed_server_hits = allowedHits.length;
    report.passed = true;
  } finally {
    await client?.close().catch(()=>{});
    await owner?.close(true).catch(()=>{});
    for (const socket of sockets) socket.destroy();
    await close(allowed); await close(tls); await close(denied);
    report.denied_server_connections = deniedConnections;
    report.completed_at = new Date().toISOString();
    fs.writeFileSync(reportPath, JSON.stringify(report,null,2)+'\n');
    fs.rmSync(temp, {recursive:true,force:true}); // This verifier owns this mkdtemp only.
  }
  console.log(JSON.stringify(report));
}

if (require.main === module) {
  const argv = process.argv.slice(2);
  assert.equal(argv.length, 4, 'usage: --backend-root <absolute installed package> --report <absolute output>');
  assert.equal(argv[0], '--backend-root'); assert.equal(argv[2], '--report');
  assert.ok(path.isAbsolute(argv[1]) && path.isAbsolute(argv[3]));
  verify(argv[1], argv[3]).catch(error => { console.error(error); process.exitCode=1; });
}
