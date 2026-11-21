// Mobile Safari keeps a tapped <button> unfocused and blurs the field that
// had focus. The composer must not collapse out from under that tap, or the
// selector's click is dispatched to a display:none button and the menu never
// opens. Real browser + local fixtures; no daemon or provider calls.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';

const root = resolve(import.meta.dirname, '../dist');
const catalogue = JSON.parse(await readFile(resolve(import.meta.dirname, '../../data/model-cards.json'), 'utf8'));
const meta = { id: 'mobile-collapse', name: 'Mobile collapse fixture', working_dir: '/fixture', project_hash: 'fixture', created_at: 1700000000, updated_at: 1700000000, message_count: 2 };
const models = ['first', 'second', 'third'].map(provider => ({
  provider, model: provider + '-model', provider_type: 'openai', effort_applicable: false,
  effort_levels: [], context_window: 128000, is_default: provider === 'first',
  // The model card is opened from the model menu, so the fixture needs a model
  // that the catalogue knows about.
  ...(provider === 'first' ? { model_card_id: 'glm-5-3-flash' } : {}),
}));
const history = [{ role: 'user', content: 'Original task' }, { role: 'assistant', content: 'Original output' }];
let selected = 'first';
const providerWrites = [];
const clients = new Set();
const emit = event => { for (const client of clients) client.write('data: ' + JSON.stringify(event) + '\n\n'); };
const readBody = async req => { let raw = ''; for await (const chunk of req) raw += chunk; return JSON.parse(raw || '{}'); };

const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream' });
    clients.add(res); req.on('close', () => clients.delete(res));
    for (const event of [
      { type: 'snapshot', session_id: meta.id, provider: selected, model: selected + '-model', mode: 'build', messages: history },
      { type: 'state', running: false },
    ]) res.write('data: ' + JSON.stringify(event) + '\n\n');
    return;
  }
  if (path === '/live/provider') {
    const body = await readBody(req);
    providerWrites.push(body);
    selected = body.provider;
    emit({ type: 'provider', provider: selected, model: selected + '-model' });
    return json({ ok: true });
  }
  if (path === '/live/mode') return json({ mode: 'build' });
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/models') return json(models);
  if (path === '/model-cards') return json(catalogue);
  if (path.startsWith('/sessions/resolve/')) return json(meta);
  if (path === '/projects/fixture/sessions/' + meta.id) return json({ ...meta, messages: history });
  if (path === '/chat/active' || path === '/projects' || path === '/skills') return json([]);
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json([meta]);
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw Error('path');
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream');
    res.end(await readFile(file));
  } catch { res.writeHead(404); res.end(); }
});

