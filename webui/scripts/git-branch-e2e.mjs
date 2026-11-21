import { createServer } from 'vite';
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
const root = resolve(import.meta.dirname, '..');
const fixture = `
import { h, render } from 'preact';
import { GitBranch } from '/src/components/GitBranch.tsx';
import '/src/styles/theme.css'; import '/src/styles/app.css';
window.showBranchCwd = cwd => render(h('button', {class:'input-cwd'}, h('span', {class:'input-cwd-path'}, cwd), h(GitBranch, {cwd})), document.getElementById('root'));
window.showBranchCwd('/first');
`;
const server = await createServer({ root, server: { host: '127.0.0.1', port: 0 }, plugins: [{
  name: 'branch-fixture',
  configureServer(server) {
    server.middlewares.use((req, res, next) => {
      if (req.url !== '/__branch_fixture') return next();
      res.setHeader('Content-Type', 'text/html');
      res.end('<html><head><meta name="viewport" content="width=device-width, initial-scale=1"></head><body><div id="root"></div><script type="module" src="/__branch_fixture.js"></script></body></html>');
    });
  },
  resolveId(id) { if (id === '/__branch_fixture.js') return '\0branch-fixture.js'; },
  load(id) { if (id === '\0branch-fixture.js') return fixture; },
}] });
await server.listen();
const base = server.resolvedUrls.local[0];
const browser = await chromium.launch({ executablePath: process.env.GIT_BRANCH_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
try {
  const page = await browser.newPage({ viewport: { width: 1100, height: 500 } });
  const errors = [];
  page.on('pageerror', error => { errors.push(error.message); console.error(error.message); });
  page.on('console', msg => { if (msg.type() === 'error') console.error(msg.text()); });
  let branch = 'main'; let reads = 0;
  await page.route('**/project/git-branch?*', async route => {
    reads++;
    const dir = new URL(route.request().url()).searchParams.get('dir');
    if (dir === '/slow') await new Promise(resolve => setTimeout(resolve, 500));
    await route.fulfill({ json: { working_dir: dir, branch: dir === '/slow' ? 'stale' : branch } }).catch(() => {});
  });
  await page.goto(base + '__branch_fixture');
  await page.waitForFunction(() => document.querySelector('.input-cwd-branch-name')?.textContent === 'main');
  branch = 'feature/switched';
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await page.waitForFunction(() => document.querySelector('.input-cwd-branch-name')?.textContent === 'feature/switched');
  await page.evaluate(() => window.showBranchCwd('/slow'));
  await page.waitForTimeout(50);
  branch = 'next-checkout';
  await page.evaluate(() => window.showBranchCwd('/second'));
  await page.waitForFunction(() => document.querySelector('.input-cwd-branch-name')?.textContent === 'next-checkout');
  await page.waitForTimeout(600);
  assert.equal(await page.locator('.input-cwd-branch-name').textContent(), 'next-checkout');
  await page.evaluate(() => Object.defineProperty(document, 'hidden', { configurable: true, value: true }));
  const before = reads;
  await page.waitForTimeout(2100);
  assert.equal(reads, before, 'hidden pages do not poll');
  await page.evaluate(() => { delete document.hidden; document.dispatchEvent(new Event('visibilitychange')); });
  branch = null;
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await page.waitForFunction(() => !document.querySelector('.input-cwd-branch'));
  branch = 'feature/' + 'long-branch-'.repeat(12);
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await page.waitForFunction(() => document.querySelector('.input-cwd-branch'));
  await page.setViewportSize({ width: 390, height: 500 });
  assert.ok(await page.locator('.input-cwd-branch').evaluate(el => el.getBoundingClientRect().right <= el.parentElement.getBoundingClientRect().right));
  await page.screenshot({ path: '/tmp/atomcode-web-branch-mobile.png' });
  assert.deepEqual(errors, []);
  console.log('Git branch refresh, stale-response isolation, hidden-page polling, absent branch and mobile layout passed');
} finally { await browser.close(); await server.close(); }
