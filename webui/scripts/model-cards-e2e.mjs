import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile, mkdir } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const dist = resolve(import.meta.dirname, '../dist');
const catalog = JSON.parse(await readFile(resolve(import.meta.dirname, '../../data/model-cards.json'), 'utf8'));
const session = { id: 'card-fixture', name: 'Model card fixture', working_dir: '/fixture', project_hash: 'fixture', message_count: 0, created_at: 1700000000, updated_at: 1700000001 };
const models = [
  { provider: 'glm', model: 'glm-5.3-flash', model_card_id: 'glm-5-3-flash', context_window: 12345, supports_vision: true, is_default: true, effort_levels: [] },
  { provider: 'unlisted', model: 'lumen-alpha', model_card_id: null, model_card_status: 'no_public_data', context_window: 9876, supports_vision: null, effort_levels: [] },
  { provider: 'unknown', model: 'private-model', model_card_id: null, context_window: 9876, supports_vision: false, effort_levels: [] },
];
let catalogueRequests = 0;
let writes = 0;
let fail = false;
const streams = new Set();
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (!['GET', 'HEAD'].includes(req.method)) writes++;
  if (path === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream' });
    res.write(`data: ${JSON.stringify({ type: 'snapshot', session_id: session.id, working_dir: '/fixture', project_hash: 'fixture', provider: 'glm', model: 'glm-5.3-flash', mode: 'build', messages: [] })}\n\n`);
    res.write('data: {"type":"state","running":false}\n\n');
    streams.add(res); req.on('close', () => streams.delete(res)); return;
  }
  if (path === '/model-cards') { catalogueRequests++; if (fail) {res.writeHead(503);res.end();return;} return json(catalog); }
  if (path === '/models') return json(models);
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/live/mode') return json({ mode: 'build' });
  if (path === '/live/session') return json({ success: true, session_id: session.id });
  if (path.includes('/sessions/resolve/')) return json(session);
  if (path === '/projects/fixture/sessions/' + session.id) return json({ ...session, messages: [] });
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json([session]);
  if (path === '/chat/active' || path === '/skills' || path === '/projects') return json([]);
  try {
    const file = resolve(dist, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(dist + '/')) throw new Error('path');
    const bytes = await readFile(file);
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream');
    res.end(bytes);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({ executablePath: process.env.MODEL_CARDS_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const errors = [];
try {
  for (const mobile of [false, true]) {
    const context = await browser.newContext({ viewport: mobile ? { width: 390, height: 844 } : { width: 1440, height: 1000 } });
    await context.addInitScript(({ mobile }) => {
      localStorage.setItem('atomcode.lang', mobile ? 'zh' : 'en');
      localStorage.setItem('atomcode.theme', mobile ? 'light' : 'dark');
    }, { mobile });
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(base + '/?sync=1&session=' + session.id);
    await page.locator('.model-selector-trigger').last().waitFor();
    const before = catalogueRequests;
    await page.locator('.model-selector-trigger').last().click();
    await page.locator('.model-card-info').first().waitFor();
    assert.equal(catalogueRequests, before, 'catalogue is not polled by the selector');
    const writesBefore = writes;
    await page.locator('.model-card-info').first().click();
    const dialog = page.locator('dialog.model-card-dialog');
    await dialog.locator('.mc-headings').waitFor();
    assert.ok((await dialog.locator('.mc-config').innerText()).includes('12,345'));
    assert.equal(await dialog.locator('.mc-notes').getAttribute('open'), null);
    assert.equal(await dialog.locator('a:visible').count(), 0, 'sources are not shown on the primary card');
    await dialog.locator('.mc-pickers select').nth(1).selectOption('qwen3-8-27b');
    await dialog.locator('.mc-headings.comparing').waitFor();
    assert.ok(await dialog.locator('tbody tr').count() > 5);
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), 'no page overflow on narrow view');
    await mkdir('/tmp/atomcode-model-cards', { recursive: true });
    await page.screenshot({ path: `/tmp/atomcode-model-cards/${mobile ? 'mobile' : 'desktop'}.png` });
    await dialog.locator('.mc-notes>summary').click();
    assert.ok(await dialog.locator('a:visible').count() > 0);
    if (!mobile) {
      const box = await dialog.boundingBox();
      await page.mouse.click(box.x + box.width - 2, box.y + 5);
      assert.ok(await dialog.isVisible(), 'clicking the dialog border must not dismiss it');
      await page.mouse.click(5, 5);
      assert.ok(await dialog.isVisible(), 'backdrop click must not dismiss model cards');
    }
    await page.keyboard.press('Escape');
    assert.ok(await dialog.isVisible(), 'Escape must not dismiss model cards');
    await dialog.locator('header button').click();
    await dialog.waitFor({ state: 'detached' });
    await page.locator('.model-selector-trigger').last().click();
    await page.locator('.model-card-info').nth(2).click();
    await dialog.locator('.mc-empty').waitFor();
    await dialog.locator('.mc-pickers select').first().selectOption('minimax-m3-1-flash');
    await dialog.locator('.mc-headings').waitFor();
    assert.equal((await dialog.locator('.mc-table-scroll').last().locator('tbody tr').count()), 2);
    await dialog.locator('header button').click();
    // Confirmed absent catalogue entries are not repeatedly fetched or scored as zero.
    for (let attempt = 0; attempt < 2; attempt++) {
      await page.locator('.model-selector-trigger').last().click();
      const beforeMissing = catalogueRequests;
      await page.locator('.model-card-info').nth(1).click();
      await dialog.locator('.mc-no-public').waitFor();
      assert.ok((await dialog.locator('header').innerText()).includes('lumen-alpha'));
      assert.equal(catalogueRequests, beforeMissing, 'known missing evidence does not trigger a catalogue request');
      assert.equal(await dialog.locator('table').count(), 0, 'missing evidence must not create zero-score rows');
      if (attempt === 1) {
        await dialog.locator('.mc-browse').click();
        await dialog.locator('.mc-pickers').waitFor();
        await dialog.locator('.mc-pickers select').first().selectOption('glm-5-3-flash');
        await dialog.locator('.mc-headings').waitFor();
        assert.ok(await dialog.locator('.mc-no-public').isVisible(), 'browsing another card keeps the configured model no-data state explicit');
        assert.ok((await dialog.locator('header').innerText()).includes('lumen-alpha'));
      }
      await dialog.locator('header button').click();
      await dialog.waitFor({ state: 'detached' });
    }
    if (!mobile) {
      fail = true;
      await page.locator('.model-selector-trigger').last().click();
      await page.locator('.model-card-info').first().click();
      await dialog.getByRole('alert').waitFor();
      fail = false;
      await dialog.getByRole('alert').getByRole('button').click();
      await dialog.locator('.mc-headings').waitFor();
      await dialog.locator('header button').click();
    }
    assert.equal(writes, writesBefore, 'viewing, comparison and retries must never switch or reconfigure a model');
    await context.close();
  }
  assert.deepEqual(errors, []);
  // Bootstrap can attach the initial session; card viewing itself never posts a model switch.
  console.log('model cards desktop/mobile, compare, sparse/unknown, notes, retry and keyboard tests passed');
} finally {
  await browser.close();
  for (const stream of streams) stream.end();
  await new Promise(resolve => server.close(resolve));
}
