// Real app, synthetic native SSE. No provider or user session is accessed.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const id = 'context-summary-fixture';
const meta = { id, name: 'Context summary', working_dir: '/fixture', project_hash: 'fixture', created_at: 1700000000, updated_at: 1700000001, message_count: 2 };
const specificationText = 'MUST NOT change public API\r\nconst exact = 42;';
const messages = [{ role: 'user', content: specificationText, source_seq:12, task_spec_pinned:false }, { role: 'assistant', content: 'The resource read boundary is ready.' }];
const other = { ...meta, id: 'browse-fixture', name: 'Other conversation' };
let switches = 0;
let specificationPinned = false;
const patchUploads = [];
let patchReplyMode = 'ok';
const pendingPatchReplies = [];
const patchReference = JSON.stringify({artifact_id:'f'.repeat(64), patch_sha256:'a'.repeat(64), file_paths:['unicode file.txt']});
const terminalRequests = [];
const clients = new Set();
let baseline = 3;
const snapshot = () => ({ type: 'snapshot', session_id: id, session_name: meta.name, project_hash: 'fixture', working_dir: '/fixture', provider: 'fixture', model: 'fixture', mode: 'build', summary_count: baseline, messages });
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path === '/live/spec') {
    let body='';for await (const chunk of req) body+=chunk;
    const input=JSON.parse(body);assert.equal(input.session_id,id);
    if(input.args==='pin 12'){specificationPinned=true;messages[0].task_spec_pinned=true;}
    if(input.args==='unpin 12'){specificationPinned=false;messages[0].task_spec_pinned=false;}
    if(input.args.startsWith('pin ')||input.args.startsWith('unpin '))return json({seq:12,pinned:specificationPinned});
    if(input.args==='show 12')return json({seq:12,text:specificationText,pinned:specificationPinned,bytes:specificationText.length,pinned_bytes:specificationPinned?specificationText.length:0,budget_bytes:8192});
    return json({session_id:id,items:[{seq:12,turn:2,at:1700000000000,bytes:specificationText.length,preview:specificationText,pinned:specificationPinned}],pinned_bytes:specificationPinned?specificationText.length:0,budget_bytes:8192});
  }
  if (path === '/live/patch') {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    patchUploads.push({session:new URL(req.url, 'http://localhost').searchParams.get('session_id'), bytes:Buffer.concat(chunks)});
    if (patchReplyMode === 'fail') { res.statusCode = 409; return json({error:'Fixture import refused'}); }
    if (patchReplyMode === 'hold') { pendingPatchReplies.push(() => json({reference:patchReference})); return; }
    return json({reference:patchReference});
  }
  if (path === '/workbench/terminals' && req.method === 'POST') {
    let body = ''; for await (const chunk of req) body += chunk;
    terminalRequests.push(JSON.parse(body));
    res.statusCode = 403; return json({error:'Terminal creation denied for fixture'});
  }
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
  await panel.getByRole('heading', {name:'Project workspace', exact:true}).waitFor();
  assert.equal(await panel.locator('.workbench-home-options button').count(), 3);
  await panel.locator('.workbench-home-options').getByRole('button', {name:'New terminal', exact:false}).click();
  await panel.getByRole('alert').waitFor();
  assert.equal(terminalRequests.at(-1).session_id, id);
  assert.equal(await panel.locator('.workbench-home').count(), 1);
  assert.equal(await panel.locator('.workbench-home-options').getByRole('button', {name:'New terminal', exact:false}).isEnabled(), true);
  await panel.locator('.workbench-home-options').getByRole('button', {name:'Files', exact:false}).click();
  await panel.locator('.workbench-tree').waitFor();
  await panel.locator('nav').getByRole('button', {name:'Start', exact:true}).click();
  await panel.locator('.workbench-home-options').getByRole('button', {name:'Git', exact:false}).click();
  await panel.locator('.workbench-home').waitFor({state:'detached'});
  await panel.locator('nav').getByRole('button', {name:'Start', exact:true}).click();

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
  await page.screenshot({path:`/tmp/atomcode-workbench-home-${width}.png`});
  await page.keyboard.press('Escape');
  await page.getByRole('button', {name:'Fullscreen workspace', exact:true}).waitFor();
  assert.equal((await panel.boundingBox()).width, splitBounds.width);
  assert.equal(await panel.getAttribute('data-retained-instance'), 'yes');
  await page.getByRole('button', {name:'Fullscreen workspace', exact:true}).click();
  await page.getByRole('button', {name:'Exit workspace fullscreen', exact:true}).click();
  await panel.locator('nav').getByRole('button', {name:'Files', exact:true}).click();
  await page.getByRole('button', {name:'Close workspace panel', exact:true}).click();
  await panel.waitFor({state:'detached'});
  await page.getByRole('button', {name:'Workspace', exact:true}).click();
  await panel.locator('.workbench-tree').waitFor();
  assert.equal(await panel.locator('.workbench-home').count(), 0);
  await page.getByRole('button', {name:'Close workspace panel', exact:true}).click();
  await page.screenshot({path:`/tmp/atomcode-chat-layout-${width}.png`});
  const composer = page.locator('.message-input');
  await composer.fill('Keep my draft');
  const rawPatch = Buffer.from('--- a/unicode file.txt\r\n+++ b/unicode file.txt\r\n@@ -1 +1 @@\r\n-旧\r\n+新\r\n');
  const upload = page.locator('input[type="file"][accept=".patch,.diff"]');
  await upload.setInputFiles({name:'中文 patch.patch', mimeType:'application/octet-stream', buffer:rawPatch});
  await page.waitForFunction(() => document.querySelector('.message-input')?.value.includes('artifact_id'));
  assert.deepEqual(patchUploads.at(-1).bytes, rawPatch);
  assert.equal(patchUploads.at(-1).session, id);
  const draft = await composer.inputValue();
  assert(draft.startsWith('Keep my draft'));
  assert(draft.includes(patchReference));
  assert(!draft.includes('--- a/unicode'), 'patch body must never enter composer/model text');
  const count = patchUploads.length;
  await upload.setInputFiles({name:'large.patch', mimeType:'application/octet-stream', buffer:Buffer.alloc(8*1024*1024+1, 65)});
  await page.getByRole('alert').filter({hasText:'Patch exceeds the 8 MiB import limit'}).waitFor();
  assert.equal(patchUploads.length, count);
  assert.equal(await composer.inputValue(), draft);
  patchReplyMode = 'fail';
  await upload.setInputFiles({name:'refused.patch', mimeType:'application/octet-stream', buffer:rawPatch});
  await page.getByRole('alert').filter({hasText:'Fixture import refused'}).waitFor();
  assert.equal(await composer.inputValue(), draft);
  patchReplyMode = 'ok';
  await upload.setInputFiles({name:'retry.patch', mimeType:'application/octet-stream', buffer:rawPatch});
  await page.waitForFunction(reference => document.querySelector('.message-input')?.value.split(reference).length === 3, patchReference);

  // A late reply must not insert a reference after leaving the live view.
  await composer.fill('Draft before leaving');
  patchReplyMode = 'hold';
  await upload.setInputFiles({name:'delayed.patch', mimeType:'application/octet-stream', buffer:rawPatch});
  for (let attempt = 0; attempt < 100 && !pendingPatchReplies.length; attempt++) await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(pendingPatchReplies.length, 1, 'delayed upload must reach server');
  emit({type:'state', running:true});
  if (width < 600) await page.locator('.mobile-menu-btn').click();
  await page.locator('.session-item-main').filter({hasText:'Other conversation'}).click();
  await page.getByText('Historical answer', {exact:true}).waitFor();
  for (const reply of pendingPatchReplies.splice(0)) reply();
  patchReplyMode = 'ok';
  await page.getByRole('button', {name:'Return to Context summary', exact:true}).click();
  await composer.waitFor();
  emit({type:'state', running:false});
  assert.equal(await composer.inputValue(), 'Draft before leaving');
  await page.getByRole('button',{name:'Pin as task specification',exact:true}).click();
  const specifications=page.getByRole('dialog',{name:'Task specifications'});
  await specifications.waitFor();
  await specifications.evaluate(async el=>{await Promise.all(el.closest('.modal-overlay').getAnimations({subtree:true}).map(animation=>animation.finished));});
  const specBounds=await specifications.boundingBox();
  await page.screenshot({path:`/tmp/atomcode-spec-picker-${width}.png`});
  assert(specBounds.x>=0&&specBounds.x+specBounds.width<=width&&specBounds.y>=0&&specBounds.y+specBounds.height<=900,'specification picker fits viewport: '+JSON.stringify(specBounds));
  assert.equal(specificationPinned,false,'opening a source does not pin automatically');
  assert.equal(await specifications.locator('pre').textContent(),specificationText);
  await page.screenshot({path:`/tmp/atomcode-spec-review-${width}.png`});
  await specifications.getByRole('button',{name:'Pin as task specification',exact:true}).click();
  await specifications.getByRole('button',{name:'Unpin task specification',exact:true}).waitFor();
  assert.equal(specificationPinned,true);
  await specifications.getByRole('button',{name:'Close',exact:true}).click();
  await specifications.waitFor({state:'detached'});
  const inlineUnpin=page.getByRole('button',{name:'Unpin task specification',exact:true});
  assert.equal(await inlineUnpin.getAttribute('aria-pressed'),'true');
  await inlineUnpin.click();
  await page.getByRole('button',{name:'Pin as task specification',exact:true}).waitFor();
  assert.equal(specificationPinned,false);

  assert.deepEqual(errors, []);
  await page.close();
 }
  console.log('session browsing: history while running, return, completion retention, no rebind: passed');
} finally {
  await browser.close();
  for (const client of clients) client.end();
  await new Promise(resolve => server.close(resolve));
}
