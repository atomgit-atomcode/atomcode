// Synthetic overnight history, real browser/IndexedDB, no provider or user data.
// Run after npm run build. TIMELINE_BROWSER can override the Chrome executable.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
import { stripTypeScriptTypes } from 'node:module';

const root = resolve(import.meta.dirname, '../dist');
const id = 'long-session-fixture';
const meta = { id, name: 'Overnight fixture', working_dir: '/fixture', project_hash: 'fixture', created_at: 1700000000, updated_at: 1700000001, message_count: 2002 };
const messages = [{ role: 'user', content: 'Overnight task', created_at: 1700000000000 }];
for (let i = 0; i < 1000; i++) {
  messages.push({ role: 'assistant', content: i === 5 ? 'prefix '.repeat(80) + 'needle-5' : `step ${i}`, tool_calls: [{ id: `tool-${i}`, name: 'bash', arguments: JSON.stringify({ command: `command-${i}` }) }] });
  messages.push({ role: 'tool', content: '', tool_result: { call_id: `tool-${i}`, summary: `output-${i} ` + 'data '.repeat(200), success: true } });
}
let historyMode = false;
const archivedTurn = { turn_id: 42, user: 'Archived task', assistant: '', undone: false, started_at: 1699999999000, ts: 1699999999500,
  steps: Array.from({ length: 400 }, (_, i) => ({ text: `archive-step-${i}`, reasoning: '', tools: [{ id: `archive-${i}`, name: 'bash', args: JSON.stringify({ command: `archive-command-${i}` }), result: `archive-output-${i}`, is_error: false }] })) };
const clients = new Set();
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path === '/__timeline_archive.js') {
    res.setHeader('Content-Type', 'text/javascript');
    res.end(stripTypeScriptTypes(await readFile(resolve(root, '../src/lib/timelineArchive.ts'), 'utf8'))); return;
  }
  if (path === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
    res.write(`data: ${JSON.stringify({ type: 'snapshot', session_id: id, session_name: meta.name, project_hash: 'fixture', working_dir: '/fixture', provider: 'fixture', model: 'fixture', mode: 'build', messages: historyMode ? messages.slice(0, 3) : messages })}\n\n`);
    res.write('data: {"type":"state","running":false}\n\n');
    clients.add(res); req.on('close', () => clients.delete(res)); return;
  }
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/models') return json([{ name: 'fixture', model: 'fixture', provider: 'fixture', effort_levels: [] }]);
  if (path.startsWith('/sessions/resolve/')) return json(meta);
  if (path === '/projects/fixture/sessions/' + id) return json({ ...meta, messages: historyMode ? messages.slice(0, 3) : messages });
  if (path.endsWith('/transcript')) {
    const summary = new URL(req.url, 'http://localhost').searchParams.get('summary') === 'true';
    return json({ session_id: id, turns: historyMode ? [summary ? { ...archivedTurn, steps: undefined } : archivedTurn] : [] });
  }
  if (path === '/live/mode') return json({ mode: 'build' });
  if (path === '/live/session') return json({ success: true, session_id: id });
  if (path === '/chat/active') return json([]);
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json([meta]);
  if (path === '/projects' || path === '/skills') return json([]);
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw new Error('path');
    const data = await readFile(file);
    const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.webmanifest': 'application/manifest+json' };
    res.setHeader('Content-Type', types[extname(file)] ?? 'application/octet-stream'); res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const heartbeat = setInterval(() => { for (const client of clients) client.write(': ping\n\n'); }, 15000);
