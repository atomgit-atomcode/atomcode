// Real WebUI with controlled HTTP/SSE; no user's daemon, model or config access.
import {chromium} from 'playwright';
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {resolve,extname} from 'node:path';
import assert from 'node:assert/strict';
const root=resolve(import.meta.dirname,'../dist');
const meta={id:'model-next-turn',name:'Model selection fixture',working_dir:'/fixture',project_hash:'fixture',created_at:1700000000,updated_at:1700000000,message_count:2};
const models=['first','second','third'].map(provider=>({provider,model:provider+'-model',provider_type:'openai',effort_levels:[],context_window:128000}));
let selected='first',running=true,reject=false,pending=null;
const clients=new Set(),submissions=[];
const emit=event=>{for(const client of clients)client.write('data: '+JSON.stringify(event)+'\n\n');};
const identity=provider=>({provider,model:provider+'-model'});
const server=createServer(async(req,res)=>{
 const path=new URL(req.url,'http://localhost').pathname;
 const json=value=>{res.setHeader('Content-Type','application/json');res.end(JSON.stringify(value));};
 const body=async()=>{let b='';for await(const chunk of req)b+=chunk;return JSON.parse(b||'{}');};
 if(path==='/live'){
  res.writeHead(200,{'Content-Type':'text/event-stream'});clients.add(res);req.on('close',()=>clients.delete(res));
  for(const event of [
   {type:'snapshot',session_id:meta.id,provider:selected,model:identity(selected).model,mode:'build',messages:[{role:'user',content:'Original task'},{role:'assistant',content:'Original output'}]},
   ...(pending?[{type:'provider_selection_pending',current:pending}]:[]),
   {type:'state',running},
  ])res.write('data: '+JSON.stringify(event)+'\n\n');
  return;
 }
 if(path==='/live/provider'){
  const input=await body();assert.equal(input.session_id,meta.id);
  if(reject)return json({ok:false,error:'Provider preparation failed'});
  if(running&&!pending)pending=identity(selected);
  selected=input.provider;
  emit({type:'provider_selection_pending',current:pending});emit({type:'provider',...identity(selected)});
  return json({ok:true,deferred:running});
 }
 if(path==='/live/message'){
  const input=await body();submissions.push(input);running=true;emit({type:'state',running:true});
  emit({type:'user',text:input.message,client_input_id:input.client_input_id});
  emit({type:'text',content:'Next turn uses '+selected});
  return json({accepted:true,disposition:'started',provider:selected,generation:1,turn_id:2});
 }
 if(path==='/live/permission')return json({accepted:true});
 if(path==='/project')return json({working_dir:'/fixture',project_hash:'fixture'});
 if(path==='/config')return json({providers:[],notifications:{enabled:false}});
 if(path==='/models')return json(models);
 if(path.startsWith('/sessions/resolve/'))return json(meta);
 if(path==='/projects/fixture/sessions/'+meta.id)return json({...meta,messages:[{role:'user',content:'Original task'},{role:'assistant',content:'Original output'}]});
 if(path.endsWith('/transcript'))return json({session_id:meta.id,turns:[]});
 if(path==='/live/mode')return json({mode:'build'});
 if(path==='/chat/active'||path==='/projects'||path==='/skills')return json([]);
 if(path==='/sessions'||path==='/projects/fixture/sessions')return json([meta]);
 try{const file=resolve(root,path==='/'?'index.html':'.'+path);if(!file.startsWith(root+'/'))throw Error('path');res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)]??'application/octet-stream');res.end(await readFile(file));}catch{res.writeHead(404);res.end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
try{
 for(const width of [1200,390]){
  selected='first';running=true;pending=null;reject=false;submissions.length=0;
  const page=await browser.newPage({viewport:{width,height:900}});const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.addInitScript(()=>localStorage.setItem('atomcode.lang','en'));
  await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${meta.id}`);
  await page.locator('.message-input').waitFor();await page.locator('.btn-stop').waitFor();
  const select=async(name)=>{
   await page.locator('.message-input').focus();
   await page.locator('.model-selector-trigger').filter({hasText:/-model/}).click();
   await page.locator('.model-item').filter({has:page.getByText(name+'-model',{exact:true})}).click();
  };
  await select('second');
  await page.getByText('Current turn: first-model → Next turn: second-model',{exact:true}).waitFor();
  emit({type:'model',model:'first-model'});
  await page.getByText('Current turn: first-model → Next turn: second-model',{exact:true}).waitFor();
  reject=true;await select('third');
  await page.getByText('Provider preparation failed',{exact:false}).waitFor();
  await page.waitForFunction(()=>document.querySelector('.model-next-turn')?.textContent?.endsWith('second-model'));
  reject=false;await select('third');
  await page.getByText('Current turn: first-model → Next turn: third-model',{exact:true}).waitFor();
  emit({type:'text',content:'Original stream still running'});
  await page.getByText('Original stream still running',{exact:false}).waitFor();
  await page.reload();await page.locator('.message-input').waitFor();
  await page.getByText('Current turn: first-model → Next turn: third-model',{exact:true}).waitFor();
  await page.locator('.message-input').focus();
  await page.screenshot({path:`/tmp/atomcode-next-turn-model-${width}.png`});
  running=false;pending=null;emit({type:'state',running:false,stop_reason:'completed'});emit({type:'provider_selection_pending',current:null});
  await page.locator('.model-next-turn').waitFor({state:'detached'});
  await page.locator('.btn-stop').waitFor({state:'detached'});
  await page.locator('.message-input').fill('Next request');await page.locator('.message-input').press('Enter');
  try {await page.getByText('Next turn uses third',{exact:false}).waitFor({timeout:5000});} catch(error) {console.log('submissions',submissions);console.log(await page.locator('body').innerText());throw error;}
  assert.equal(submissions.at(-1).provider,'third');
  assert.deepEqual(errors,[]);await page.close();
 }
 console.log('Next-turn selection: active stream preserved, repeated choice, failure rollback, reconnect, and next submit passed at 1200/390px');
}finally{await browser.close();for(const client of clients)client.end();await new Promise(resolve=>server.close(resolve));}
