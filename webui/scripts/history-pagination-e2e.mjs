// Real WebUI against a bounded synthetic server. No user/provider data.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const id = 'history-pages';
const meta = { id, name:'Paged history', working_dir:'/fixture', project_hash:'fixture', created_at:1700000000000, updated_at:1700000000001, message_count:50 };
const full = Array.from({length:25}, (_,i) => [
  {role:'user',content:`PAGE_PROMPT_${i}`, source_seq:i*4+2, created_at:1700000000000+i*1000},
  {role:'assistant',content:`PAGE_ANSWER_${i}`,created_at:1700000000000+i*1000},
]).flat();
const browse = {...meta,id:'read-only-pages',name:'Paged reader'};
const browseFull = full.map(message=>({...message,content:message.content.replace('PAGE_','BROWSE_')}));
const cursor = before => before > 0 ? {session:`fixture/${id}`,before,digest:`fixture-${before}`} : undefined;
let clients = new Set();
let pages = [];
let transcriptReads = 0;
let hold = null;
let pending = null;
const snapshot = () => ({type:'snapshot',session_id:id,session_name:meta.name,project_hash:'fixture',working_dir:'/fixture',provider:'fixture',model:'fixture',mode:'build',messages:full.slice(34),history_older:cursor(34)});
const server = createServer(async (req,res) => {
  const url = new URL(req.url,'http://localhost');
  const path = url.pathname;
  const json = data => {res.setHeader('Content-Type','application/json');res.end(JSON.stringify(data));};
  if(path==='/live') {
    assert.equal(url.searchParams.get('recent'),'true');
    res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(`data: ${JSON.stringify(snapshot())}\n\ndata: {"type":"state","running":false}\n\n`);
    clients.add(res);req.on('close',()=>clients.delete(res));return;
  }
  if(path===`/projects/fixture/sessions/${browse.id}`) {
    const c=url.searchParams.get('before');
    const end=c?JSON.parse(c).before:34;
    const start=c?Math.max(0,end-16):34;
    return json({...browse,messages:browseFull.slice(start,c?end:50),history_older:start?{session:`fixture/${browse.id}`,before:start,digest:`browse-${start}`}:undefined});
  }
  if(path===`/projects/fixture/sessions/${id}`) {
    const before = url.searchParams.get('before');
    if(before) {
      const c=JSON.parse(before);assert.equal(c.session,`fixture/${id}`);pages.push(c.before);
      const start=Math.max(0,c.before-16);
      const answer=()=>json({...meta,messages:full.slice(start,c.before),history_older:cursor(start)});
      if(hold) {pending=answer;hold();return;} return answer();
    }
    assert.equal(url.searchParams.get('recent'),'true');
    return json({...meta,messages:full.slice(34),history_older:cursor(34)});
  }
  if(path==='/project')return json({working_dir:'/fixture',project_hash:'fixture'});
  if(path==='/config')return json({providers:[],notifications:{enabled:false}});
  if(path==='/models')return json([{name:'fixture',model:'fixture',provider:'fixture',effort_levels:[]}]);
  if(path.startsWith('/sessions/resolve/'))return json(path.endsWith(browse.id)?browse:meta);
  if(path.endsWith('/transcript')){transcriptReads++;return json({session_id:id,turns:[]});}
  if(path==='/live/mode')return json({mode:'build'});
  if(path==='/live/session')return json({success:true,session_id:id});
  if(path==='/chat/active')return json([]);
  if(path==='/sessions'||path==='/projects/fixture/sessions')return json([meta,browse]);
  if(path==='/projects'||path==='/skills'||path.startsWith('/workbench/'))return json([]);
  try {
    const file=resolve(root,path==='/'?'index.html':'.'+path);if(!file.startsWith(root+'/'))throw Error('path');
    res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)]??'application/octet-stream');res.end(await readFile(file));
  }catch{res.writeHead(404);res.end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
const emit=e=>{for(const c of clients)c.write(`data: ${JSON.stringify(e)}\n\n`);};
const errors=[];
try {
  const page=await browser.newPage({viewport:{width:1200,height:900}});
  page.on('pageerror',e=>errors.push(e.message));
  await page.addInitScript(()=>{localStorage.setItem('atomcode.sidebar.view.v1','flat');localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
  await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
  await page.locator('.message-input').waitFor();
  await page.getByText('PAGE_ANSWER_24',{exact:true}).waitFor();
  assert.equal(await page.getByText('PAGE_PROMPT_0',{exact:true}).count(),0);
  assert.equal(pages.length,0,'no eager older-page fetch');
  // Hold the old page, append a live reply, then release. It must survive.
  let requested;const waiting=new Promise(resolve=>requested=resolve);hold=requested;
  for (let attempt=0;attempt<20 && !pending;attempt++) {
    await page.locator('.messages-container').last().evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new WheelEvent('wheel',{deltaY:-120,bubbles:true}));});
    await page.waitForTimeout(100);
  }
  await waiting;
  emit({type:'user',text:'LIVE_PROMPT'});emit({type:'state',running:true});emit({type:'text',content:'LIVE_REPLY_SURVIVES'});
  await page.getByText('LIVE_REPLY_SURVIVES',{exact:true}).waitFor();
  pending();pending=null;hold=null;
  await page.waitForTimeout(600);
  assert.equal(pages.length,1,'one request in flight');
  assert.equal(await page.getByText('LIVE_REPLY_SURVIVES',{exact:true}).count(),1);
  // Scroll through local mounted windows and server pages to the oldest turn.
  for(let i=0;i<25 && !await page.getByText('PAGE_PROMPT_0',{exact:true}).count();i++) {
    await page.locator('.messages-container').last().evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new WheelEvent('wheel',{deltaY:-120,bubbles:true}));});
    await page.waitForTimeout(550);
  }
  await page.getByText('PAGE_PROMPT_0',{exact:true}).waitFor();
  assert.deepEqual(pages,[34,18,2]);
  assert.equal(await page.getByText('PAGE_PROMPT_0',{exact:true}).count(),1);
  const exhaustedPages=pages.length;
  for(let i=0;i<3;i++) {
    await page.locator('.messages-container').last().evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new WheelEvent('wheel',{deltaY:-120,bubbles:true}));});
    await page.waitForTimeout(150);
  }
  const exhaustedReads=transcriptReads;
  await page.locator('.messages-container').last().evaluate(el=>el.dispatchEvent(new WheelEvent('wheel',{deltaY:-120,bubbles:true})));
  await page.waitForTimeout(200);
  assert.equal(pages.length,exhaustedPages,'no native reads after oldest page');
  assert.equal(transcriptReads,exhaustedReads,'empty transcript index is not fetched repeatedly');
  assert.equal(await page.getByRole('button',{name:'Load earlier messages',exact:true}).count(),0);
  emit(snapshot());
  await page.waitForTimeout(300);
  assert.equal(await page.getByText('PAGE_PROMPT_0',{exact:true}).count(),1,'reconnect retained already fetched pages');
  // A replacement snapshot cancels an older read; its late response cannot
  // splice a stale window into the refreshed view.
  await page.reload();
  await page.getByText('PAGE_ANSWER_24',{exact:true}).waitFor();
  let started;const oldRead=new Promise(resolve=>started=resolve);hold=started;
  for (let attempt=0;attempt<20 && !pending;attempt++) {
    await page.locator('.messages-container').last().evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new WheelEvent('wheel',{deltaY:-120,bubbles:true}));});
    await page.waitForTimeout(100);
  }
  await oldRead;
  emit(snapshot());
  await page.waitForTimeout(250);
  pending();pending=null;hold=null;
  await page.waitForTimeout(550);
  assert.equal(await page.getByText('PAGE_PROMPT_9',{exact:true}).count(),0,'late page was ignored');
  emit({type:'state',running:true});
  await page.waitForTimeout(200);
  await page.locator('.session-item-main').filter({hasText:browse.name}).click();
  await page.getByText('BROWSE_ANSWER_24',{exact:true}).waitFor();
  assert.equal(await page.getByText('BROWSE_PROMPT_0',{exact:true}).count(),0);
  for(let i=0;i<6 && !await page.getByText('BROWSE_PROMPT_0',{exact:true}).count();i++) {
    await page.locator('.messages-container').last().evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new WheelEvent('wheel',{deltaY:-120,bubbles:true}));});
    await page.waitForTimeout(200);
  }
  await page.getByText('BROWSE_PROMPT_0',{exact:true}).waitFor();
  assert.equal(await page.getByText('BROWSE_PROMPT_0',{exact:true}).count(),1);
  assert.deepEqual(errors,[]);
  console.log('PASS: recent-only bootstrap, ordered lazy pages, no duplicates, live output preserved');
} finally {
  for(const c of clients)c.end();await browser.close();server.closeAllConnections();server.close();
}
