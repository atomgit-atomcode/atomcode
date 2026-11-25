// Test the built WebUI with local fixture data and no model requests.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, resolve } from 'node:path';
import { chromium } from 'playwright';

const dist = resolve(import.meta.dirname, '../dist');
const hash = '0123456789abcdef';
const history = { id: 'attach-menu-fixture', name: 'Attachment fixture', project_hash: hash, working_dir: '/fixture', message_count: 2, created_at: 1, updated_at: 2 };
const messages = [{ role: 'user', content: 'Initial question' }, { role: 'assistant', content: 'Fixture answer' }];
const skills = Array.from({ length: 30 }, (_, i) => ({ name: `fixture-skill-${i}`, description: 'A skill to exercise the scrolling list' }));
const server = createServer(async (req, res) => {
  const route = new URL(req.url, 'http://localhost').pathname;
  const json = value => {
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(value));
  };
  if (route === '/project') return json({ working_dir: '/fixture', project_hash: hash });
  if (route === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (route === '/skills') return json(skills);
  if (route.startsWith('/sessions/resolve/')) return json(history);
  if (route === `/projects/${hash}/sessions/${history.id}`) return json({ ...history, messages });
  if (route.endsWith('/transcript')) return json({ session_id: history.id, turns: [] });
  if (['/sessions', '/projects', '/chat/active', '/models'].includes(route) || route.startsWith('/projects/')) return json([]);
  try {
    const file = resolve(dist, route === '/' ? 'index.html' : '.' + route);
    if (!file.startsWith(dist + '/')) throw new Error('Outside dist');
    const content = await readFile(file);
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream');
    res.end(content);
  } catch {
    res.writeHead(404).end();
  }
});

await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({
  executablePath: process.env.ATTACH_MENU_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  headless: true,
});

try {
  const open = async (height, session = false, width = 1366) => {
    const page = await browser.newPage({ viewport: { width, height }, hasTouch: width <= 480 });
    await page.addInitScript(() => {
      localStorage.setItem('atomcode.lang', 'en');
      localStorage.setItem('atomcode.onboarding.done', '1');
    });
    await page.goto(base + (session ? '/?session=' + history.id.slice(0, 8) : '/'));
    if (session) await page.getByText('Fixture answer').waitFor();
    const trigger = page.locator('.attach-btn');
    if (width <= 480) await trigger.tap();
    else await trigger.click();
    const menu = page.locator('.attach-popover');
    await menu.locator('.attach-skill-row').first().waitFor({ state: 'attached' });
    return { page, trigger, menu };
  };

  for (const height of [1000, 768, 614, 500]) {
    const { page, trigger, menu } = await open(height);
    const box = await menu.boundingBox();
    assert(box.y >= 0 && box.y + box.height <= height, `popover overflows the ${height}px viewport`);
    const fixedRows = menu.locator('.attach-fixed-rows .attach-row');
    assert.equal(await fixedRows.count(), 4);
    for (const row of await fixedRows.all()) {
      assert(await row.evaluate(button => {
        const box = button.getBoundingClientRect();
        const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
        return button.contains(hit);
      }), `a fixed action is clipped at ${height}px`);
    }
    assert(await menu.locator('.attach-skill-list').evaluate(list => list.scrollHeight > list.clientHeight));
    if (height === 1000) {
      const anchor = await trigger.boundingBox();
      assert(box.y + box.height <= anchor.y, 'tall viewport keeps the upward placement');
      await page.setViewportSize({ width: 1366, height: 500 });
      await page.waitForFunction(() => {
        const menu = document.querySelector('.attach-popover');
        if (!menu) return false;
        const box = menu.getBoundingClientRect();
        return box.top >= 0 && box.bottom <= innerHeight;
      });
    }
    await page.close();
  }

  const tiny = await open(320, false, 360);
  assert(await tiny.menu.evaluate(menu => menu.scrollHeight > menu.clientHeight));
  const tinyRows = tiny.menu.locator('.attach-fixed-rows .attach-row');
  const last = tinyRows.last();
  await last.scrollIntoViewIfNeeded();
  assert(await last.evaluate(button => {
    const box = button.getBoundingClientRect();
    return button.contains(document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2));
  }));
  const upload = tinyRows.first();
  const chooser = tiny.page.waitForEvent('filechooser');
  await upload.click();
  assert.equal(await (await chooser).element().getAttribute('accept'), 'image/*');
  await tiny.menu.waitFor({ state: 'detached' });
  await tiny.page.close();

  const conversation = await open(450, true);
  const bounds = await conversation.menu.boundingBox();
  assert(bounds.y >= 0 && bounds.y + bounds.height <= 450);
  await conversation.menu.locator('.attach-skill-row').first().click();
  assert.match(await conversation.page.locator('.message-input').inputValue(), /^\/fixture-skill-/);
  await conversation.trigger.click();
  await conversation.page.keyboard.press('Escape');
  await conversation.menu.waitFor({ state: 'detached' });
  assert(await conversation.trigger.evaluate(button => document.activeElement === button));
  await conversation.trigger.click();
  await conversation.page.locator('.message-input').click();
  await conversation.menu.waitFor({ state: 'detached' });
  await conversation.page.close();
  console.log('Attachment menu: viewport fit, fixed actions, scrolling, focus and skill insertion passed');
} finally {
  await browser.close();
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
}
