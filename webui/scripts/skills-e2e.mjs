// Real WebUI, synthetic host: never submits a prompt or changes a user session.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const id = 'skills-fixture';
let failSkills = false;
let submissions = 0;
const skill = { name: 'atomcode-smoke-test:atomcode-smoke-test', description: '24 sequential smoke steps' };
const meta = { id, name: 'Context summary', working_dir: '/fixture', project_hash: 'fixture', created_at: 1700000000, updated_at: 1700000001, message_count: 2 };
const messages = [{ role: 'user', content: 'Check the resource boundary' }, { role: 'assistant', content: 'The resource read boundary is ready.' }];
const clients = new Set();
let baseline = 3;
const snapshot = () => ({ type: 'snapshot', session_id: id, session_name: meta.name, project_hash: 'fixture', working_dir: '/fixture', provider: 'fixture', model: 'fixture', mode: 'build', summary_count: baseline, messages });
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
    res.write(`data: ${JSON.stringify(snapshot())}\n\n`);
    res.write('data: {"type":"state","running":false}\n\n');
    clients.add(res); req.on('close', () => clients.delete(res)); return;
  }
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/models') return json([{ name: 'fixture', model: 'fixture', provider: 'fixture', effort_levels: [] }]);
  if (path.startsWith('/sessions/resolve/')) return json(meta);
  if (path === '/projects/fixture/sessions/' + id) return json({ ...meta, messages, summary_count: baseline });
  if (path.endsWith('/transcript')) return json({ session_id: id, turns: [] });
  if (path === '/live/mode') return json({ mode: 'build' });
  if (path === '/live/session') return json({ success: true, session_id: id });
  if (path === '/chat/active') return json([]);
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json([meta]);
  if (path === '/skills') { if (failSkills) { res.statusCode = 503; return json({ error: 'Skill fixture unavailable' }); } return json([skill]); }
  if (path === '/chat') { submissions++; return json({}); }
  if (path === '/projects') return json([]);
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw new Error('path');
    const data = await readFile(file);
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream'); res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: process.env.SKILLS_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const errors = [];
try {
  for (const width of [1200, 390]) {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => { localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.onboarding.done', '1'); });
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
    const input = page.locator('.message-input');
    await input.fill('/skills atom');
    const row = page.locator('.slash-row').filter({ hasText: skill.name });
    await row.waitFor();
    assert.equal(await page.locator('.slash-row').count(), 1);
    await input.press('ArrowDown');
    await input.press('Enter');
    assert.equal(await input.inputValue(), `/${skill.name} `);
    await input.fill(`/${skill.name}`);
    assert.equal(await input.inputValue(), `/${skill.name}`);
    await row.waitFor();
    await row.click();
    assert.equal(await input.inputValue(), `/${skill.name} `);
    // A failed refresh keeps usable cached rows, but identifies them as cached.
    await input.fill('close menu');
    await page.locator('.slash-popover').waitFor({ state: 'hidden' });
    failSkills = true;
    await input.fill('/skills atom');
    const retry = page.getByRole('button', { name: /Refresh failed/ });
    await retry.waitFor();
    assert((await retry.textContent()).includes('Skill fixture unavailable'));
    await row.waitFor();
    failSkills = false;
    await retry.click();
    await retry.waitFor({ state: 'hidden' });
    await row.waitFor();
    await page.screenshot({ path: `/tmp/atomcode-skills-${width}.png`, fullPage: true });
    await page.close();
  }
  // A new browser context has no stale cache: the first failure must still be retryable.
  const firstLoad = await browser.newPage();
  firstLoad.on('pageerror', error => errors.push(error.message));
  await firstLoad.addInitScript(() => { localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.onboarding.done', '1'); });
  failSkills = true;
  await firstLoad.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
  await firstLoad.locator('.message-input').fill('/skills atom');
  const firstRetry = firstLoad.getByRole('button', { name: /Could not load skills/ });
  await firstRetry.waitFor();
  failSkills = false;
  const recovered = firstLoad.locator('.slash-row').filter({ hasText: skill.name });
  try { await firstRetry.click({ timeout: 2000 }); }
  catch (error) {
    // Project initialization can itself invalidate and reload the shared list.
    // If that request already recovered, the disappearing retry is expected.
    if (!(await recovered.isVisible())) throw error;
  }
  await recovered.waitFor();
  await firstLoad.close();
  assert.equal(submissions, 0, 'completion must not submit a chat');
  assert.deepEqual(errors, []);
  console.log('Passed: /skills query, long plugin names, keyboard/mouse completion, cached refresh failure and retry at 1200/390px.');
} finally {
  await browser.close(); for (const client of clients) client.end(); await new Promise(resolve => server.close(resolve));
}
