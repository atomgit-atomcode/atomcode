// Exercise production WebUI using synthetic events, never user sessions/providers.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root=resolve(import.meta.dirname,'../dist');
const id='turn-process';
const meta={id,name:'Turn process',working_dir:'/fixture',project_hash:'fixture',created_at:1700000000000,updated_at:1700000004000,message_count:4};
const png='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=';
const full=[
  {role:'user',content:'Compare these [Image #1] [Image #2]',source_seq:2,images:[{media_type:'image/png',data:png},{media_type:'image/png',data:png}]},
  {role:'assistant',content:'Images received'},
  {role:'user',content:'Missing original [Image #1]',source_seq:4,images:[{media_type:'image/png',data:'',missing:true}]},
  {role:'assistant',content:'Original unavailable'},
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
  const page=await browser.newPage({viewport:{width:1200,height:900}});
  page.on('pageerror',error=>errors.push(error.message));
  await page.addInitScript(()=>{localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
  await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
  await page.getByText('Images received',{exact:true}).waitFor();
  const bubbles=page.locator('.user-message-bubble');
  assert.equal((await bubbles.first().innerText()).trim(),'Compare these');
  assert.match(await bubbles.last().innerText(),/\[Image #1\]/);
  assert.equal(await page.locator('.msg-image-button').count(),2);
  const first=page.getByRole('button',{name:'Enlarge image 1',exact:true});
  await first.click();
  const modal=page.locator('dialog.message-image-preview[open]');
  await modal.waitFor();
  assert.equal(await modal.locator('img').evaluate(image=>image.complete&&image.naturalWidth>0),true);
  await page.keyboard.press('Escape'); await modal.waitFor({state:'detached'});
  assert.equal(await first.evaluate(button=>document.activeElement===button),true);
  await page.getByRole('button',{name:'Enlarge image 2',exact:true}).click();
  await modal.waitFor(); await page.getByRole('button',{name:'Close image preview',exact:true}).click();
  await modal.waitFor({state:'detached'});
  await first.click(); await modal.waitFor(); await modal.click({position:{x:4,y:4}});
  await modal.waitFor({state:'detached'});
  await page.setViewportSize({width:390,height:844});
  await first.click(); await modal.waitFor();
  assert.ok((await modal.boundingBox()).width<=391);
  await page.keyboard.press('Escape'); await modal.waitFor({state:'detached'});
  await page.reload(); await first.waitFor();
  assert.equal((await page.locator('.user-message-bubble').first().innerText()).trim(),'Compare these');
  for (const sync of [true, false]) {
    const endpoint=sync?'/live/message':'/chat';
    const submissions=[];
    await page.route(`**${endpoint}`, async route=>{
      const request=route.request();
      submissions.push({bytes:Buffer.byteLength(request.postData()),body:request.postDataJSON()});
      if(submissions.length===1){
        await route.fulfill({status:413,body:'Failed to buffer the request body: length limit exceeded'});
      }else if(sync){
        await route.fulfill({json:{accepted:true}});
      }else{
        await route.fulfill({contentType:'text/event-stream',body:'data: {"type":"stopped"}\n\n'});
      }
    });
    if(!sync){
      await page.goto(`http://127.0.0.1:${server.address().port}/?sync=0&session=${id}`);
      await first.waitFor();
    }
    const composer=page.locator('.message-input');
    await composer.fill('Describe this image');
    await composer.evaluate((input,{data,size})=>{
      const clipboardData=new DataTransfer();
      const png=Uint8Array.from(atob(data),value=>value.charCodeAt(0));
      clipboardData.items.add(new File([png,new Uint8Array(size-png.length)],'large.png',{type:'image/png'}));
      input.dispatchEvent(new ClipboardEvent('paste',{clipboardData,bubbles:true,cancelable:true}));
    },{data:png,size:1_774_142});
    await page.locator('.input-thumb').waitFor();
    await composer.press('Enter');
    await page.getByRole('alert').filter({hasText:'Compress the images'}).waitFor();
    assert.equal(await composer.inputValue(),'Describe this image');
    assert.equal(await page.locator('.input-thumb').count(),1);
    assert.equal(await bubbles.filter({hasText:'Describe this image'}).count(),0);
    assert.ok(submissions[0].bytes>2*1024*1024);
    assert.equal(submissions[0].body.images[0].data.length,4*Math.ceil(1_774_142/3));
    assert.equal(await page.getByText('length limit exceeded',{exact:false}).count(),0);
    const retry=page.waitForResponse(response=>response.url().endsWith(endpoint)&&response.ok());
    await composer.press('Enter');
    await retry;
    assert.equal(submissions.length,2);
    assert.equal(submissions[1].body.message,'Describe this image');
    assert.equal(await page.locator('.input-thumb').count(),0);
  }
  assert.deepEqual(errors,[]);
  console.log('PASS: image previews, large clipboard upload, localized 413, and draft retry');
} finally {
  for(const client of clients)client.end(); await browser.close(); server.closeAllConnections(); server.close();
}
