#!/usr/bin/env node
'use strict';
// Opt-in characterization, NOT an MCP backend. Partition-loss is an expected
// negative control in a private fixture, never an accepted publication path.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const https = require('node:https');
const tls = require('node:tls');
const { firstPartyCookieHeader } = require('./partition-cookies.cjs');
const { ControlledDownloads } = require('./transfers.cjs');
const { spawnSync } = require('node:child_process');
const { once } = require('node:events');
const { createHash, X509Certificate } = require('node:crypto');
const { loadBackend, ownedBrowser } = require('./backend.cjs');

function processes() {
  const result = spawnSync('ps', ['-ax', '-o', 'pid=,ppid=,command='], { encoding: 'utf8' });
  assert.equal(result.status, 0);
  return result.stdout.trim().split('\n').map(line => {
    const match = line.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/);
    return { pid: Number(match[1]), parent: Number(match[2]), command: match[3] };
  });
}
function owned(rows) {
  const found = new Set([process.pid]);
  for (let i = 0; i < rows.length; i++) for (const row of rows) if (found.has(row.parent)) found.add(row.pid);
  found.delete(process.pid); return [...found];
}
function alive(pid) { try { process.kill(pid, 0); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; } }
async function characterize(root, output) {
  assert.notEqual(process.platform, 'win32', 'Recorded process verification uses ps');
  assert.equal(fs.existsSync(output), false, 'fresh report required');
  const external = loadBackend(root);
  const report = { passed: false, kind: 'partition-characterization', platform: process.platform,
    node: process.version, source_hashes: external.sourceHashes, verifier_sha256: createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'), cases: [], integration_hashes: {} };
  for (const name of ['partition-cookies.cjs', 'cookies.cjs', 'transfers.cjs', 'backend.cjs'])
    report.integration_hashes[name] = createHash('sha256').update(fs.readFileSync(path.join(__dirname, name))).digest('hex');
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-partition-characterization-'));
  const key = path.join(temp, 'fixture.key'), cert = path.join(temp, 'fixture.pem');
  assert.equal(spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key,
    '-out', cert, '-days', '1', '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1'], { stdio: 'ignore' }).status, 0);
  const certificate = new X509Certificate(fs.readFileSync(cert));
  const pin = createHash('sha256').update(certificate.publicKey.export({ type: 'spki', format: 'der' })).digest('base64');
  const observations = [], privateHeaders = new Map();
  const server = https.createServer({ key: fs.readFileSync(key), cert: fs.readFileSync(cert) }, (req, res) => {
    const cookie = req.headers.cookie || '';
    privateHeaders.set(req.url, cookie);
    observations.push({ path: req.url, ordinary: cookie.includes('chips=ordinary-fixture'),
      first: cookie.includes('chips=first-fixture'), ancestor: cookie.includes('chips=ancestor-fixture'), foreign: cookie.includes('chips=foreign-fixture'),
      direct: cookie.includes('chips=direct-fixture') });
    if (req.url === '/direct-set') res.setHeader('Set-Cookie', 'chips=direct-fixture; Secure; HttpOnly; SameSite=None; Partitioned; Path=/');
    res.setHeader('Content-Type', 'text/plain'); res.end('PARTITION_FIXTURE');
  });
  let owner, captured = [], originalCAs;
  try {
    server.listen(0, '127.0.0.1'); await once(server, 'listening');
    const origin = `https://127.0.0.1:${server.address().port}`;
    owner = ownedBrowser({ launch: options => external.chromium.launch({ ...options,
      args: [...options.args, '--ignore-certificate-errors-spki-list=' + pin] }) }, new Set([origin]), false);
    const context = await owner.get(), browser = context.browser(), page = await context.newPage();
    const rows = processes(); captured = owned(rows); assert.ok(captured.length > 1);
    const command = rows.find(row => row.parent === process.pid && captured.includes(row.pid))?.command || '';
    report.default_disables_third_party_storage_partitioning = command.match(/--disable-features=([^\s]+)/)?.[1].split(',').includes('ThirdPartyStoragePartitioning') || false;
    report.chrome = browser.version();
    await page.goto(origin + '/direct-set');
    let direct = (await context.cookies()).find(cookie => cookie.name === 'chips');
    assert.equal(typeof direct?.partitionKey, 'string');
    await page.goto(origin + '/direct-check');
    assert.equal(observations.find(item => item.path === '/direct-check').direct, true);
    report.direct_response_partition_key = direct.partitionKey;
    report.cases.push('ordinary HTTPS response creates a real Partitioned cookie and the browser sends it in first-party navigation');
    await context.clearCookies();
    await page.route('**/*', async route => {
      try { await route.fulfill({ status: 200, headers: { 'content-type': 'text/plain',
        'set-cookie': 'chips=intercepted-fixture; Secure; HttpOnly; SameSite=None; Partitioned; Path=/' }, body: '' }); }
      catch { await route.abort().catch(() => {}); }
    });
    await page.goto(origin + '/intercepted-set');
    const intercepted = (await context.cookies()).find(cookie => cookie.name === 'chips');
    assert.ok(intercepted); assert.equal(intercepted.partitionKey, undefined);
    assert.equal(observations.filter(item => item.path === '/intercepted-set').length, 0, 'fulfillment did not make a remote GET');
    report.interception_loses_partition = true;
    report.cases.push('expected negative control: Fetch fulfillment stores an unpartitioned cookie without a remote HTTP GET');
    await page.unroute('**/*'); await context.clearCookies();
    await context.addCookies([{ name: 'chips', value: 'ordinary-fixture', url: origin + '/', secure: true, httpOnly: true, sameSite: 'None' }]);
    await context.addCookies([{ name: 'chips', value: 'ancestor-fixture', url: origin + '/', secure: true, httpOnly: true, sameSite: 'None', partitionKey: origin }]);
    const pageSession = await context.newCDPSession(page);
    const info = (await pageSession.send('Target.getTargetInfo')).targetInfo;
    assert.equal(typeof info.browserContextId, 'string');
    const storage = await browser.newBrowserCDPSession();
    const stored = await storage.send('Storage.getCookies', { browserContextId: info.browserContextId });
    const ancestor = stored.cookies.find(cookie => cookie.partitionKey);
    assert.equal(ancestor.partitionKey.hasCrossSiteAncestor, true);
    report.canonical_site = ancestor.partitionKey.topLevelSite;
    const first = { name: 'chips', value: 'first-fixture', domain: '127.0.0.1', path: '/', secure: true, httpOnly: true, sameSite: 'None',
      partitionKey: { topLevelSite: report.canonical_site, hasCrossSiteAncestor: false } };
    await storage.send('Storage.setCookies', { browserContextId: info.browserContextId, cookies: [first,
      { ...first, value: 'foreign-fixture', partitionKey: { topLevelSite: 'https://foreign.test', hasCrossSiteAncestor: false } }] });
    const readback = await storage.send('Storage.getCookies', { browserContextId: info.browserContextId });
    assert.equal(readback.cookies.filter(cookie => cookie.name === 'chips').length, 4);
    await page.goto(origin + '/storage-check');
    assert.deepEqual(observations.find(item => item.path === '/storage-check'), { path: '/storage-check', ordinary: true, first: true, ancestor: false, foreign: false, direct: false });
    report.cases.push('explicit context-bound Storage bridge preserves four independent cookie identities; browser sends only ordinary and matching first-party keys');
    const header = await firstPartyCookieHeader(context, new URL(origin + '/partition-read'), new AbortController().signal, () => owner.closeCaptured(context));
    assert.ok(header.includes('chips=ordinary-fixture')); assert.ok(header.includes('chips=first-fixture'));
    assert.ok(!header.includes('ancestor-fixture')); assert.ok(!header.includes('foreign-fixture'));
    assert.equal(observations.filter(item => item.path === '/partition-read').length, 0);
    assert.equal(context.pages().length, 1); assert.equal(context.pages()[0], page); assert.equal(browser.contexts().length, 1);
    assert.equal(typeof tls.setDefaultCACertificates, 'function', 'publisher TLS fixture verification requires tls.setDefaultCACertificates (recorded Node 22.22.0)');
    originalCAs = tls.getCACertificates('default');
    tls.setDefaultCACertificates([...originalCAs, fs.readFileSync(cert, 'utf8')]); // This verifier process only.
    const files = path.join(temp, 'download-files'); fs.mkdirSync(files, { mode: 0o700 });
    const publisher = new ControlledDownloads({ files, authorize: candidate => candidate === origin,
      getContext: () => owner.existing(), closeContext: capturedContext => owner.closeCaptured(capturedContext) });
    try {
      const artifact = await publisher.execute('first-party', { url: origin + '/publisher-first-party', use_browser_context: true });
      assert.equal(fs.readFileSync(artifact.path, 'utf8'), 'PARTITION_FIXTURE');
      assert.deepEqual(observations.find(item => item.path === '/publisher-first-party'),
        { path: '/publisher-first-party', ordinary: true, first: true, ancestor: false, foreign: false, direct: false });
      assert.equal(observations.filter(item => item.path === '/publisher-first-party').length, 1);
      assert.equal(privateHeaders.get('/publisher-first-party'), privateHeaders.get('/storage-check'), 'native header order');
      assert.equal(context.pages().length, 1); assert.equal(browser.contexts().length, 1);
    } finally { await publisher.close(); }
    report.cases.push('actual publisher first-party HTTPS request includes only matching ordinary/partitioned credentials, publishes exact bytes once and leaves no helper page/context');

    await storage.send('Storage.setCookies', { browserContextId: info.browserContextId, cookies: [{ ...first, expires: 1 }] });
    await page.goto(origin + '/expiry-check');
    assert.deepEqual(observations.find(item => item.path === '/expiry-check'), { path: '/expiry-check', ordinary: true, first: false, ancestor: false, foreign: false, direct: false });
    const remaining = await storage.send('Storage.getCookies', { browserContextId: info.browserContextId });
    assert.equal(remaining.cookies.filter(cookie => cookie.name === 'chips').length, 3);
    report.cases.push('expired Storage write deletes only the selected partition, retaining ordinary, cross-ancestor and foreign-site cookies');
    await context.clearCookies();
    await storage.send('Storage.setCookies', { browserContextId: info.browserContextId, cookies: [first] });
    await context.addCookies([{ name: 'chips', value: 'ordinary-fixture', url: origin + '/', secure: true, httpOnly: true, sameSite: 'None' }]);
    await page.goto(origin + '/reverse-check');
    const reordered = new ControlledDownloads({ files, authorize: candidate => candidate === origin,
      getContext: () => owner.existing(), closeContext: capturedContext => owner.closeCaptured(capturedContext) });
    try {
      await reordered.execute('reversed', { url: origin + '/publisher-reversed', use_browser_context: true });
      assert.equal(privateHeaders.get('/publisher-reversed'), privateHeaders.get('/reverse-check'), 'native header order after reversing creation');
      assert.equal(context.pages().length, 1); assert.equal(browser.contexts().length, 1);
    } finally { await reordered.close(); }
    report.cases.push('matching native Cookie header ordering with ordinary/partitioned creation order reversed');
    captured = [...new Set([...captured, ...owned(processes())])];

    await storage.detach(); await pageSession.detach();
    report.observations = observations;
    report.passed = true;
  } finally {
    if (originalCAs) tls.setDefaultCACertificates(originalCAs);
    await owner?.close(true);
    const deadline = Date.now() + 10000;
    while (captured.some(alive) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 20));
    report.owned_pids = captured; report.owned_processes_gone = captured.every(pid => !alive(pid));
    if (!report.owned_processes_gone) report.passed = false;
    server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
    fs.rmSync(temp, { recursive: true, force: true });
    report.completed_at = new Date().toISOString(); fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n', { flag: 'wx' });
  }
  assert.equal(report.passed, true); console.log(JSON.stringify(report));
}
if (require.main === module) {
  const args = process.argv.slice(2);
  assert.equal(args.length, 4); assert.equal(args[0], '--backend-root'); assert.equal(args[2], '--report');
  assert.ok(path.isAbsolute(args[1]) && path.isAbsolute(args[3]));
  characterize(args[1], args[3]).catch(error => { console.error(error); process.exitCode = 1; });
}
