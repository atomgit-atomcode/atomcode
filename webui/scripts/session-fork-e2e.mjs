// Real WebUI, fixture-only session service. Does not access user conversations.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const source = {id:'fork-source',name:'Original discussion',working_dir:'/fixture',project_hash:'fixture',created_at:1700000000,updated_at:1700000000,message_count:2};
const branch = {...source,id:'fork-copy',name:'Original discussion (fork)'};
const messages = [{role:'user',content:'Keep this original context'}, {role:'assistant',content:'Shared conversation before the fork'}];
let current = source;
let copied = false;
let refused = false;
let requests = [];
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
  if(path.startsWith('/sessions/resolve/'))return json(path.endsWith(branch.id)?branch:source);
  if(path===`/projects/fixture/sessions/${source.id}`)return json({...source,messages});
  if(path===`/projects/fixture/sessions/${branch.id}`)return json({...branch,messages});
  if(path.endsWith('/transcript'))return json({session_id:current.id,turns:[]});
  if(path==='/live/session')return json({success:true,session_id:current.id});
  if(path==='/live/mode')return json({mode:'build'});
  if(path==='/chat/active')return json([]);
  if(path==='/sessions'||path==='/projects/fixture/sessions')return json(copied?[branch,source]:[source]);
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
}finally{await browser.close();server.closeAllConnections();await new Promise(done=>server.close(done));}
