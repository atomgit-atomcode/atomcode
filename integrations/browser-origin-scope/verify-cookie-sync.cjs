#!/usr/bin/env node
'use strict';
// Opt-in native-cookie conformance, using only locally fulfilled fixture URLs.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const { loadBackend, ownedBrowser } = require('./backend.cjs');
const { synchronizeResponseCookies } = require('./cookies.cjs');
const { synchronizeBrowserCookies } = require('./partition-cookies.cjs');

function children(pid) {
  const result = spawnSync('ps', ['-ax', '-o', 'pid=,ppid='], { encoding: 'utf8' });
  assert.equal(result.status, 0);
  const pairs = result.stdout.trim().split('\n').map(line => line.trim().split(/\s+/).map(Number));
  const found = new Set([pid]);
  for (let i = 0; i < pairs.length; i++) for (const [child, parent] of pairs) if (found.has(parent)) found.add(child);
  found.delete(pid); return [...found];
}
function alive(pid) { try { process.kill(pid, 0); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; } }
async function verify(root, output) {
  assert.notEqual(process.platform, 'win32', 'Recorded process verification uses ps');
  assert.equal(fs.existsSync(output), false, 'fresh report required');
  const external = loadBackend(root);
  const report = { passed: false, platform: process.platform, node: process.version, source_hashes: external.sourceHashes, integration_hashes: {}, cases: [] };
  for (const name of ['cookies.cjs', 'backend.cjs', 'transfers.cjs', 'verify-cookie-sync.cjs'])
    report.integration_hashes[name] = createHash('sha256').update(fs.readFileSync(path.join(__dirname, name))).digest('hex');
  const owner = ownedBrowser(external.chromium, new Set(['https://app.cookie.test', 'http://app.cookie.test']), false);
  let captured = [];
  try {
    const context = await owner.get();
    captured = children(process.pid); assert.ok(captured.length > 1);
    report.chrome = context.browser().version();
    const original = await context.newPage();
    await original.setContent('<title>ORIGINAL_COOKIE_FIXTURE</title><h1>Original page</h1>');
    const sync = (url, headers) => synchronizeResponseCookies(context, new URL(url), headers,
      new AbortController().signal, () => owner.closeCaptured(context));
    const source = 'https://app.cookie.test/private/rotate';
    await sync(source, [
      'session=first; Secure; HttpOnly; SameSite=Strict; Path=/private/',
      'defaultPath=present; Secure', 'shared=present; Secure; Domain=cookie.test; Path=/',
      '__Host-valid=present; Secure; Path=/',
    ]);
    let cookies = await context.cookies('https://app.cookie.test/private/file');
    const session = cookies.find(cookie => cookie.name === 'session');
    assert.equal(session.value, 'first'); assert.equal(session.httpOnly, true); assert.equal(session.secure, true); assert.equal(session.sameSite, 'Strict');
    assert.equal(cookies.find(cookie => cookie.name === 'defaultPath').path, '/private');
    assert.ok((await context.cookies('https://sibling.cookie.test/private/file')).some(cookie => cookie.name === 'shared'));
    assert.ok(!(await context.cookies('https://sibling.cookie.test/private/file')).some(cookie => cookie.name === 'session'));
    report.cases.push('native HttpOnly/Secure/SameSite/default-path and domain/host-only semantics');
    await sync(source, [
      'foreign=never; Secure; Domain=foreign.test; Path=/', 'suffix=never; Secure; Domain=test; Path=/',
      '__Host-invalid=never; Secure; Domain=app.cookie.test; Path=/',
      '__Secure-invalid=never; Path=/', 'insecureNone=never; SameSite=None; Path=/', 'oversized=' + 'x'.repeat(5000) + '; Secure; Path=/',
    ]);
    const names = (await context.cookies()).map(cookie => cookie.name);
    for (const name of ['foreign', 'suffix', '__Host-invalid', '__Secure-invalid', 'insecureNone', 'oversized']) assert.ok(!names.includes(name), name);
    report.cases.push('foreign/public-suffix domains, invalid prefixes and insecure SameSite=None refused by Chromium');
    await sync('http://app.cookie.test/private/rotate', ['session=plaintext-overwrite; Path=/private/']);
    assert.equal((await context.cookies('https://app.cookie.test/private/file')).find(cookie => cookie.name === 'session').value, 'first');
    report.cases.push('insecure HTTP response cannot overlay the Secure session cookie');
    await sync(source, ['session=rotated; Secure; HttpOnly; Path=/private/', 'gone=present; Secure; Path=/private/']);
    assert.equal((await context.cookies('https://app.cookie.test/private/file')).find(cookie => cookie.name === 'session').value, 'rotated');
    await sync(source, ['gone=deleted; Secure; Path=/private/; Max-Age=0; Expires=Wed, 01 Jan 2031 00:00:00 GMT']);
    assert.ok(!(await context.cookies('https://app.cookie.test/private/file')).some(cookie => cookie.name === 'gone'));
    report.cases.push('session rotation and Max-Age deletion precedence');
    await assert.rejects(sync(source, ['partitioned=present; Secure; SameSite=None; Partitioned; Path=/']));
    assert.ok(!(await context.cookies()).some(cookie => cookie.name === 'partitioned'));
    report.cases.push('Partitioned response cookie refused before mutation; never downgraded to an unpartitioned cookie');
    assert.equal(context.pages().length, 1); assert.equal(context.pages()[0], original);
    assert.equal(await original.title(), 'ORIGINAL_COOKIE_FIXTURE');
    report.proxy = await owner.stats();
    assert.equal(report.proxy.httpAllowed, 0); assert.equal(report.proxy.upgradeAllowed, 0);
    report.cases.push('original page retained, every temporary page disposed and zero forwarded HTTP/WS requests; source-origin speculative CONNECT attempts recorded separately');
    const controller = new AbortController();
    context.once('page', page => page.once('request', () => controller.abort()));
    await assert.rejects(synchronizeResponseCookies(context, new URL('https://app.cookie.test/private/cancel'),
      ['session=cancelled; Secure; Path=/private/'], controller.signal, () => owner.closeCaptured(context)));
    await assert.rejects(owner.existing());
    const replacement = await owner.get();
    captured = [...new Set([...captured, ...children(process.pid)])];
    report.replacement_chrome = replacement.browser().version();
    assert.equal((await replacement.cookies()).length, 0);
    await owner.closeCaptured(context);
    assert.equal(await owner.existing(), replacement);
    report.cases.push('actual native-navigation cancellation disposes the captured context; explicit replacement is empty and late old cleanup cannot close it');

    // Inject cancellation through public browser/session seams after one
    // original-context commit, while parsing the next header in a scratch jar.
    const identityPage = await replacement.newPage();
    const identitySession = await replacement.newCDPSession(identityPage);
    const replacementId = (await identitySession.send('Target.getTargetInfo')).targetInfo.browserContextId;
    await identitySession.detach();
    const browser = replacement.browser();
    const newContext = browser.newContext.bind(browser);
    const newStorage = browser.newBrowserCDPSession.bind(browser);
    const partitionCancel = new AbortController();
    let nativeRequests = 0, originalCommits = 0;
    browser.newContext = async (...args) => {
      const temporary = await newContext(...args);
      temporary.on('page', page => page.on('request', () => {
        captured = [...new Set([...captured, ...children(process.pid)])];
        if (++nativeRequests === 3) partitionCancel.abort();
      }));
      return temporary;
    };
    browser.newBrowserCDPSession = async (...args) => {
      const session = await newStorage(...args);
      const send = session.send.bind(session);
      session.send = async (method, params) => {
        if (method === 'Storage.setCookies' && params.browserContextId === replacementId) originalCommits++;
        return send(method, params);
      };
      return session;
    };
    await assert.rejects(synchronizeBrowserCookies(replacement, new URL(source), [
      'first=fixture; Secure; HttpOnly; Partitioned; Path=/',
      'second=fixture; Secure; HttpOnly; Partitioned; Path=/',
    ], partitionCancel.signal, () => owner.closeCaptured(replacement)));
    assert.equal(nativeRequests, 3); assert.equal(originalCommits, 1);
    await assert.rejects(owner.existing());
    const afterPartial = await owner.get();
    captured = [...new Set([...captured, ...children(process.pid)])];
    assert.equal((await afterPartial.cookies()).length, 0);
    await owner.closeCaptured(replacement);
    assert.equal(await owner.existing(), afterPartial);
    report.cases.push('Partitioned batch cancellation after exactly one native commit closes captured credentials without retry; explicit replacement stays empty and survives late cleanup');

    report.passed = true;
  } finally {
    await owner.close(true);
    const deadline = Date.now() + 10000;
    while (captured.some(alive) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 20));
    report.owned_pids = captured;
    report.owned_processes_gone = captured.every(pid => !alive(pid));
    if (!report.owned_processes_gone) report.passed = false;
    report.completed_at = new Date().toISOString();
    fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n', { flag: 'wx' });
  }
  assert.equal(report.passed, true); console.log(JSON.stringify(report));
}
if (require.main === module) {
  const args = process.argv.slice(2);
  assert.equal(args.length, 4); assert.equal(args[0], '--backend-root'); assert.equal(args[2], '--report');
  assert.ok(path.isAbsolute(args[1]) && path.isAbsolute(args[3]));
  verify(args[1], args[3]).catch(error => { console.error(error); process.exitCode = 1; });
}
