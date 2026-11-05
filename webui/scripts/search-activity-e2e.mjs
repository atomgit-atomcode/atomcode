// Real app, synthetic native SSE. No provider or user session is accessed.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const id = 'search-activity-fixture';
const meta = { id, name: 'Search activity', working_dir: '/fixture', project_hash: 'fixture', created_at: 1700000000, updated_at: 1700000001, message_count: 2 };
const messages = [{ role: 'user', content: 'Check the resource boundary' }, { role: 'assistant', content: 'The resource read boundary is ready.' }];
const clients = new Set();
const baseline = 3;
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
  if (path === '/projects' || path === '/skills') return json([]);
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw new Error('path');
    const data = await readFile(file);
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream'); res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({ executablePath: process.env.SEARCH_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const emit = event => { for (const client of clients) client.write(`data: ${JSON.stringify(event)}\n\n`); };
const errors = [];
try {
  for (const width of [1200, 390]) {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => { localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.onboarding.done', '1'); });
    await page.goto(base + '/?sync=1&session=' + id);
    await page.getByText('The resource read boundary is ready.', { exact: false }).waitFor();
    emit({ type: 'state', running: true });
    emit({ type: 'tool_start', id: 'search-1', name: 'grep', arguments: { pattern: 'needle', path: 'linked' } });
    const card = page.locator('.tool-body').filter({ hasText: 'needle' });
    await card.waitFor();
    const activity = card.locator('.tool-progress');
    assert.equal(await activity.count(), 0, 'a fast search has no heartbeat yet');
    emit({ type: 'tool_progress', id: 'search-1', progress: 'Searching · 1s elapsed · linked · cancel to stop' });
    await activity.getByText('Searching · 1s elapsed · linked · cancel to stop', { exact: true }).waitFor();
    emit({ type: 'tool_progress', id: 'search-1', progress: 'Searching · 2s elapsed · linked · cancel to stop' });
    await activity.getByText('Searching · 2s elapsed · linked · cancel to stop', { exact: true }).waitFor();
    assert.equal(await activity.count(), 1, 'activity replaces the current row');
    assert.equal(await page.getByText('Searching · 1s elapsed · linked · cancel to stop', { exact: true }).count(), 0);
    await page.screenshot({ path: `/tmp/atomcode-search-activity-${width}.png` });
    emit({ type: 'tool_result', id: 'search-1', name: 'grep', output: 'No matches found under the active filters', success: true, duration_ms: 2300 });
    await activity.waitFor({ state: 'detached' });
    emit({ type: 'tool_progress', id: 'search-1', progress: 'late activity must be ignored' });
    emit({ type: 'state', running: false, stop_reason: 'completed' });
    await page.waitForTimeout(100);
    assert.equal(await page.getByText('late activity must be ignored', { exact: true }).count(), 0);
    assert.equal(await card.count(), 1, 'progress creates no new tool calls');
    // A cancel terminal can beat the individual tool result on the wire.
    emit({ type: 'state', running: true });
    emit({ type: 'tool_start', id: 'search-cancel', name: 'glob', arguments: { pattern: 'cancelled-search', path: 'linked' } });
    const cancelledCard = page.locator('.tool-body').filter({ hasText: 'cancelled-search' });
    await cancelledCard.waitFor();
    emit({ type: 'tool_progress', id: 'search-cancel', progress: 'Searching before cancellation' });
    await cancelledCard.locator('.tool-progress').waitFor();
    emit({ type: 'state', running: false, stop_reason: 'cancelled' });
    await cancelledCard.locator('.tool-progress').waitFor({ state: 'detached' });
    emit({ type: 'tool_progress', id: 'search-cancel', progress: 'late cancelled activity' });
    await page.waitForTimeout(100);
    assert.equal(await page.getByText('late cancelled activity', { exact: true }).count(), 0);
    await page.close();
  }
  assert.deepEqual(errors, []);
  console.log('PASS: scoped search activity replaces in place on desktop/mobile and disappears on completion; late updates stay hidden');
} finally {
  await browser.close();
  for (const client of clients) client.end();
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
}
