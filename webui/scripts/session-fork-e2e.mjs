// Real WebUI, fixture-only session service. Does not access user conversations.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const source = {id:'fork-source',name:'Original discussion',working_dir:'/fixture',project_hash:'fixture',created_at:1700000000,updated_at:1700000000,message_count:2};
const branch = {...source,id:'fork-copy',name:'Original discussion (fork)'};
const heldBranch = {...branch,id:'held-copy',name:'Held discussion (fork)'};
const messages = [{role:'user',content:'Keep this original context'}, {role:'assistant',content:'Shared conversation before the fork'}];
let current = source;
let copied = false;
let refused = false;
let requests = [];
let heldMode = false;
let releaseHeldFork;
let onHeldRequest;
let heldActivity='running';
let heldHistoryFailures=0;
let heldAnswer='Held task is still running';
let heldStarted=Date.now()-20000;
let heldCompleted=heldStarted+1000;
const clients = new Set();
const snapshot = () => ({type:'snapshot',session_id:current.id,session_name:current.name,project_hash:'fixture',working_dir:'/fixture',provider:'fixture',model:'fixture',mode:'build',messages});
const server = createServer(async(req,res) => {
  const url = new URL(req.url,'http://localhost');
  const path = url.pathname;
  const json = value => { res.setHeader('Content-Type','application/json'); res.end(JSON.stringify(value)); };
  if(path==='/command') {
    let body='';for await(const chunk of req)body+=chunk;
    const args=JSON.parse(body);requests.push(args);
    assert.equal(args.command,'fork');assert.equal(args.session_id,source.id);
    if(heldMode) {
      releaseHeldFork=()=>json({kind:'fork',session_id:heldBranch.id,source_id:source.id});
      onHeldRequest?.();
      return;
    }
    if(refused)return json({kind:'error',message:'Fork refused: session is busy'});
    copied=true;current=branch;
    for(const client of clients)client.write(`data: ${JSON.stringify({type:'session_switched',session_id:branch.id})}\n\n`);
    return json({kind:'fork',session_id:branch.id,source_id:source.id});
  }
  if(path==='/live') {
    res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(`data: ${JSON.stringify(snapshot())}\n\n`);
    res.write('data: {"type":"state","running":false}\n\n');
    clients.add(res);req.on('close',()=>clients.delete(res));return;
  }
  if(path==='/project')return json({working_dir:'/fixture',project_hash:'fixture'});
  if(path==='/config')return json({providers:[],notifications:{enabled:false}});
  if(path==='/models')return json([{name:'fixture',model:'fixture',provider:'fixture',effort_levels:[]}]);
  if(path.startsWith('/sessions/resolve/'))return json(path.endsWith(heldBranch.id)?heldBranch:path.endsWith(branch.id)?branch:source);
  if(path.endsWith('/activity'))return json({activity:heldMode?heldActivity:null});
  if(path===`/projects/fixture/sessions/${heldBranch.id}`)return json({...heldBranch,messages});
  if(path===`/projects/fixture/sessions/${source.id}`) {
    if(heldMode&&heldHistoryFailures>0){heldHistoryFailures--;res.statusCode=503;return json({error:'retry final refresh'});}
    return json({...source,messages:heldMode?[
      {role:'user',content:'Previous completed question',created_at:heldStarted-20000},
      {role:'assistant',content:'Previous completed answer',created_at:heldStarted-10000},
      {...messages[0],created_at:heldStarted},
      {role:'assistant',content:'Checkpoint inspection',tool_calls:[{id:'checkpoint-tool',name:'bash',arguments:'{"command":"pwd"}'}]},
      {role:'tool',tool_result:{call_id:'checkpoint-tool',success:true,summary:'/fixture',line_count:1}},
      // Simulate an older daemon exposing a checkpoint timestamp mid-turn.
      {role:'assistant',content:heldAnswer,created_at:heldCompleted},
    ]:messages});
  }
  if(path===`/projects/fixture/sessions/${branch.id}`)return json({...branch,messages});
  if(path.endsWith('/transcript'))return json({session_id:current.id,turns:[]});
  if(path==='/live/session')return json({success:true,session_id:current.id});
  if(path==='/live/mode')return json({mode:'build'});
  if(path==='/chat/active')return json([]);
  if(path==='/sessions'||path==='/projects/fixture/sessions')return json(heldMode?[branch,{...source,activity:heldActivity}]:copied?[branch,source]:[source]);
  if(path==='/projects'||path==='/skills'||path.startsWith('/workbench/'))return json([]);
  try {
    const file=resolve(root,path==='/'?'index.html':'.'+path);
    if(!file.startsWith(root+'/'))throw Error('path');
    const data=await readFile(file);
    res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)]??'application/octet-stream');res.end(data);
  }catch{res.writeHead(404);res.end();}
});
await new Promise(done=>server.listen(0,'127.0.0.1',done));
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
try {
  for(const [sync,width,entry] of [[true,1200,'menu'],[false,390,'slash']]) {
    current=source;copied=false;requests=[];
    const page=await browser.newPage({viewport:{width,height:900}});
    page.setDefaultTimeout(10000);
    const errors=[];page.on('pageerror',error=>errors.push(error.message));
    await page.addInitScript(()=>{localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');localStorage.setItem('atomcode.sidebar.view.v1','flat');});
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=${sync?1:0}&session=${source.id}`);
    await page.locator('.session-title-text').filter({hasText:source.name}).waitFor();
    if(entry==='menu') {
      refused=true;
      await page.locator('.session-title-btn').click();
      const refusal=page.waitForEvent('dialog').then(async dialog=>{assert(dialog.message().includes('session is busy'));await dialog.accept();});
      await page.getByRole('button',{name:'Copy this conversation and switch to the copy'}).click();
      await refusal;
      assert.equal(copied,false);
      assert.equal(await page.locator('.session-title-text').textContent(),source.name);
      refused=false;requests=[];
      await page.locator('.session-title-btn').click();
      await page.getByRole('button',{name:'Copy this conversation and switch to the copy'}).click();
    }else{
      await page.locator('.message-input').fill('/fork');
      await page.locator('.btn-send').click();
    }
    await page.locator('.session-title-text').filter({hasText:branch.name}).waitFor();
    await page.getByRole('status').filter({hasText:'Working-directory files are shared'}).waitFor();
    assert.equal(requests.length,1);
    assert.equal(copied,true);
    assert((await page.locator('body').textContent()).includes('Keep this original context'));
    if(width===1200) { assert(await page.locator('.session-item-name').filter({hasText:source.name}).count() >= 2); }
    await page.screenshot({path:`/tmp/atomcode-fork-${width}.png`});
    assert.deepEqual(errors,[]);
    await page.close();
    console.log(`${entry}, sync=${sync}, width=${width}: fork switched and original context retained`);
  }
  heldMode=true;current=branch;
  for(const leave of [true,false]) {
    releaseHeldFork=undefined;
    heldActivity='running';heldAnswer='Held task is still running';
    heldStarted=Date.now()-20000;heldCompleted=heldStarted+1000;
    const page=await browser.newPage({viewport:{width:1200,height:900}});
    page.setDefaultTimeout(10000);
    await page.addInitScript(()=>{localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');localStorage.setItem('atomcode.sidebar.view.v1','flat');});
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=0&session=${branch.id}`);
    await page.locator('.session-title-text').filter({hasText:branch.name}).waitFor();
    await page.locator('.session-item-main').filter({hasText:source.name}).filter({hasNotText:'(fork)'}).click();
    await page.getByText('Held task is still running',{exact:true}).waitFor();
    await page.locator('.browse-activity-status').filter({hasText:'Running'}).waitFor();
    assert.equal(await page.locator('.timeline-message .msg-time').count(),1,'only the earlier completed turn has a terminal footer');
    await page.getByText('Checkpoint inspection',{exact:true}).waitFor();
    const elapsed=await page.locator('.browse-elapsed').textContent();
    await page.waitForFunction(previous=>document.querySelector('.browse-elapsed')?.textContent!==previous,elapsed,{timeout:2500});
    if(leave) {
      await page.locator('.browse-activity-status').scrollIntoViewIfNeeded();
      await page.screenshot({path:'/tmp/atomcode-browse-running-status.png'});
    }
    if(leave) {
      heldActivity='waiting';
      await page.locator('.browse-activity-status').filter({hasText:'Waiting'}).waitFor();
      assert.equal(await page.locator('.timeline-message .msg-time').count(),1);
      // A failed final read must be retried after the activity becomes idle.
      // Hold failures until the activity poll has observed idle, rather than
      // racing the independent content poll's first final read.
      heldActivity='open';heldAnswer='Held task finished';heldHistoryFailures=100;heldCompleted=Date.now();
      await page.locator('.browse-activity-status').filter({hasText:'Syncing final result'}).waitFor();
      assert.equal(await page.locator('.timeline-message .msg-time').count(),1,'stale page is not shown as a completed answer');
      heldHistoryFailures=0;
      await page.getByText('Held task finished',{exact:true}).waitFor();
      await page.locator('.browse-activity-status').waitFor({state:'detached'});
      assert.equal(await page.locator('.timeline-message .msg-time').count(),2);
    }
    const received=new Promise(resolve=>{onHeldRequest=resolve;});
    await page.getByRole('button',{name:'Fork and continue',exact:true}).click();
    await received;
    assert.equal(typeof releaseHeldFork,'function');
    if(leave) {
      await page.locator('.session-browse-return').click();
      // Leaving and reopening the SAME history must invalidate the old fork.
      await page.locator('.session-item-main').filter({hasText:source.name}).filter({hasNotText:'(fork)'}).click();
    }
    const response=page.waitForResponse(response=>new URL(response.url()).pathname==='/command');
    releaseHeldFork();await response;
    if(leave) {
      await page.waitForTimeout(200);
      await page.locator('.session-browse-title').filter({hasText:source.name}).waitFor();
      assert.equal(await page.locator('.session-title-text').textContent(),source.name);
    }else {
      await page.locator('.session-title-text').filter({hasText:heldBranch.name}).waitFor();
    }
    await page.close();
    console.log(`held-session fork: ${leave?'late result does not steal navigation':'copy activated'}`);
  }
}finally{await browser.close();server.closeAllConnections();await new Promise(done=>server.close(done));}
