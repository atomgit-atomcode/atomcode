// Settings UI with a local mock daemon: no credentials or model calls.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile, mkdir } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const clients = new Set(), writes = [];
let fail = false;
const config = { path: '/fixture/config.toml', providers: [
  { name: 'account/vl', model: 'VL model', supports_vision: true },
  { name: 'account/text', model: 'Text model', supports_vision: false },
], provider_accounts: [], vision_preprocessor_provider: 'removed-model', vision_preprocessor_status: 'unresolvable' };
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://fixture').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path === '/live') { res.writeHead(200, { 'Content-Type': 'text/event-stream' }); res.write('data: {"type":"state","running":false}\n\n'); clients.add(res); req.on('close', () => clients.delete(res)); return; }
  if (path === '/config/vision-preprocessor') {
    let body = ''; for await (const chunk of req) body += chunk;
    assert.match(req.headers['content-type'], /application\/json/);
    const update = JSON.parse(body); writes.push(update);
    if (fail) { res.statusCode = 400; return json({ error: 'save rejected' }); }
    assert.ok(['', 'account/vl'].includes(update.provider));
    config.vision_preprocessor_provider = update.provider || null;
    config.vision_preprocessor_status = update.provider ? 'resolved' : 'unconfigured';
    return json(config);
  }
  if (path === '/config') return json(config);
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (['/models', '/projects', '/sessions', '/skills', '/chat/active'].includes(path)) return json([]);
  if (path === '/live/mode') return json({ mode: 'build' });
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw new Error('path');
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream');
    res.end(await readFile(file));
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: process.env.PROVIDER_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const errors = [];
try {
  for (const [width, lang, modelMenu] of [[1200, 'en', 'Model configuration'], [390, 'zh', '模型配置']]) {
    config.vision_preprocessor_provider = 'removed-model'; config.vision_preprocessor_status = 'unresolvable';
    const context = await browser.newContext({ viewport: { width, height: 1000 } });
    await context.addInitScript(lang => { localStorage.setItem('atomcode.lang', lang); localStorage.setItem('atomcode.onboarding.done', '1'); }, lang);
    const page = await context.newPage(); page.on('pageerror', error => errors.push(error.message));
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    if (width < 768) await page.locator('.mobile-menu-btn').click();
    await page.locator('.sidebar-settings-btn:visible, .rail-btn-settings:visible').first().click();
    await page.getByRole('button', { name: modelMenu, exact: true }).click();
    const section = page.locator('.vision-preprocessor'), select = section.locator('select');
    await select.waitFor();
    assert.equal(await select.inputValue(), 'removed-model');
    assert.deepEqual(await select.locator('option:not(:disabled)').evaluateAll(options => options.map(option => option.value)), ['', 'account/vl']);
    assert.match(await section.innerText(), lang === 'en' ? /Cannot resolve/ : /无法解析/);
    await select.selectOption('account/vl');
    await page.waitForFunction(() => document.querySelector('.vision-preprocessor')?.getAttribute('aria-busy') === 'false');
    assert.equal(await select.inputValue(), 'account/vl');
    fail = true;
    await select.selectOption('');
    await section.getByRole('alert').waitFor();
    assert.equal(await select.inputValue(), 'account/vl', 'failed save retains the saved selection');
    fail = false;
    await select.selectOption('');
    await page.waitForFunction(() => document.querySelector('.vision-preprocessor')?.getAttribute('aria-busy') === 'false');
    assert.equal(await select.inputValue(), '');
    assert.equal(config.vision_preprocessor_provider, null);
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'no viewport overflow');
    const box = await section.boundingBox(); assert.ok(box.width <= width && box.x >= 0);
    await mkdir('/tmp/atomcode-vl-preview', { recursive: true });
    await section.screenshot({ path: `/tmp/atomcode-vl-preview/${width}.png` });
    await context.close();
  }
  assert.equal(writes.length, 6);
  assert.deepEqual(errors, []);
  console.log('VL desktop/mobile selection, clear, stale value, failure rollback and i18n passed');
} finally { await browser.close(); for (const client of clients) client.end(); await new Promise(resolve => server.close(resolve)); }
