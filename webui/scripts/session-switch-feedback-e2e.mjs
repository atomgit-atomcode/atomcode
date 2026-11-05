// Real app with synthetic HTTP/SSE; no user session or provider calls.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const original = {id:'switch-original', name:'Original session', working_dir:'/fixture', project_hash:'fixture', created_at:1700000000, updated_at:1700000001, message_count:2};
const target = {...original, id:'switch-occupied', name:'Occupied session'};
const copy = {...target, id:'switch-copy', name:'Copied session'};
const metadata = id => [original,target,copy].find(row => row.id === id) ?? original;
const messages = [{role:'user',content:'Persisted question'}, {role:'assistant',content:'Persisted answer'}];
let mode = 'reject';
let current = original.id;
let switches = 0;
const server = createServer(async (req,res) => {
  const url = new URL(req.url,'http://localhost');
  const path = url.pathname;
  const json = value => {res.setHeader('Content-Type','application/json');res.end(JSON.stringify(value));};
  if(path === '/live/switch_session') {
    let body='';for await(const chunk of req) body+=chunk;
    const {session_id:id} = JSON.parse(body);switches++;
    if(id === target.id) {
      if(mode === 'reject') return json({ok:false,active_turn:false,error:'session is already in use by another runtime'});
      if(mode === 'http') {res.statusCode=409;return json({error:'Fixture session lease refused',code:'session_in_use'});}
      current=copy.id;return json({ok:true,session_id:copy.id,forked_from:target.id});
    }
    current=id;return json({ok:true,session_id:id});
  }
  if(path === '/live') {
    res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(`data: ${JSON.stringify({type:'snapshot',session_id:current,session_name:metadata(current).name,project_hash:'fixture',working_dir:'/fixture',provider:'fixture',model:'fixture',mode:'build',messages})}\n\n`);
    res.write('data: {"type":"state","running":false}\n\n');return;
  }
  if(path === '/project') return json({working_dir:'/fixture',project_hash:'fixture'});
  if(path === '/config') return json({providers:[],notifications:{enabled:false}});
  if(path === '/models') return json([{name:'fixture',model:'fixture',provider:'fixture',effort_levels:[]}]);
  if(path.startsWith('/sessions/resolve/')) return json(metadata(path.split('/').pop()));
  if(path.endsWith('/transcript')) return json({session_id:current,turns:[]});
  if(path.startsWith('/projects/fixture/sessions/')) return json({...metadata(path.split('/').pop()),messages});
  if(path === '/live/mode') return json({mode:'build'});
  if(path === '/live/session') return json({success:true,session_id:current});
  if(path === '/sessions' || path === '/projects/fixture/sessions') return json([original,target]);
  if(path === '/projects' || path === '/skills' || path === '/chat/active') return json([]);
  try {
    const file=resolve(root,path === '/' ? 'index.html' : '.'+path);
    if(!file.startsWith(root+'/')) throw new Error('path');
    res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)] ?? 'application/octet-stream');
    res.end(await readFile(file));
  }catch{res.writeHead(404);res.end();}
});
await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
try {
  for(const width of [1200,390]) for(const scenario of ['reject','fork','http']) {
    mode=scenario;current=original.id;
    const page=await browser.newPage({viewport:{width,height:900}});
    await page.addInitScript(() => {localStorage.setItem('atomcode.sidebar.view.v1','flat');localStorage.setItem('atomcode.lang','en');});
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=switch-original`);
    await page.getByText('Persisted answer',{exact:true}).waitFor({timeout:10000}).catch(async error => {console.log(await page.locator('body').innerText());throw error;});
    const before=switches;
    if(width<600) await page.locator('.mobile-menu-btn').click();
    await page.locator('.session-item-main').filter({hasText:'Occupied session'}).click();
    const notice=page.locator('.session-switch-notice');
    await notice.waitFor();
    if(scenario === 'fork') {
      await page.waitForFunction(() => document.querySelector('.session-switch-notice')?.textContent.includes('forked'));
      await page.getByText('Persisted answer',{exact:true}).waitFor();
      assert.match(await notice.innerText(),/forked/);
    } else if(scenario === 'reject') {
      await page.waitForFunction(() => document.querySelector('.session-switch-notice')?.textContent.includes('another atomcode'));
      await page.getByText('Persisted answer',{exact:true}).waitFor();
      assert.match(await notice.innerText(),/another atomcode/);
    } else {
      await page.waitForFunction(() => document.querySelector('.session-switch-notice')?.textContent.includes('Fixture session lease refused'));
    }
    assert(switches>before);
    await page.evaluate(() => Promise.all(document.getAnimations()
      .filter(animation => animation.effect?.getTiming().iterations !== Infinity)
      .map(animation => animation.finished.catch(() => {}))));
    await page.screenshot({path:`/tmp/atomcode-session-switch-${scenario}-${width}.png`});
    await notice.getByRole('button').click();
    await notice.waitFor({state:'detached'});
    await page.close();
  }
  console.log('Session rejection rollback, fork history and structured HTTP errors remain visible');
} finally {await browser.close();server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
