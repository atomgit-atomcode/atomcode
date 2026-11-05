// Real WebUI, synthetic HTTP/SSE. No user config, daemon or model requests.
import {chromium} from 'playwright';
import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {resolve,extname} from 'node:path';
import assert from 'node:assert/strict';
const root=resolve(import.meta.dirname,'../dist');
const meta={id:'mode-fixture',name:'Mode fixture',working_dir:'/fixture',project_hash:'fixture',created_at:1700000000,updated_at:1700000000,message_count:2};
const models=[
 {provider:'fast',model:'fast-wire',provider_type:'openai',effort_applicable:true,effort_levels:['low'],reasoning_effort:'low',context_window:128000},
 {provider:'deep',model:'deep-wire',provider_type:'openai',effort_applicable:true,effort_levels:['low','high'],reasoning_effort:'low',context_window:128000},
];
let bindings={plan:{},build:{}};
let mode='build';let reject=false;let modeRequests=0;let saved=0;const chats=[];
const choices=models.map(m=>({id:m.provider,label:m.model+' · '+m.provider,effort_levels:m.effort_levels}));
const server=createServer(async(req,res)=>{
 const path=new URL(req.url,'http://localhost').pathname;
 const json=value=>{res.setHeader('Content-Type','application/json');res.end(JSON.stringify(value));};
 const body=async()=>{let value='';for await(const chunk of req)value+=chunk;return JSON.parse(value||'{}');};
 if(path==='/config/modes'){
  if(req.method==='POST'){bindings=await body();saved++;}
  return json({bindings,models:choices,reasoning_effort_levels:['low','medium','high']});
 }
 if(path==='/chat'){
  const input=await body();chats.push(input);
  res.writeHead(200,{'Content-Type':'text/event-stream'});
  for(const event of [{type:'runtime_info',provider:input.provider,model:models.find(m=>m.provider===input.provider)?.model}, {type:'text',content:'Turn reply '+chats.length}, {type:'done',session_id:meta.id,stop_reason:'completed'}])res.write('data: '+JSON.stringify(event)+'\n\n');
  return res.end();
 }
 if(path==='/approval_mode'){
  if(req.method==='GET')return json({ok:true,mode});
  const input=await body();modeRequests++;
  if(reject)return json({ok:false,mode,error:'Stop the current task before changing a mode-bound model'});
  let selection;
  const binding=bindings[input.mode];
  if(binding&&(binding.model||binding.reasoning_effort)){
   const selected=binding.model??input.current_model?.model;
   selection={model:selected,reasoning_effort:binding.reasoning_effort??models.find(m=>m.provider===selected)?.reasoning_effort??null};
  }else if(input.mode==='build'&&input.previous_mode==='plan')selection=input.return_model;
  mode=input.mode;return json({ok:true,mode,...(selection?{selection}:{})});
 }
 if(path==='/project')return json({working_dir:'/fixture',project_hash:'fixture'});
 if(path==='/config')return json({providers:[],notifications:{enabled:false}});
 if(path==='/models')return json(models);
 if(path.startsWith('/sessions/resolve/'))return json(meta);
 if(path==='/projects/fixture/sessions/'+meta.id)return json({...meta,messages:[{role:'user',content:'Mode question'},{role:'assistant',content:'Mode answer'}]});
 if(path.endsWith('/transcript'))return json({session_id:meta.id,turns:[]});
 if(path==='/live/mode')return json({mode});
 if(path==='/live/session')return json({success:true,session_id:meta.id});
 if(path==='/sessions'||path==='/projects/fixture/sessions')return json([meta]);
 if(path==='/projects'||path==='/skills'||path==='/chat/active')return json([]);
 try{const file=resolve(root,path==='/'?'index.html':'.'+path);if(!file.startsWith(root+'/'))throw new Error('path');res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)]??'application/octet-stream');res.end(await readFile(file));}
 catch{res.writeHead(404);res.end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
try{
 for(const width of [1200,390]){
  bindings={plan:{},build:{}};mode='build';reject=false;chats.length=0;
  const page=await browser.newPage({viewport:{width,height:900}});
  const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await page.addInitScript(()=>localStorage.setItem('atomcode.lang','en'));
  await page.goto(`http://127.0.0.1:${server.address().port}/?sync=0&session=mode-fixture`);
  await page.getByText('Mode answer',{exact:true}).waitFor();
  const selectMode=async label=>{
    const expected=label.toLowerCase();
    const response=expected!==mode?page.waitForResponse(response=>response.url().endsWith('/approval_mode')&&response.request().method()==='POST'&&response.request().postDataJSON().mode===expected):null;
    await page.locator('.message-input').focus();
    await page.locator('.mode-selector-trigger').click();
    await page.locator('.mode-item').filter({has:page.locator('.model-item-model',{hasText:new RegExp('^'+label+'$')})}).click();
    if(response)await response;
  };
  await selectMode('Plan');
  await page.waitForFunction(()=>document.querySelector('.mode-selector .model-selector-label')?.textContent==='Plan');
  assert.match(await page.locator('.model-selector-trigger').last().innerText(),/fast-wire/);
  await selectMode('Build');
  await page.locator('.message-input').focus();
  await page.locator('.mode-selector-trigger').click();
  await page.getByRole('button',{name:'Configure mode models…',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Mode models'});
  await dialog.waitFor();
  const plan=dialog.locator('fieldset').first();
  await plan.locator('select').first().selectOption('deep');
  await plan.locator('select').last().selectOption('high');
  // The light model exposes only low; switching the model resets old high.
  await plan.locator('select').first().selectOption('fast');
  assert.equal(await plan.locator('option[value="high"]').count(),0);
  await plan.locator('select').first().selectOption('deep');
  await plan.locator('select').last().selectOption('high');
  await dialog.screenshot({path:`/tmp/atomcode-mode-settings-${width}.png`});
  await dialog.getByRole('button',{name:'Save',exact:true}).click();
  await dialog.waitFor({state:'detached'});assert(saved>0);
  await selectMode('Plan');
  await page.waitForFunction(()=>document.querySelector('.model-selector-trigger:last-child')?.textContent?.includes('deep-wire') || [...document.querySelectorAll('.model-selector-trigger')].some(el=>el.textContent.includes('deep-wire')));
  await page.getByText('plan · deep · reasoning effort high',{exact:false}).waitFor();
  for(const turn of [1,2]){
    await page.locator('.message-input').fill('Bound turn '+turn);
    await page.locator('.message-input').press('Enter');
    await page.getByText('Turn reply '+turn,{exact:true}).waitFor();
    await page.waitForFunction(()=>!document.querySelector('.btn-stop'));
    assert.equal(chats.at(-1).provider,'deep');
    assert.equal(chats.at(-1).reasoning_effort,'high');
  }
  // Manual selection is a temporary override until the next real mode transition.
  await page.locator('.message-input').focus();
  await page.locator('.model-selector-trigger').filter({hasText:'deep-wire'}).click();
  await page.locator('.model-item').filter({hasText:/^fast-wire$/}).click();
  await page.waitForFunction(()=>[...document.querySelectorAll('.model-selector-trigger')].some(el=>el.textContent.includes('fast-wire')));
  // Same-mode selection does not reapply bindings or issue another request.
  const before=modeRequests;
  await selectMode('Plan');assert.equal(modeRequests,before);
  await selectMode('Build');
  await page.waitForFunction(()=>[...document.querySelectorAll('.model-selector-trigger')].some(el=>el.textContent.includes('fast-wire')));
  await page.waitForFunction(()=>document.querySelector('.mode-selector .model-selector-label')?.textContent==='Build');
  reject=true;
  await selectMode('Plan');
  await page.getByText(/Mode switch failed:.*Stop the current task/).waitFor();
  assert.equal(mode,'build');
  await page.screenshot({path:`/tmp/atomcode-mode-bindings-${width}.png`});
  assert.deepEqual(errors,[]);
  await page.close();
 }
 console.log('Mode settings, effort choices, Plan/Build binding, same-mode no-op and rejection feedback passed at 1200/390px');
}finally{await browser.close();server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
