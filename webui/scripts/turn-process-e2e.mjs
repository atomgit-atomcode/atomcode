// Exercise production WebUI using synthetic events, never user sessions/providers.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root=resolve(import.meta.dirname,'../dist');
const id='turn-process';
const meta={id,name:'Turn process',working_dir:'/fixture',project_hash:'fixture',created_at:1700000000000,updated_at:1700000004000,message_count:4};
const full=[
  {role:'user',content:'HISTORY_REQUEST',source_seq:2,created_at:1700000000000},
  {role:'assistant',content:'HISTORY_PROCESS',tool_calls:[{id:'read-history',name:'read',arguments:'{}'}]},
  {role:'tool',content:'',tool_result:{call_id:'read-history',success:true,summary:'ok'}},
  {role:'assistant',content:'HISTORY_FINAL',created_at:1700000004000},
];
const clients=new Set();
let connections=0;
const snapshot=()=>({type:'snapshot',session_id:id,session_name:meta.name,project_hash:'fixture',working_dir:'/fixture',provider:'fixture',model:'fixture',mode:'build',messages:full});
const server=createServer(async(req,res)=>{
  const path=new URL(req.url,'http://localhost').pathname;
  const json=data=>{res.setHeader('Content-Type','application/json');res.end(JSON.stringify(data));};
  if(path==='/live'){
    connections++;
    res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(`data: ${JSON.stringify(snapshot())}\n\ndata: {"type":"state","running":false}\n\n`);
    clients.add(res);req.on('close',()=>clients.delete(res));return;
  }
  if(path.startsWith('/sessions/resolve/'))return json(meta);
  if(path===`/projects/fixture/sessions/${id}`)return json({...meta,messages:full});
  if(path==='/project')return json({working_dir:'/fixture',project_hash:'fixture'});
  if(path==='/config')return json({providers:[],notifications:{enabled:false}});
  if(path==='/models')return json([{name:'fixture',model:'fixture',provider:'fixture',effort_levels:[]}]);
  if(path.endsWith('/transcript'))return json({session_id:id,turns:[]});
  if(path==='/live/mode')return json({mode:'build'});
  if(path==='/live/session')return json({success:true,session_id:id});
  if(path==='/sessions'||path==='/projects/fixture/sessions')return json([meta]);
  if(path==='/chat/active'||path==='/projects'||path==='/skills'||path.startsWith('/workbench/'))return json([]);
  try{
    const file=resolve(root,path==='/'?'index.html':'.'+path);if(!file.startsWith(root+'/'))throw Error('path');
    res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)]??'application/octet-stream');res.end(await readFile(file));
  }catch{res.writeHead(404);res.end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
const emit=event=>{for(const client of clients)client.write(`data: ${JSON.stringify(event)}\n\n`);};
const errors=[];
try {
  const page=await browser.newPage({viewport:{width:1200,height:900}});
  page.on('pageerror',error=>errors.push(error.message));
  await page.addInitScript(()=>{localStorage.setItem('atomcode.sidebar.view.v1','flat');localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
  await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
  await page.getByText('HISTORY_FINAL',{exact:true}).waitFor();
  assert.equal(await page.getByText('HISTORY_PROCESS',{exact:true}).count(),0);
  const toggle=page.locator('.turn-process-toggle').first();
  assert.equal(await toggle.getAttribute('aria-expanded'),'false');
  assert.match(await toggle.textContent(),/4s/,'restored completion time must not be replaced by reconnect time');
  await toggle.click(); await page.getByText('HISTORY_PROCESS',{exact:true}).waitFor();
  await toggle.press('Enter');
  assert.equal(await page.getByText('HISTORY_PROCESS',{exact:true}).count(),0);
  await page.keyboard.press('Control+f');
  await page.locator('.msg-search-input').fill('HISTORY_PROCESS');
  await page.getByText('HISTORY_PROCESS',{exact:true}).waitFor();
  await page.locator('.msg-search-input').press('Escape');
  const beforeReload=connections;
  await page.reload(); await page.getByText('HISTORY_FINAL',{exact:true}).waitFor();
  for(let attempt=0;attempt<100 && (connections<=beforeReload || !clients.size);attempt++) await page.waitForTimeout(20);
  assert.ok(connections>beforeReload && clients.size,'reload established a new live subscriber');
  assert.equal(await page.getByText('HISTORY_PROCESS',{exact:true}).count(),0);
  emit({type:'user',text:'LIVE_REQUEST'});
  await page.getByText('LIVE_REQUEST',{exact:true}).waitFor();
  emit({type:'state',running:true});
  emit({type:'text',content:'LIVE_PROCESS'});
  emit({type:'tool_start',id:'read-live',name:'read',arguments:'{}'});
  emit({type:'tool_result',id:'read-live',name:'read',output:'ok',success:true,duration_ms:100});
  emit({type:'text',content:'LIVE_FINAL'});
  await page.getByText('LIVE_PROCESS',{exact:true}).waitFor();
  await page.getByText('LIVE_FINAL',{exact:true}).waitFor();
  await page.locator('.messages-container').last().evaluate(el=>{el.scrollTop=el.scrollHeight;el.dispatchEvent(new Event('scroll'));});
  await page.waitForTimeout(200);
  emit({type:'state',running:false,stop_reason:'stopped'});
  await page.waitForTimeout(300);
  assert.equal(await page.getByText('LIVE_PROCESS',{exact:true}).count(),0);
  await page.getByText('LIVE_FINAL',{exact:true}).waitFor();
  await page.screenshot({path:'/tmp/atomcode-turn-process.png',fullPage:true});
  await page.locator('.turn-process-toggle').last().click();
  await page.getByText('LIVE_PROCESS',{exact:true}).waitFor();
  emit({type:'user',text:'CANCEL_REQUEST'});emit({type:'state',running:true});
  emit({type:'text',content:'CANCEL_PROCESS'});
  emit({type:'tool_start',id:'cancel-tool',name:'read',arguments:'{}'});
  emit({type:'tool_result',id:'cancel-tool',name:'read',output:'ok',success:true,duration_ms:1});
  emit({type:'text',content:'CANCEL_PARTIAL'});
  emit({type:'state',running:false,stop_reason:'cancelled'});
  await page.getByText('CANCEL_PROCESS',{exact:true}).waitFor();
  await page.getByText('CANCEL_PARTIAL',{exact:true}).waitFor();
  emit({type:'user',text:'READ_REQUEST'});emit({type:'state',running:true});
  emit({type:'text',content:'READ_PROCESS\n\n' + Array.from({length:50},(_,i)=>`Reading line ${i}`).join('\n\n')});
  emit({type:'tool_start',id:'read-reading',name:'read',arguments:'{}'});
  emit({type:'tool_result',id:'read-reading',name:'read',output:'ok',success:true,duration_ms:1});
  emit({type:'text',content:'READ_FINAL'});
  await page.getByText('READ_FINAL',{exact:true}).waitFor();
  await page.locator('.messages-container').last().evaluate(el=>{el.dispatchEvent(new WheelEvent('wheel',{deltaY:-120,bubbles:true}));el.scrollTop=0;el.dispatchEvent(new Event('scroll'));});
  await page.waitForTimeout(250);
  emit({type:'state',running:false,stop_reason:'stopped'});
  await page.waitForTimeout(300);
  assert.equal(await page.locator('.turn-process-toggle').last().getAttribute('aria-expanded'),'true','reading progress must not collapse on completion');
  assert.deepEqual(errors,[]);
  console.log('PASS: history collapse, keyboard expansion, reload, live process, completion and cancellation');
} finally {
  for(const client of clients)client.end(); await browser.close(); server.closeAllConnections(); server.close();
}