const browser = await chromium.launch({ executablePath: process.env.TIMELINE_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const errors = [];
async function until(check) {
  const deadline = Date.now() + 20000;
  while (Date.now() < deadline) { if (await check()) return; await new Promise(resolve => setTimeout(resolve, 25)); }
  throw new Error('Condition did not become true');
}
try {
  for (const compact of [true, false]) {
    const context = await browser.newContext({ viewport: compact ? { width: 390, height: 844 } : { width: 1440, height: 1000 }, permissions: ['clipboard-read', 'clipboard-write'] });
    await context.addInitScript(() => {
      let copied = '';
      Object.defineProperty(navigator, 'clipboard', { value: {
        write: async items => { copied = await (await items[0].getType('text/plain')).text(); },
        writeText: async text => { copied = text; },
        readText: async () => copied,
      } });
    });
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(base + '/?sync=1&session=long-ses');
    await page.locator('.tool-body').first().waitFor();
    const bound = compact ? 12 : 32;
    assert.ok(await page.locator('.tool-body').count() <= bound);
    await until(() => page.evaluate(() => new Promise(resolve => {
      const request = indexedDB.open('atomcode.timeline-display.v2');
      request.onsuccess = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains('payloads')) { db.close(); resolve(false); return; }
        const count = db.transaction('payloads').objectStore('payloads').count();
        count.onsuccess = () => { db.close(); resolve(count.result > 800); };
      };
      request.onerror = () => resolve(false);
    })));
    const initialCommand = await page.locator('.tool-name-secondary').first().innerText();
    await page.locator('.messages-container').evaluate(node => { node.scrollTop = 0; });
    await new Promise(resolve => setTimeout(resolve, 550));
    await page.locator('.messages-container').hover(); await page.mouse.wheel(0, -300);
    await until(async () => (await page.locator('.tool-name-secondary').first().innerText()) !== initialCommand);
    for (let i = 0; i < 6; i++) {
      await page.getByRole('button', { name: /加载更早的消息|Load earlier messages/ }).click();
      await until(() => page.evaluate(() => !document.querySelector('.timeline-history-status[role="status"]')));
    }
    assert.ok(await page.locator('.tool-body').count() <= bound);
    const reading = await page.locator('.tool-name-secondary').first().innerText();
    for (const client of clients) {
      for (const event of [{ type: 'state', running: true }, { type: 'tool_start', id: 'new-live-tool', name: 'bash', arguments: { command: 'new-live-command' } },
        { type: 'tool_result', id: 'new-live-tool', name: 'bash', output: 'new-live-output', success: true, duration_ms: 1 }, { type: 'text', content: 'new-live-text' }, { type: 'state', running: false }]) {
        client.write(`data: ${JSON.stringify(event)}\n\n`);
      }
    }
    await new Promise(resolve => setTimeout(resolve, 200));
    assert.equal(await page.locator('.tool-name-secondary').first().innerText(), reading, 'live events must preserve the old reading page');
    await page.locator('.tool-header').first().click();
    assert.match(await page.locator('.tool-body-grid').first().innerText(), /output-\d+ data/);
    await page.keyboard.press('Control+f');
    await page.locator('.msg-search-input').fill('needle-5');
    await until(() => page.evaluate(() => document.querySelector('.msg-search-count')?.textContent === '1/1'));
    await page.locator('.msg-search-nav').last().click();
    await page.getByText(/needle-5/).first().waitFor();
    assert.ok(await page.locator('.tool-body').count() <= bound);
    await page.locator('.msg-search-close').click();
    await page.getByRole('button', { name: /回到最新消息|Back to latest messages/ }).click();
    await page.evaluate(() => navigator.clipboard.writeText('before-copy'));
    await page.locator('.msg-copy-btn').last().click();
    await until(() => page.evaluate(async () => { const text = await navigator.clipboard.readText(); return text.includes('output-0') && text.includes('needle-5') && text.includes('output-999'); }));
    const copied = await page.evaluate(() => navigator.clipboard.readText());
    assert.ok(copied.includes('output-0') && copied.includes('needle-5') && copied.includes('output-999'));
    if (!compact) {
      const storage = await page.evaluate(async () => {
        const { createTimelineArchive } = await import('/__timeline_archive.js');
        const first = createTimelineArchive(); const second = createTimelineArchive();
        await first.put([{ key: 'owner-first:1', value: { text: 'first' } }, { key: 'owner-future:3', value: 'staged' }]);
        await second.put([{ key: 'owner-second', value: { text: 'second' } }]);
        let isolated = false;
        try { await second.get('owner-first:1'); } catch { isolated = true; }
        const put = IDBObjectStore.prototype.put;
        let count = 0; let aborted = false;
        try {
          IDBObjectStore.prototype.put = function (...args) {
            if (this.name === 'payloads' && ++count === 2) throw new DOMException('Quota', 'QuotaExceededError');
            return put.apply(this, args);
          };
          await first.put([{ key: 'partial-first', value: 'a' }, { key: 'partial-second', value: 'b' }]);
        } catch { aborted = true; } finally { IDBObjectStore.prototype.put = put; }
        let atomic = false;
        try { await first.get('partial-first'); } catch { atomic = true; }
        const release = first.pin(['owner-first:1']);
        await first.prune(new Set(), 2);
        const pinned = (await first.get('owner-first:1')).text === 'first';
        const staged = await first.get('owner-future:3') === 'staged';
        release(); await first.prune(new Set(), 2);
        let pruned = false;
        try { await first.get('owner-first:1'); } catch { pruned = true; }
        first.close();
        const retained = (await second.get('owner-second')).text === 'second'; second.close();
        return { isolated, aborted, atomic, retained, pinned, staged, pruned };
      });
      assert.deepEqual(storage, { isolated: true, aborted: true, atomic: true, retained: true, pinned: true, staged: true, pruned: true });
    }
    await page.screenshot({ path: `/tmp/atomcode-timeline-${compact ? 'mobile' : 'desktop'}.png` });
    console.log(JSON.stringify({ device: compact ? 'mobile' : 'desktop', mountedTools: await page.locator('.tool-body').count(), copyChars: copied.length }));
    await context.close();
  }
  // In-flight /chat turns can put a thousand calls into ONE assistant message.
  {
    const context = await browser.newContext({ viewport: { width: 390, height: 844 } });
    const page = await context.newPage(); page.on('pageerror', error => errors.push(error.message));
    await page.goto(base + '/?sync=1&session=long-ses'); await page.locator('.tool-body').first().waitFor();
    const single = [{ role: 'user', content: 'Single long turn', created_at: 1700000000000 },
      { role: 'assistant', content: 'One assistant', tool_calls: Array.from({ length: 1000 }, (_, i) => ({ id: `single-${i}`, name: 'bash', arguments: JSON.stringify({ command: `single-${i}` }) })) },
      ...Array.from({ length: 1000 }, (_, i) => ({ role: 'tool', content: '', tool_result: { call_id: `single-${i}`, summary: `single-output-${i}`, success: true } }))];
    for (const client of clients) client.write(`data: ${JSON.stringify({ type: 'snapshot', session_id: id, project_hash: 'fixture', working_dir: '/fixture', messages: single })}\n\n`);
    await until(async () => (await page.locator('.tool-name-secondary').last().innerText()).includes('single-999'));
    assert.ok(await page.locator('.tool-body').count() <= 12 * 8);
    console.log('single-assistant 1,000-tool turn: mounted tools', await page.locator('.tool-body').count());
    await context.close();
  }
  // A persisted single turn must also expose its earlier steps on a gesture.
  historyMode = true;
  {
    const context = await browser.newContext({ viewport: { width: 390, height: 844 } });
    const page = await context.newPage(); page.on('pageerror', error => errors.push(error.message));
    await page.goto(base + '/?sync=1&session=long-ses'); await page.locator('.tool-body').first().waitFor();
    await page.locator('.messages-container').hover(); await page.mouse.wheel(0, -300);
    await page.locator('[data-history-turn="42"]').waitFor();
    const tools = page.locator('[data-history-turn="42"] .tool-name-secondary');
    const before = await tools.first().innerText();
    await page.locator('.messages-container').evaluate(node => { node.scrollTop = 0; });
    await new Promise(resolve => setTimeout(resolve, 550));
    await page.mouse.wheel(0, -300);
    await until(async () => (await tools.first().innerText()) !== before);
    assert.ok(await tools.count() <= 12 * 8);
    console.log('persisted 400-step turn: automatic inner paging passed');
    await context.close();
  }
  historyMode = false;
  // Storage denial must keep data available, with the same DOM bound.
  const context = await browser.newContext({ viewport: { width: 390, height: 844 } });
  await context.addInitScript(() => Object.defineProperty(window, 'indexedDB', { get() { throw new DOMException('Denied', 'SecurityError'); } }));
  const page = await context.newPage(); page.on('pageerror', error => errors.push(error.message));
  await page.goto(base + '/?sync=1&session=long-ses');
  await page.locator('.tool-body').first().waitFor();
  assert.ok(await page.locator('.tool-body').count() <= 12);
  await page.keyboard.press('Control+f'); await page.locator('.msg-search-input').fill('needle-5');
  await until(() => page.evaluate(() => document.querySelector('.msg-search-count')?.textContent === '1/1'));
  console.log('storage-denied fallback: historical search and mounted bounds passed');
  await context.close();
  assert.deepEqual(errors, []);
} finally {
  clearInterval(heartbeat); await browser.close();
  for (const client of clients) client.end();
  await new Promise(resolve => server.close(resolve));
}
