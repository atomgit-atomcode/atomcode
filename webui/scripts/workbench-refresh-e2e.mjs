// Isolated browser regression: no daemon, filesystem writes or model calls.
import { createServer } from 'vite';
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
const fixture = `
import { h, render } from 'preact';
import { Workbench } from '/src/components/Workbench.tsx';
import { SettingsProvider } from '/src/settings.tsx';
import '/src/styles/theme.css'; import '/src/styles/app.css';
localStorage.setItem('atomcode.lang', 'en');
sessionStorage.setItem('workbench:refresh-test', JSON.stringify({files:[],terminals:[],tab:'files'}));
render(h(SettingsProvider, null, h(Workbench, {session:'refresh-test',cwd:'/project',onClose:()=>{}})), document.getElementById('root'));
`;
const server = await createServer({ root: resolve(import.meta.dirname, '..'), server: { host: '127.0.0.1', port: 0 }, plugins: [{
  name: 'workbench-refresh-fixture',
  configureServer(server) { server.middlewares.use((req, res, next) => {
    if (req.url !== '/__refresh_fixture') return next();
    res.setHeader('Content-Type', 'text/html');
    res.end('<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head><body><div id="root"></div><script type="module" src="/__refresh_fixture.js"></script></body></html>');
  }); },
  resolveId(id) { if (id === '/__refresh_fixture.js') return '\0refresh-fixture.js'; },
  load(id) { if (id === '\0refresh-fixture.js') return fixture; },
}] });
await server.listen();
let browser;
try {
  browser = await chromium.launch({ executablePath: process.env.WORKBENCH_BROWSER || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
  const page = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  let hold = false, failFiles = false, pending = [], previewReads = 0;
  const rows = [{ name: 'folder', directory: true }, ...Array.from({ length: 40 }, (_, i) => ({ name: `file-${i}.txt`, directory: false }))];
  await page.route('**/workbench/files?*', async route => {
    if (hold) await new Promise(resolve => pending.push(resolve));
    if (failFiles) return route.fulfill({ status: 503, json: { error: 'directory temporarily unavailable' } });
    const path = new URL(route.request().url()).searchParams.get('path');
    await route.fulfill({ json: path ? [{ name: 'child.txt', directory: false }] : rows });
  });
  await page.route('**/workbench/preview?*', route => {
    previewReads++;
    return route.fulfill({ contentType: 'text/plain', body: Array.from({ length: 100 }, (_, i) => `Line ${i}`).join('\n') });
  });
  await page.clock.install();
  await page.goto(server.resolvedUrls.local[0] + '__refresh_fixture');
  const tree = page.locator('.workbench-tree');
  await tree.getByRole('button', { name: 'file-0.txt', exact: false }).click();
  await page.locator('.workbench-preview pre').filter({ hasText: 'Line 99' }).waitFor();
  await tree.evaluate(el => {
    el.scrollTop = 120;
    window.originalRow = el.querySelector('button');
    window.originalScroll = el.scrollTop;
    window.originalPreview = document.querySelector('.workbench-preview');
    window.originalPreview.scrollTop = 80;
  });
  assert.ok(await page.evaluate(() => window.originalScroll > 0), 'fixture must have a scrolled directory');
  for (const trigger of ['focus', 'timer']) {
    hold = true;
    const request = page.waitForRequest(r => r.url().includes('/workbench/files?'));
    if (trigger === 'focus') await page.evaluate(() => window.dispatchEvent(new Event('focus')));
    else await page.clock.fastForward(10000);
    await request;
    await page.waitForTimeout(80);
    assert.equal(await tree.getByText('Loading directory…').count(), 0);
    assert.equal(await tree.getByRole('button').count(), rows.length);
    assert.ok(await tree.evaluate(el => el.querySelector('button') === window.originalRow && el.scrollTop === window.originalScroll));
    assert.ok(await page.evaluate(() => document.querySelector('.workbench-preview') === window.originalPreview && window.originalPreview.scrollTop === 80));
    const response = page.waitForResponse(r => r.url().includes('/workbench/files?'));
    hold = false; pending.splice(0).forEach(resolve => resolve());
    await response;
    await page.waitForTimeout(80);
  }
  assert.equal(previewReads, 1, 'directory refresh must not reload the file preview');
  rows.push({ name: 'added.txt', directory: false });
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await tree.getByRole('button', { name: 'added.txt', exact: false }).waitFor();
  failFiles = true;
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await page.getByRole('alert').filter({ hasText: 'directory temporarily unavailable' }).waitFor();
  assert.equal(await tree.locator(':scope > button').count(), rows.length, 'background failure keeps the cached directory');
  failFiles = false;
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await page.waitForFunction(() => !document.querySelector('.workbench-error'));
  // Navigating to a different directory still hides the previous directory.
  hold = true;
  await tree.getByRole('button', { name: 'folder', exact: false }).click();
  await tree.getByText('Loading directory…').waitFor();
  assert.equal(await tree.getByRole('button').count(), 0);
  hold = false; pending.splice(0).forEach(resolve => resolve());
  await tree.getByRole('button', { name: 'child.txt', exact: false }).waitFor();
  // A slow old directory must not replace a newer navigation's rows.
  await page.locator('.workbench-path button').click();
  await tree.getByRole('button', { name: 'added.txt', exact: false }).waitFor();
  hold = true;
  const slowRequest = page.waitForRequest(r => new URL(r.url()).pathname === '/workbench/files' && new URL(r.url()).searchParams.get('path') === 'folder');
  await tree.getByRole('button', { name: 'folder', exact: false }).click();
  await slowRequest;
  await tree.getByText('Loading directory…').waitFor();
  hold = false;
  await page.locator('.workbench-path button').click();
  await tree.getByRole('button', { name: 'added.txt', exact: false }).waitFor();
  pending.splice(0).forEach(resolve => resolve());
  await page.waitForTimeout(80);
  assert.equal(await tree.getByRole('button', { name: 'child.txt', exact: false }).count(), 0);
  assert.deepEqual(errors, []);
  console.log('Mobile focus/timer refresh preserves rows, scroll and preview; new files, refresh failures and stale navigation responses behave correctly');
} finally { await browser?.close(); await server.close(); }
