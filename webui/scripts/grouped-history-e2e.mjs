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
const snapshot=()=>({type:'snapshot',session_id:id,session_name:meta.name,project_hash:'fixture',working_dir:'/fixture',provider:'fixture',model:'fixture',mode:'build',messages:full});
const server=createServer(async(req,res)=>{
  const path=new URL(req.url,'http://localhost').pathname;
  const json=data=>{res.setHeader('Content-Type','application/json');res.end(JSON.stringify(data));};
  if(path==='/live'){
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
  full.splice(0,full.length,...Array.from({length:3},(_,group)=>[
    {role:'user',content:`GROUP_REQUEST_${group}`,source_seq:group*1000+2,created_at:1700000000000+group*10000},
    {role:'assistant',content:`GROUP_PROCESS_${group}`,tool_calls:Array.from({length:300},(_,index)=>({id:`tool-${group}-${index}`,name:'read',arguments:'{}'}))},
    ...Array.from({length:300},(_,index)=>({role:'tool',content:'',tool_result:{call_id:`tool-${group}-${index}`,success:true,summary:'ok'}})),
    {role:'assistant',content:`GROUP_FINAL_${group}`,created_at:1700000004000+group*10000},
  ]).flat());
  for(const width of [1200,390]) {
    const page=await browser.newPage({viewport:{width,height:900}});
    page.on('pageerror',error=>errors.push(error.message));
    await page.addInitScript(()=>{localStorage.setItem('atomcode.sidebar.view.v1','flat');localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
    await page.getByText('GROUP_FINAL_2',{exact:true}).waitFor();
    await page.getByText('GROUP_REQUEST_2',{exact:true}).waitFor();
    assert.equal(await page.locator('.tool-body').count(),0);
    const viewport=page.locator('.messages-container');
    for(let attempt=0;attempt<20 && await page.getByText('GROUP_FINAL_0',{exact:true}).count()===0;attempt++) {
      await viewport.evaluate(el=>el.scrollTop=0);
      await viewport.hover(); await page.mouse.wheel(0,-1000); await page.waitForTimeout(600);
    }
    await page.getByText('GROUP_REQUEST_0',{exact:true}).waitFor();
    await page.getByText('GROUP_FINAL_0',{exact:true}).waitFor();
    assert.equal(await page.locator('.tool-body').count(),0,'archived completed groups stay collapsed');
    const toggle=page.locator('.turn-process-toggle').first();
    assert.equal(await toggle.getAttribute('aria-expanded'),'false');
    await toggle.click(); await page.waitForTimeout(500);
    assert.equal(await toggle.getAttribute('aria-expanded'),'true');
    assert.ok(await page.locator('.tool-body').count()<300,'expanded process remains bounded');
    await page.getByText('GROUP_REQUEST_0',{exact:true}).waitFor();
    await page.getByText('GROUP_FINAL_0',{exact:true}).waitFor();
    await toggle.click(); await page.waitForTimeout(300);
    assert.equal(await page.locator('.tool-body').count(),0);
    for(let attempt=0;attempt<20 && await page.getByText('GROUP_FINAL_2',{exact:true}).count()===0;attempt++) {
      await viewport.evaluate(el=>el.scrollTop=el.scrollHeight);
      await viewport.hover(); await page.mouse.wheel(0,1000); await page.waitForTimeout(600);
    }
    await page.getByText('GROUP_REQUEST_2',{exact:true}).waitFor();
    await page.getByText('GROUP_FINAL_2',{exact:true}).waitFor();
    await page.close();
  }
  assert.deepEqual(errors,[]);
  console.log('PASS: mobile/desktop whole conversation groups, archived collapse, bounded expansion');
} finally { await browser.close(); await new Promise(resolve=>server.close(resolve)); }
