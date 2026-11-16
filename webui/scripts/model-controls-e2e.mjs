import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile, mkdir } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const dist = resolve(import.meta.dirname, '../dist');
const catalog = JSON.parse(await readFile(resolve(import.meta.dirname, '../../data/model-cards.json'), 'utf8'));
const session = { id: 'card-fixture', name: 'Model card fixture', working_dir: '/fixture', project_hash: 'fixture', message_count: 0, created_at: 1700000000, updated_at: 1700000001 };
const models = [
  { provider: 'glm', model: 'glm-5.3-flash', model_card_id: 'glm-5-3-flash', context_window: 12345, supports_vision: true, is_default: true, effort_applicable: true, reasoning_effort: 'low', effort_levels: ['low', 'high'] },
  { provider: 'unlisted', model: 'lumen-alpha', model_card_id: null, model_card_status: 'no_public_data', context_window: 9876, supports_vision: null, effort_levels: [] },
  { provider: 'unknown', model: 'private-model', model_card_id: null, context_window: 9876, supports_vision: false, effort_levels: [] },
];
let catalogueRequests = 0;
let writes = 0;
let fail = false;
const effortWrites = [];
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
  if (path === '/live/reasoning_effort') {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = JSON.parse(raw); effortWrites.push(body);
    if (fail) { res.statusCode = 409; return json({ error: 'busy', active_turn: true }); }
    models[0].reasoning_effort = body.reasoning_effort;
    return json({ ok: true });
  }
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
  for (const width of [1440, 390, 320]) {
    const mobile = width <= 600;
    models[0].reasoning_effort = 'low';
    const context = await browser.newContext({ viewport: { width, height: mobile ? 844 : 1000 }, hasTouch: mobile });
    await context.addInitScript(() => { localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.theme', 'light'); });
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(base + '/?sync=1&session=' + session.id);
    const trigger = page.locator('.unified-model-selector .model-selector-trigger');
    const openSelector = async () => {
      if (!await trigger.isVisible()) await page.locator('textarea.message-input').focus();
      await trigger.click();
    };
    await page.waitForFunction(() => document.querySelector('.model-effort-summary')?.textContent === 'Low');
    assert.equal(await page.locator('.effort-selector').count(), 0);
    await openSelector();
    const slider = page.getByRole('slider');
    await slider.waitFor();
    assert.equal(await slider.getAttribute('max'), '2', 'only default and declared low/high are offered');
    await slider.focus();
    await Promise.all([page.waitForResponse(r => r.url().endsWith('/live/reasoning_effort')), slider.press('End')]);
    await page.waitForFunction(() => { const el = document.querySelector('input[type=range]'); return el && el.getAttribute('aria-busy') !== 'true'; });
    await page.waitForFunction(() => document.querySelector('.model-effort-summary')?.textContent === 'High');
    assert.deepEqual(effortWrites.at(-1), { provider: 'glm', reasoning_effort: 'high' });
    fail = true;
    await Promise.all([page.waitForResponse(r => r.url().endsWith('/live/reasoning_effort')), slider.press('Home')]);
    await page.waitForFunction(() => document.querySelector('.model-effort-summary')?.textContent === 'High');
    await page.waitForFunction(() => document.querySelector('input[type=range]')?.getAttribute('aria-busy') !== 'true');
    if (!await slider.count()) await openSelector();
    await slider.waitFor();
    assert.equal(await slider.getAttribute('aria-valuetext'), 'High', 'failed write restores the confirmed value');
    fail = false;
    await mkdir('/tmp/atomcode-model-controls', { recursive: true });
    await page.screenshot({path: `/tmp/atomcode-model-controls/${mobile ? `mobile-${width}` : 'desktop'}.png`});
    const box = await page.locator('.model-effort-panel').boundingBox();
    const viewport = page.viewportSize();
    assert.ok(box.x >= 0 && box.x + box.width <= viewport.width && box.y >= 0 && box.y + box.height <= viewport.height, 'panel fits viewport');
    if (mobile) {
      const anchor = await trigger.boundingBox();
      assert.ok(box.y + box.height <= anchor.y, 'mobile panel is anchored above the entry');
    }
    await page.locator('.model-effort-name').click();
    assert.equal(await slider.count(), 0);
    await page.locator('.model-card-info').first().waitFor();
    assert.equal(await page.locator('.model-choice-check').count(), 1);
    await page.keyboard.press('Escape');
    assert.equal(await page.locator('.model-choice-list').count(), 0);
    assert.ok(await trigger.evaluate(el => el === document.activeElement));
    // Capability updates remove the slider and open the model list directly.
    models[0].effort_applicable = false;
    await page.waitForFunction(() => !document.querySelector('.model-effort-summary'));
    await openSelector();
    await page.locator('.model-choice-list').waitFor();
    assert.equal(await slider.count(), 0);
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
    models[0].effort_applicable = true;
    await page.keyboard.press('Escape');
    models[0].effort_levels = ['minimal', 'low', 'high'];
    await page.locator('.model-choice-list').waitFor({state:'detached'});
    await page.waitForFunction(() => !!document.querySelector('.model-effort-summary'));
    await openSelector();
    await slider.waitFor();
    assert.equal(await slider.getAttribute('max'), '3');
    await slider.focus();
    await Promise.all([page.waitForResponse(r => r.url().endsWith('/live/reasoning_effort')), slider.press('Home')]);
    await page.waitForFunction(() => document.querySelector('input[type=range]')?.getAttribute('aria-busy') !== 'true');
    if (!await slider.count()) { await openSelector(); await slider.waitFor(); }
    await slider.press('ArrowRight');
    await page.waitForFunction(() => document.querySelector('.model-effort-summary')?.textContent === 'minimal');
    assert.equal(effortWrites.at(-1).reasoning_effort, 'minimal', 'new server-declared levels preserve their order');
    models[0].effort_levels = ['low', 'high'];
    await context.close();
  }
  assert.deepEqual(errors, []);
  console.log('unified model/effort: declared levels, save, rollback, keyboard, capability, desktop/mobile passed');
} finally {
  await browser.close();
  for (const stream of streams) stream.end();
  await new Promise(resolve => server.close(resolve));
}
