// Browser regression for directory failure/empty/retry and relay diagnostics.
import { createServer } from 'vite';
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
const root = fileURLToPath(new URL('..', import.meta.url));
const server = await createServer({ root, server: { port: 0 }, plugins: [{
  name: 'workbench-diagnostics-fixture',
  resolveId(id) { if (id === '/diagnostics-entry.js') return '\0diagnostics-entry'; },
  load(id) {
    if (id !== '\0diagnostics-entry') return;
    return `import {h,render} from 'preact';
      import {Workbench} from '/src/components/Workbench.tsx';
      import {SettingsProvider} from '/src/settings.tsx';
      render(h(SettingsProvider,{},h(Workbench,{session:'fixture',cwd:'workspace',onClose(){}})),document.getElementById('app'));`;
  },
  configureServer(server) {
    server.middlewares.use(async (req, res, next) => {
      if (!req.url?.split('?')[0].endsWith('/diagnostics')) return next();
      res.setHeader('Content-Type', 'text/html');
      res.end(await server.transformIndexHtml(req.url, '<!doctype html><div id="app"></div><script type="module" src="/diagnostics-entry.js"></script>'));
    });
  },
}] });
await server.listen();
const browser = await chromium.launch({ executablePath: process.env.WORKBENCH_BROWSER || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
try {
  const page = await browser.newPage();
  page.on('pageerror', error => console.error(error));
  let release;
  const pending = new Promise(resolve => { release = resolve; });
  let attempt = 0;
  await page.route('**/workbench/files?**', async route => {
    attempt++;
    if (attempt === 1) {
      await pending;
      await route.fulfill({ contentType: 'text/html', body: '<html>SPA</html>' });
    }
    else await route.fulfill({ contentType: 'application/json', body: '[]' });
  });
  const base = server.resolvedUrls.local[0];
  await page.goto(base + 'diagnostics');
  await page.getByRole('button', { name: /Files|文件/ }).first().click();
  await page.getByText(/Loading directory|正在加载目录/).waitFor();
  release();
  await page.getByText(/Directory could not be loaded|目录加载失败/).waitFor();
  assert.match(await page.locator('.workbench-error').innerText(), /接口返回了页面|API returned a page/);
  await page.getByRole('button', { name: /Retry|重试/ }).click();
  await page.getByText(/Empty directory|空目录/).waitFor();
  await page.goto(base + 'webui/diagnostics');
  await page.getByRole('button', { name: /Files|文件/ }).first().click();
  await page.getByText(/Directory could not be loaded|目录加载失败/).waitFor();
  assert.match(await page.locator('.workbench-error').innerText(), /relay.*not paired|中继未配对/i);
  await page.screenshot({ path: '/tmp/atomcode-workbench-diagnostics.png' });
  console.log('Workbench HTML failure, retry, empty and unpaired relay states passed');
} finally { await browser.close(); await server.close(); }
