// Real app, synthetic native SSE. No provider or user session is accessed.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const id = 'context-summary-fixture';
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
const browser = await chromium.launch({ executablePath: process.env.CONTEXT_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const emit = event => { for (const client of clients) client.write(`data: ${JSON.stringify(event)}\n\n`); };
const errors = [];
try {
  for (const width of [1200, 390]) {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => { localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.onboarding.done', '1'); });
    baseline = 3;
    await page.goto(base + '/?sync=1&session=context-summary');
    const footer = page.locator('.input-footer');
    if (width < 600) await page.locator('.message-input').click();
    try { await footer.getByText('Summarized 3 times', { exact: false }).waitFor({ timeout: 5000 }); } catch (error) { await page.screenshot({path:'/tmp/context-summary-failure.png', fullPage:true}); console.log((await page.locator('body').innerText()).slice(0,1800)); throw error; }
    emit({ type: 'tokens', prompt: 420000, completion: 100, total: 420100 });
    emit({ type: 'context_compacting', active: true });
    await page.getByRole('status').filter({ hasText: 'Summarizing context' }).waitFor();
    assert(!(await footer.textContent()).includes('Summarizing'));
    emit({ type: 'context_summary', count: 4, record: true });
    await footer.getByText('Summarized 4 times', { exact: false }).waitFor();
    emit({ type: 'context_summary', count: 4, record: true });
    await page.getByText('Context summarized · #4', { exact: true }).waitFor();
    assert.equal(await page.getByText('Context summarized · #4', { exact: true }).count(), 1);
    await footer.getByRole('button', { name: 'Summarized 4 times', exact: false }).click();
    assert.equal(await page.getByRole('status').filter({ hasText: 'Summarizing context' }).count(), 0);
    if (width < 600) await page.locator('.message-input').click();
    await page.screenshot({ path: `/tmp/atomcode-context-summary-${width}.png`, fullPage: true });
    baseline = 4;
    await page.reload();
    if (width < 600) await page.locator('.message-input').click();
    await footer.getByText('Summarized 4 times', { exact: false }).waitFor();
    assert.equal(await page.getByText('Context summarized · #4', { exact: true }).count(), 0);
    // A late/older history response with no reading must not erase a known count.
    const unknownSnapshot = { ...snapshot() }; delete unknownSnapshot.summary_count;
    emit(unknownSnapshot);
    await footer.getByText('Summarized 4 times', { exact: false }).waitFor({ state: 'attached' });
    baseline = 0;
    emit({ ...snapshot(), session_id: 'fresh-summary-fixture', messages: [] });
    await page.waitForFunction(() => !document.querySelector('.input-footer')?.textContent.includes('Summarized'));
    await page.close();
  }
  assert.deepEqual(errors, []);
  console.log('Passed: footer count, live progress outside footer, replay deduplication, reconnect and fresh-session reset, at 1200/390px.');
} finally {
  await browser.close(); for (const client of clients) client.end(); await new Promise(resolve => server.close(resolve));
}
