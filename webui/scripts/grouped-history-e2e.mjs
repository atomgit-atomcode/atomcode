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
let navigationRecords=[];
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
  if(path.endsWith('/transcript'))return json({session_id:id,turns:navigationRecords});
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
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true,ignoreDefaultArgs:['--hide-scrollbars']});
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
    const page=await browser.newPage({viewport:{width,height:900},hasTouch:width<600});
    page.on('pageerror',error=>errors.push(error.message));
    await page.addInitScript(()=>{localStorage.setItem('atomcode.sidebar.view.v1','flat');localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
    await page.getByText('GROUP_FINAL_2',{exact:true}).waitFor();
    await page.getByText('GROUP_REQUEST_2',{exact:true}).waitFor();
    assert.equal(await page.locator('.tool-body').count(),0);
    const viewport=page.locator('.messages-container');
    // Browser/layout corrections must not navigate to another history page.
    const beforeLayout = await page.locator('.user-message-wrapper').first().innerText();
    await viewport.evaluate(el => {
      const spacer = document.createElement('div');
      spacer.id = 'layout-correction-fixture'; spacer.style.height = '2000px';
      el.querySelector('.timeline-inner').append(spacer);
      el.scrollTop = el.scrollHeight;
    });
    await page.waitForTimeout(650);
    await viewport.evaluate(el => { el.querySelector('#layout-correction-fixture').remove(); });
    await page.waitForTimeout(650);
    assert.equal(await page.locator('.user-message-wrapper').first().innerText(), beforeLayout,
      'layout shrink must not silently page to an earlier conversation');
    for(let attempt=0;attempt<20 && await page.getByText('GROUP_FINAL_0',{exact:true}).count()===0;attempt++) {
      await viewport.evaluate(el=>el.scrollTop=0);
      await viewport.hover(); await page.mouse.wheel(0,-1000); await page.waitForTimeout(600);
    }
    await page.getByText('GROUP_REQUEST_0',{exact:true}).waitFor();
    await page.getByText('GROUP_FINAL_0',{exact:true}).waitFor();
    assert.match(await page.locator('.turn-nav-node.is-active').getAttribute('aria-label'),/GROUP_REQUEST_0/,'navigation follows the displayed page even when it does not overflow');
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
    // Keyboard navigation works, but an editor inside the timeline cannot page it.
    await viewport.evaluate(el => {
      el.tabIndex = 0;
      const input = document.createElement('textarea');
      input.id = 'timeline-editor-fixture'; el.append(input); input.focus();
    });
    await page.keyboard.press('PageUp'); await page.waitForTimeout(650);
    await page.getByText('GROUP_REQUEST_2',{exact:true}).waitFor();
    await viewport.evaluate(el => { el.querySelector('#timeline-editor-fixture').remove(); el.focus(); });
    for(let attempt=0;attempt<20 && await page.getByText('GROUP_REQUEST_1',{exact:true}).count()===0;attempt++) {
      await viewport.evaluate(el=>el.scrollTop=0);
      await page.keyboard.press('PageUp'); await page.waitForTimeout(650);
    }
    await page.getByText('GROUP_REQUEST_1',{exact:true}).waitFor();
    await viewport.focus(); await page.keyboard.press('PageDown'); await page.waitForTimeout(650);
    await page.getByText('GROUP_REQUEST_2',{exact:true}).waitFor();
    if (width < 600) {
      const cdp = await page.context().newCDPSession(page);
      const box = await viewport.boundingBox();
      const point = {x:box.x+box.width/2,y:box.y+80};
      for(let attempt=0;attempt<20 && await page.getByText('GROUP_REQUEST_1',{exact:true}).count()===0;attempt++) {
        await viewport.evaluate(el=>el.scrollTop=0);
        await cdp.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[point]});
        await cdp.send('Input.dispatchTouchEvent',{type:'touchMove',touchPoints:[{...point,y:point.y+200}]});
        await cdp.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
        await page.waitForTimeout(650);
      }
      await page.getByText('GROUP_REQUEST_1',{exact:true}).waitFor();
      await cdp.detach();
    } else {
      // Exercise the browser's native scrollbar, including mouse release.
      await page.addStyleTag({content:'.messages-container{scrollbar-gutter:stable;scrollbar-width:auto;scrollbar-color:auto}.messages-container::-webkit-scrollbar{width:16px;display:block}.messages-container::-webkit-scrollbar-thumb{background:#999}'});
      await viewport.evaluate(el => {
        const space = document.createElement('div');
        space.id = 'scrollbar-fixture'; space.style.height = '2000px';
        el.querySelector('.timeline-inner').append(space); el.scrollTop = el.scrollHeight;
      });
      await page.waitForTimeout(650);
      const box = await viewport.boundingBox();
      for(let attempt=0;attempt<2 && await page.getByText('GROUP_REQUEST_1',{exact:true}).count()===0;attempt++) {
        await viewport.evaluate(el=>el.scrollTop=el.scrollHeight);
        await page.waitForTimeout(100);
        await page.mouse.move(box.x+box.width-8,box.y+box.height-30);
        await page.mouse.down();
        await page.mouse.move(box.x+box.width-8,box.y+5,{steps:30});
        await page.waitForTimeout(100);
        await page.mouse.up(); await page.waitForTimeout(650);
      }
      await page.getByText('GROUP_REQUEST_1',{exact:true}).waitFor();
      const beforeRelease = await page.locator('.user-message-wrapper').first().innerText();
      await viewport.evaluate(el => el.querySelector('#scrollbar-fixture')?.remove());
      await page.waitForTimeout(650);
      assert.equal(await page.locator('.user-message-wrapper').first().innerText(),beforeRelease,
        'releasing the scrollbar must stop gesture-driven pagination');
    }
    await page.close();
  }
  // A collapsed process can extend the displayed range far before the raw
  // window. Leaving the bottom must freeze that window, not start a new page
  // at the old process and drop all the recent conversations.
  full.splice(0,full.length,
    {role:'user',content:'LARGE_OLD_REQUEST',created_at:1700000000000},
    {role:'assistant',content:'Old process',tool_calls:Array.from({length:400},(_,i)=>({id:`old-${i}`,name:'read',arguments:'{}'}))},
    ...Array.from({length:400},(_,i)=>({role:'tool',content:'',tool_result:{call_id:`old-${i}`,success:true,summary:'ok'}})),
    {role:'assistant',content:'LARGE_OLD_FINAL',created_at:1700000004000},
    ...Array.from({length:4},(_,i)=>[
      {role:'user',content:`RECENT_REQUEST_${i}`,created_at:1700000010000+i*10000},
      {role:'assistant',content:`RECENT_FINAL_${i}\n\n`+'Recent answer text. '.repeat(100),created_at:1700000014000+i*10000},
    ]).flat());
  navigationRecords=full.filter(message=>message.role==='user').map((message,index)=>({turn_id:index+1,user:message.content,started_at:message.created_at,undone:false}));
  for(const width of [1200,390]) {
    const page=await browser.newPage({viewport:{width,height:900}});
    page.on('pageerror',error=>errors.push(error.message));
    await page.addInitScript(()=>{localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
    const viewport=page.locator('.messages-container');
    await page.getByText('RECENT_REQUEST_3',{exact:true}).waitFor();
    await page.waitForTimeout(900);
    const before=await viewport.evaluate(el=>({top:el.scrollTop,height:el.scrollHeight,users:[...el.querySelectorAll('.user-message-wrapper')].map(n=>n.innerText)}));
    await viewport.hover(); await page.mouse.wheel(0,-60); await page.waitForTimeout(200);
    const after=await viewport.evaluate(el=>({top:el.scrollTop,height:el.scrollHeight,users:[...el.querySelectorAll('.user-message-wrapper')].map(n=>n.innerText)}));
    assert.deepEqual(after.users,before.users,'a small upward gesture must not replace the recent turns with an older page');
    assert.equal(after.height,before.height,'freezing the current window must not remove content');
    assert.ok(Math.abs(before.top-after.top-60)<3,'the view should move only by the wheel delta');
    assert.match(await page.locator('.turn-nav-node.is-active').getAttribute('aria-label'),/RECENT_REQUEST_3/);
    // Read the penultimate prompt and verify the navigation label, not just a
    // numeric index that could point at a different merged history entry.
    await viewport.evaluate(el=>{
      const target=[...el.querySelectorAll('.user-message-wrapper')].find(n=>n.textContent.includes('RECENT_REQUEST_2'));
      el.scrollTop+=target.getBoundingClientRect().top-el.getBoundingClientRect().top-20;
    });
    await page.waitForTimeout(150);
    assert.match(await page.locator('.turn-nav-node.is-active').getAttribute('aria-label'),/RECENT_REQUEST_2/);
    await page.close();
  }
  assert.deepEqual(errors,[]);
  console.log('PASS: mobile/desktop groups, stable layout, bounded expansion, wheel/keyboard/touch/scrollbar pagination');
} finally { await browser.close(); await new Promise(resolve=>server.close(resolve)); }
