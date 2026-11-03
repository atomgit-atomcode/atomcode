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
const other = { ...meta, id: 'browse-fixture', name: 'Other conversation' };
let switches = 0;
const clients = new Set();
let baseline = 3;
const snapshot = () => ({ type: 'snapshot', session_id: id, session_name: meta.name, project_hash: 'fixture', working_dir: '/fixture', provider: 'fixture', model: 'fixture', mode: 'build', summary_count: baseline, messages });
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path.startsWith('/workbench/')) return json([]);
  if (path === '/live/permission') return json({accepted:true});
  if (path === '/live/switch_session') { switches++; return json({ok:true}); }
  if (path === '/projects/fixture/sessions/' + other.id) return json({ ...other, messages: [{role:'user', content:'Historical question'}, {role:'assistant', content:'Historical answer'}] });
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
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json([meta, other]);
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
  await page.addInitScript(() => { localStorage.setItem('atomcode.sidebar.view.v1', 'flat'); localStorage.setItem('atomcode.lang', 'en'); localStorage.setItem('atomcode.onboarding.done', '1'); });
  await page.goto(base + '/?sync=1&session=context-summary');
  await page.locator('.message-input').waitFor();
  await page.waitForTimeout(1000);
  assert(clients.size > 0);
  emit({type:'state', running:true});
  try { await page.getByText('Running', {exact:false}).first().waitFor({timeout:5000}); } catch (e) { console.log(await page.locator('body').innerText()); throw e; }
  const before = switches;
  if (width < 600) await page.locator('.mobile-menu-btn').click();
  await page.locator('.session-item-main').filter({hasText:'Other conversation'}).click();
  await page.getByText('Historical answer', {exact:true}).waitFor();
  assert.equal(switches, before);
  assert.equal(await page.locator('.message-input').count(), 0);
  assert.equal(await page.getByRole('button', {name:'Continue this session', exact:true}).isDisabled(), true);
  emit({type:'text', content:' Background stream stays with A.'});
  emit({type:'permission_request', tool_name:'bash', reason:'Approval from running A', call_id:'browse-call', arguments:'{"command":"pwd"}'});
  await page.getByText('Approval from running A', {exact:true}).waitFor();
  await page.getByRole('button', {name:'Approve', exact:true}).click();
  await page.getByText('Approval from running A', {exact:true}).waitFor({state:'detached'});
  emit({type:'user_input_request', request_id:42, header:'Question from A', question:'Give your answer', mode:'text', options:[]});
  await page.getByText('Give your answer', {exact:true}).waitFor();
  const answer = page.locator('.interaction-dock-seat textarea');
  await answer.fill('Keep this answer');
  await page.getByRole('button', {name:'Return to Context summary', exact:true}).click();
  assert.equal(await page.locator('.interaction-dock-seat textarea').inputValue(), 'Keep this answer');
  if (width < 600) await page.locator('.mobile-menu-btn').click();
  await page.locator('.session-item-main').filter({hasText:'Other conversation'}).click();
  assert.equal(await page.locator('.interaction-dock-seat textarea').inputValue(), 'Keep this answer');
  emit({type:'user_input_resolved', request_id:42});
  await page.getByText('Give your answer', {exact:true}).waitFor({state:'detached'});
  emit({type:'state', running:false});
  await page.waitForFunction(() => ![...document.querySelectorAll('button')].find(b => b.textContent === 'Continue this session')?.disabled);
  assert.equal(await page.getByText('Historical answer', {exact:true}).count(), 1);
  await page.getByRole('button', {name:'Return to Context summary', exact:true}).click();
  await page.locator('.message-input').waitFor();
  await page.getByText('The resource read boundary is ready.', {exact:false}).waitFor();
  assert.equal(await page.getByText('Background stream stays with A.', {exact:false}).count(), 1);
  assert.equal(switches, before);
  await page.getByRole('button', {name:'Workspace', exact:true}).click();
  const panel = page.getByRole('complementary', {name:'Workspace panel'});
  await panel.waitFor();
  const layout = await page.evaluate(() => {
    const messages = document.querySelector('.session-chat-column .messages-container').getBoundingClientRect();
    const composer = document.querySelector('.session-chat-column .input-container').getBoundingClientRect();
    return {messagesRight:messages.right, composerLeft:composer.left, messagesBottom:messages.bottom, composerTop:composer.top};
  });
  const rail = await page.locator('.turn-nav-rail').boundingBox();
  assert(rail.x + rail.width <= layout.messagesRight, 'turn navigation must stay inside the chat column');
  assert(layout.composerLeft < layout.messagesRight, 'composer must share the chat column');
  assert(layout.composerTop >= layout.messagesBottom - 2, 'composer must sit below messages');
  const splitBounds = await panel.boundingBox();
  await panel.evaluate(el => { el.dataset.retainedInstance = 'yes'; });
  await page.screenshot({path:`/tmp/atomcode-workbench-split-${width}.png`});
  await page.getByRole('button', {name:'Fullscreen workspace', exact:true}).click();
  const bounds = await panel.boundingBox();
  assert.equal(bounds.x, 0); assert.equal(bounds.y, 0); assert.equal(bounds.width, width); assert.equal(bounds.height, 900);
  await page.screenshot({path:`/tmp/atomcode-workbench-fullscreen-${width}.png`});
  await page.keyboard.press('Escape');
  await page.getByRole('button', {name:'Fullscreen workspace', exact:true}).waitFor();
  assert.equal((await panel.boundingBox()).width, splitBounds.width);
  assert.equal(await panel.getAttribute('data-retained-instance'), 'yes');
  await page.getByRole('button', {name:'Fullscreen workspace', exact:true}).click();
  await page.getByRole('button', {name:'Exit workspace fullscreen', exact:true}).click();
  await page.getByRole('button', {name:'Close workspace panel', exact:true}).click();
  await panel.waitFor({state:'detached'});
  await page.screenshot({path:`/tmp/atomcode-chat-layout-${width}.png`});
  assert.deepEqual(errors, []);
  await page.close();
 }
  console.log('session browsing: history while running, return, completion retention, no rebind: passed');
} finally {
  await browser.close();
  for (const client of clients) client.end();
  await new Promise(resolve => server.close(resolve));
}