await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({
  executablePath: process.env.MOBILE_COLLAPSE_BROWSER
    ?? (process.platform === 'darwin' ? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' : undefined),
  headless: true,
});

// iOS Safari: the tap blurs the focused field and the tapped button never
// becomes the focused element. The blur is the tap's default action, so a
// press that cancels it keeps the field — and the composer — as it was.
const IOS_TAP_FOCUS = () => {
  const blurOnTap = event => {
    const target = event.target;
    if (!(target instanceof Element) || !target.closest('button')) return;
    if (event.defaultPrevented) return;
    const active = document.activeElement;
    if (active instanceof HTMLElement && active !== document.body) active.blur();
    event.preventDefault();
  };
  document.addEventListener('pointerdown', blurOnTap);
  document.addEventListener('mousedown', blurOnTap);
};

const errors = [];
try {
  for (const width of [390, 320]) {
    const context = await browser.newContext({ viewport: { width, height: 844 }, hasTouch: true, isMobile: true, deviceScaleFactor: 3 });
    await context.addInitScript(() => localStorage.setItem('atomcode.lang', 'en'));
    await context.addInitScript(IOS_TAP_FOCUS);
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(base + `/?sync=1&session=${meta.id}`);
    await page.locator('.message-input').waitFor();

    const composer = async () => page.evaluate(() => {
      const box = document.querySelector('.input-box');
      const visible = selector => { const el = document.querySelector(selector); return !!el && el.getBoundingClientRect().height > 0; };
      return {
        collapsed: !!box && getComputedStyle(box).display === 'grid',
        modelControls: visible('.model-controls'),
        modeSelector: visible('.mode-selector'),
        modelMenu: visible('.unified-model-selector .model-dropdown'),
        modeMenu: visible('.mode-selector .model-dropdown'),
        attachMenu: visible('.attach-popover'),
      };
    });

    assert.ok((await composer()).collapsed, `${width}px: a populated conversation starts collapsed`);

    // Tap the composer to reveal the toolbar, then tap the model entry.
    await page.locator('.message-input').tap();
    await page.waitForTimeout(120);
    assert.equal((await composer()).collapsed, false, `${width}px: the composer expands while the field has focus`);

    await page.locator('.unified-model-selector .model-selector-trigger').tap();
    await page.waitForTimeout(200);
    const afterModelTap = await composer();
    assert.equal(afterModelTap.collapsed, false, `${width}px: the composer stays expanded while the model menu is tapped`);
    assert.equal(afterModelTap.modelControls, true, `${width}px: the model controls stay reachable`);
    assert.equal(afterModelTap.modelMenu, true, `${width}px: tapping the model entry opens its menu`);
    await page.locator('.model-item').filter({ hasText: 'second-model' }).tap();
    await page.waitForTimeout(250);
    assert.equal(providerWrites.at(-1)?.provider, 'second', `${width}px: the tapped model is selected`);
    await page.keyboard.press('Escape');
    await page.waitForTimeout(120);

    // A touch click can arrive in a later task than pointerup. The old zero-
    // delay release hid the target in that gap, before its click could run.
    await page.locator('.message-input').tap();
    const trigger = page.locator('.unified-model-selector .model-selector-trigger');
    await trigger.dispatchEvent('pointerdown', { pointerType: 'touch', pointerId: 1 });
    await trigger.dispatchEvent('pointerup', { pointerType: 'touch', pointerId: 1 });
    await page.waitForTimeout(100);
    assert.equal((await composer()).modelControls, true, `${width}px: a delayed click retains its target`);
    await trigger.dispatchEvent('click');
    await page.waitForTimeout(120);
    assert.equal((await composer()).modelMenu, true, `${width}px: the delayed click opens the model menu`);
    await page.keyboard.press('Escape');
    await page.waitForTimeout(120);
    assert.equal(await page.locator('.input-box').evaluate(box => box.classList.contains('toolbar-held')), false,
      `${width}px: the completed click releases its hold`);

    // The approval mode sits in the same footer and must behave the same way.
    await page.locator('.message-input').tap();
    await page.waitForTimeout(120);
    await page.locator('.mode-selector .model-selector-trigger').tap();
    await page.waitForTimeout(200);
    const afterModeTap = await composer();
    assert.equal(afterModeTap.collapsed, false, `${width}px: the composer stays expanded while the mode menu is tapped`);
    assert.equal(afterModeTap.modeMenu, true, `${width}px: tapping the mode entry opens its menu`);
    // The mode menu closes on an outside tap or a second tap, not on Escape.
    await page.locator('.mode-selector .model-selector-trigger').tap();
    await page.waitForTimeout(200);
    assert.equal((await composer()).modeMenu, false, `${width}px: the mode menu closes again`);

    // The "+" attach menu is in the same footer and has the same trap.
    await page.locator('.message-input').tap();
    await page.waitForTimeout(120);
    await page.locator('.attach-btn').tap();
    await page.waitForTimeout(250);
    const afterAttachTap = await composer();
    assert.equal(afterAttachTap.collapsed, false, `${width}px: the composer stays expanded while the attach menu is tapped`);
    assert.equal(afterAttachTap.attachMenu, true, `${width}px: tapping the attach entry opens its menu`);
    await page.locator('.attach-btn').tap();
    await page.waitForTimeout(200);
    assert.equal((await composer()).attachMenu, false, `${width}px: the attach menu closes again`);

    // The model card opens from inside the toolbar, so it must not be laid out
    // inside the collapsed composer either.
    await page.locator('.message-input').tap();
    await page.waitForTimeout(120);
    await page.locator('.unified-model-selector .model-selector-trigger').tap();
    await page.waitForTimeout(200);
    await page.locator('.model-card-info').first().tap();
    await page.waitForTimeout(250);
    const cardDialog = page.locator('dialog.model-card-dialog');
    assert.equal(await cardDialog.count(), 1, `${width}px: the model card opens`);
    assert.ok(await cardDialog.isVisible(), `${width}px: the model card is visible`);
    const cardBox = await cardDialog.boundingBox();
    assert.ok(cardBox && cardBox.width > 0 && cardBox.height > 0, `${width}px: the model card is laid out`);
    await page.screenshot({ path: `/tmp/atomcode-mobile-collapse-${width}.png` });
    await context.close();
  }
  assert.deepEqual(errors, []);
  console.log('mobile composer: model and mode menus stay open under an iOS-style tap at 390/320px');
} finally {
  await browser.close();
  for (const client of clients) client.end();
  await new Promise(resolve => server.close(resolve));
}
