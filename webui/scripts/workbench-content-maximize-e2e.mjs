// Browser regression for file/diff maximize, draft and scroll preservation.
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
      res.end(await server.transformIndexHtml(req.url, '<!doctype html><style>html,body,#app{height:100%;margin:0}</style><div id="app"></div><script type="module" src="/diagnostics-entry.js"></script>'));
    });
  },
}] });
await server.listen();
const browser = await chromium.launch({ executablePath: process.env.WORKBENCH_BROWSER || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
try {
  const page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
  page.on('dialog', dialog => dialog.accept());
  await page.route('**/workbench/**', async route => {
    const path = new URL(route.request().url()).pathname;
    const body = path.endsWith('/files') ? [{name:'test.txt', directory:false}, {name:'image.png', directory:false}]
      : path.endsWith('/git') ? [{path:'test.txt', index:' ', worktree:'M'}]
      : path.endsWith('/text') ? {content:'draft original', revision:'fixture'}
      : path.endsWith('/diff') ? {diff:'@@ fixture\n+added\n-removed'} : null;
    if (path.endsWith('/preview')) {
      await route.fulfill({contentType:'text/plain', body: 'file preview\n'.repeat(35)});
    } else await route.fulfill({contentType:'application/json', body:JSON.stringify(body)});
  });
  const base = server.resolvedUrls.local[0];
  await page.goto(base + 'diagnostics');
  await page.getByRole('button', {name:/Files|文件/}).first().click();
  await page.locator('.workbench-tree button').first().click();
  const preview = page.locator('.workbench-preview');
  await preview.waitFor();
  await page.waitForFunction(() => document.querySelector('.workbench-preview pre')?.textContent.includes('file preview'));
  await preview.evaluate(el => el.scrollTop = 150);
  assert.equal(await preview.evaluate(el => el.scrollTop), 150);
  await page.getByRole('button', {name:/^最大化文件$|^Maximize file$/}).click();
  assert.equal(await page.locator('.workbench-tree').isVisible(), false);
  assert.equal(await page.locator('.workbench').evaluate(el => Math.round(el.getBoundingClientRect().width)), 1200);
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('.workbench-tree').isVisible(), true);
  assert.equal(await preview.evaluate(el => el.scrollTop), 150);
  await page.getByRole('button', {name:/^编辑$|^Edit$/}).click();
  const editor = page.locator('.workbench-editor textarea');
  await editor.fill('unsaved draft');
  await page.getByRole('button', {name:/^最大化文件$|^Maximize file$/}).click();
  await page.keyboard.press('Escape');
  assert.equal(await editor.inputValue(), 'unsaved draft');
  await page.getByRole('button', {name:/^Git$/}).click();
  await page.locator('.workbench-changes button').first().click();
  await page.getByRole('button', {name:/^最大化差异$|^Maximize diff$/}).click();
  assert.equal(await page.locator('.workbench-changes').isVisible(), false);
  await page.screenshot({path:'/tmp/atomcode-workbench-content-maximize.png'});
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('.workbench-changes').isVisible(), true);
  await page.getByRole('button', {name:/^工作区全屏$|^Fullscreen workspace$/}).click();
  await page.getByRole('button', {name:/^最大化差异$|^Maximize diff$/}).click();
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('.workbench').evaluate(el => el.classList.contains('workbench-fullscreen')), true);
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('.workbench').evaluate(el => el.classList.contains('workbench-fullscreen')), false);
  await page.getByRole('button', {name:/^最大化差异$|^Maximize diff$/}).click();
  await page.getByRole('button', {name:/^文件$|^Files$/}).click();
  await page.locator('.workbench-tree').waitFor({state:'visible'});
  await page.locator('.workbench-tree button').filter({hasText:'image.png'}).click();
  assert.equal(await page.getByRole('button', {name:/^编辑$|^Edit$/}).isDisabled(), true);
  await page.getByRole('button', {name:/^最大化文件$|^Maximize file$/}).click();
  await page.getByRole('button', {name:/^恢复文件布局$|^Restore file layout$/}).click();
  assert.equal(await page.locator('.workbench-tree').isVisible(), true);
  await page.getByRole('button', {name:/^最大化文件$|^Maximize file$/}).click();
  await page.getByRole('button', {name:/^关闭 image.png$|^Close image.png$/}).click();
  await page.getByRole('button', {name:/^关闭 test.txt$|^Close test.txt$/}).click();
  await page.locator('.workbench-tree').waitFor({state:'visible'});
  assert.equal(await page.locator('.workbench').evaluate(el => el.classList.contains('workbench-content-maximized')), false);
  console.log('Preview scroll, draft, diff maximize, tab restoration and image controls passed');
} finally { await browser.close(); await server.close(); }
