// Real app, synthetic native SSE. No provider or user session is accessed.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const id = 'turn-anchor-fixture';
const meta = { id, name: 'Turn anchor', working_dir: '/fixture', project_hash: 'fixture', created_at: 1700000000, updated_at: 1700000001, message_count: 2 };
const messages = Array.from({length: 12}, (_, i) => [{role:'user',content:'Old question '+i},{role:'assistant',content:'Old answer '+i+'\n\n'+ 'History text. '.repeat(60)}]).flat();
const clients = new Set();
const chatClients = new Set();
const snapshot = () => ({ type: 'snapshot', session_id: id, session_name: meta.name, project_hash: 'fixture', working_dir: '/fixture', provider: 'fixture', model: 'fixture', mode: 'build', messages });
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
    res.write(`data: ${JSON.stringify(snapshot())}\n\n`);
    res.write('data: {"type":"state","running":false}\n\n');
    clients.add(res); req.on('close', () => clients.delete(res)); return;
  }
  if (path === '/chat') {
    res.writeHead(200, {'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(': connected\n\n');
    chatClients.add(res); res.on('close', () => chatClients.delete(res)); return;
  }
  if (path === '/live/message') return json({accepted:true,disposition:'started',generation:1,turn_id:13});
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/models') return json([{ name: 'fixture', model: 'fixture', provider: 'fixture', effort_levels: [] }]);
  if (path.startsWith('/sessions/resolve/')) return json(meta);
  if (path === '/projects/fixture/sessions/' + id) return json({ ...meta, messages });
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
const browser = await chromium.launch({ executablePath: process.env.TURN_ANCHOR_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const emit = event => { for (const client of [...clients, ...chatClients]) client.write(`data: ${JSON.stringify(event)}\n\n`); };
const errors = [];
try {
 for (const sync of [true, false]) for (const viewport of [{width:1200,height:900},{width:390,height:844},{width:844,height:390}]) {
  const page = await browser.newPage({viewport, hasTouch: viewport.width < 900});
  page.on('pageerror', error => errors.push(error.message));
  await page.addInitScript(() => { localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1'); });
  await page.goto(base+`/?sync=${sync ? 1 : 0}&session=turn-anchor`);
  const input=page.locator('.message-input');
  await input.waitFor();
  await page.waitForTimeout(300);
  await input.fill('New anchored question');
  await page.locator('.btn-send').click();
  await page.locator('[data-turn-anchor]').waitFor();
  const metrics=()=>page.locator('.messages-container').evaluate(el=>{
    const anchor=el.querySelector('[data-turn-anchor]');
    return {offset:anchor.getBoundingClientRect().top-el.getBoundingClientRect().top,top:el.scrollTop,gap:el.scrollHeight-el.clientHeight-el.scrollTop,padding:parseFloat(getComputedStyle(el).paddingTop),space:el.querySelector('.turn-reply-space').getBoundingClientRect().height};
  });
  await page.waitForTimeout(200);
  let m=await metrics();
  assert.ok(Math.abs(m.offset-m.padding)<4,JSON.stringify({viewport,m}));
  emit({type:'text',content:'Short reply.'});
  await page.waitForTimeout(100);
  await page.getByText('Short reply.', {exact:true}).waitFor();
  m=await metrics();assert.ok((m.space === 0 && m.gap <= 2) || Math.abs(m.offset-m.padding)<4,JSON.stringify({viewport,m}));
  await page.setViewportSize({width:viewport.width,height:Math.max(320,viewport.height-220)});
  await page.waitForTimeout(150);
  m=await metrics();assert.ok((m.space === 0 && m.gap <= 2) || Math.abs(m.offset-m.padding)<4,JSON.stringify({viewport,m}));
  const el=page.locator('.messages-container');
  await el.hover();await page.mouse.wheel(0,-120);await page.waitForTimeout(100);
  const before=await metrics();
  assert.ok(before.gap > 2, 'the gesture must leave the bottom');
  emit({type:'text',content:' More streamed text.'});await page.waitForTimeout(100);
  assert.ok(Math.abs((await metrics()).top-before.top)<2,'manual reading must survive streaming');
  if (!sync) {
    await input.fill('Queued follow-up');
    await page.locator('.btn-send').click();
    await page.locator('.user-message-wrapper.queued').waitFor();
    assert.ok(Math.abs((await metrics()).top-before.top)<2,'queueing preserves the reader position');
    await page.locator('.user-message-wrapper.queued button').evaluate(button => button.click());
  }
  await page.locator('.jump-to-bottom').click();
  await page.waitForFunction(() => {
    const el=document.querySelector('.messages-container');
    return el.scrollHeight-el.clientHeight-el.scrollTop <= 2;
  });
  emit({type:'text',content:'\n\n'+'Long streamed reply.\n\n'.repeat(80)});
  await page.waitForTimeout(150);
  assert.ok((await metrics()).gap <= 2,'returning to the bottom follows a reply longer than one screen');
  emit(sync ? {type:'state',running:false} : {type:'done',session_id:id,stop_reason:'end_turn'});
  await page.waitForTimeout(100);
  await input.fill('Long prompt\n'+'A line of requirements\n'.repeat(80));
  await page.locator('.btn-send').click();
  await page.waitForTimeout(200);
  const long=await page.locator('.messages-container').evaluate(el=>{
    const anchors=el.querySelectorAll('[data-turn-anchor]');
    return anchors[anchors.length-1].getBoundingClientRect().top-el.getBoundingClientRect().top-parseFloat(getComputedStyle(el).paddingTop);
  });
  assert.ok(Math.abs(long)<4,'long prompt opens at the beginning: '+long);
  await page.screenshot({path:'/tmp/atomcode-turn-anchor-'+sync+'-'+viewport.width+'.png'});
  await page.close();
 }
 assert.deepEqual(errors,[]);
 console.log('Turn anchor desktop/mobile/keyboard resize/manual scroll passed');
} finally { await browser.close(); for(const c of [...clients,...chatClients])c.end(); await new Promise(resolve=>server.close(resolve)); }
