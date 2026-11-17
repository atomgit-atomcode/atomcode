// Production UI with a local fixture: no user files, providers or external requests.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root=resolve(import.meta.dirname,'../dist');
const id='file-link-session';
const meta={id,name:'File links',working_dir:'/fixture',project_hash:'fixture',created_at:1700000000000,updated_at:1700000004000,message_count:2};
const history={...meta,id:'other-file-session',name:'Historical files'};
const png='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=';
const content=`[Image](file:///fixture/%E5%AE%A3%E4%BC%A0%20%E5%9B%BE.png) · [Script](./gen_share_card.py#L10) · [Missing](missing.png) · [Outside](/outside.png) · [Large](large.png) · [Slow](slow.txt) · [Windows](file:///C:/workspace/result.py)

[Website](https://example.com/output.png) · [Unsafe](javascript:alert(1)) · [Remote](file://server/share.png)

[Raw HTML](./sample.html)`;
const full=[{role:'user',content:'Show the generated files',source_seq:2},
  {role:'assistant',tool_calls:[{id:'generated-script',name:'write_file',arguments:'{"file_path":"./gen_share_card.py","content":"print(1)"}'}]},
  {role:'tool',tool_result:{call_id:'generated-script',success:true,summary:'Written'}},
  ...Array.from({length:6},(_,i)=>[
    {role:'assistant',tool_calls:[{id:`extra-${i}`,name:'write_file',arguments:JSON.stringify({file_path:`/fixture/src/deeply/nested/directory/generated-file-${i}.ts`,content:'export {}'})}]},
    {role:'tool',tool_result:{call_id:`extra-${i}`,success:true,summary:'Written'}},
  ]).flat(),
  {role:'assistant',content}];
const clients=new Set(), previews=[];
let switches=0, releaseSlow, notGit=false;
const snapshot=()=>({type:'snapshot',session_id:id,session_name:meta.name,project_hash:'fixture',working_dir:'/fixture',provider:'fixture',model:'fixture',mode:'build',messages:full});
const server=createServer(async(req,res)=>{
  const url=new URL(req.url,'http://localhost'), path=url.pathname;
  const json=data=>{res.setHeader('Content-Type','application/json');res.end(JSON.stringify(data));};
  if(path==='/live'){
    res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(`data: ${JSON.stringify(snapshot())}\n\ndata: {"type":"state","running":false}\n\n`);
    clients.add(res);req.on('close',()=>clients.delete(res));return;
  }
  if(path==='/workbench/preview'){
    const session=url.searchParams.get('session_id'), file=url.searchParams.get('path');
    previews.push({session,file});
    if(file==='missing.png'||file==='/outside.png'||file==='large.png'){
      res.statusCode=400;return json({error:file==='missing.png'?'File not found':file==='large.png'?'Preview is limited to 2 MiB':'Path is outside this session workspace'});
    }
    if(file==='slow.txt'){releaseSlow=()=>{res.setHeader('Content-Type','text/plain');res.end('Late stale file');};return;}
    if(file.endsWith('.png')){res.setHeader('Content-Type','image/png');res.end(Buffer.from(png,'base64'));return;}
    res.setHeader('Content-Type','text/plain');
    res.end(file.endsWith('.html')?'<script>window.fileExecuted=true</script>':`${session}: print("generated")`);return;
  }
  if(path==='/workbench/git'){
    if(notGit){res.statusCode=400;return json({error:'No Git repository',code:'workspace_not_git_repository'});}
    return json([{path:'unrelated.txt',index:' ',worktree:'M',stats:{additions:999,deletions:999}},
      {path:'gen_share_card.py',index:'M',worktree:' ',stats:{additions:12,deletions:3}},
      {path:'src/deeply/nested/directory/generated-file-1.ts',index:' ',worktree:'M',stats:{additions:33,deletions:4}}]);
  }
  if(path==='/workbench/diff'){
    if(url.searchParams.get('path')==='gen_share_card.py')assert.equal(url.searchParams.get('staged'),'true','a staged-only artifact opens the staged diff');
    return json({diff:`diff --git a/${url.searchParams.get('path')} b/${url.searchParams.get('path')}\n+fixture change`});
  }
  if(path.startsWith('/sessions/resolve/'))return json(meta);
  if(path===`/projects/fixture/sessions/${id}`)return json({...meta,messages:full});
  if(path===`/projects/fixture/sessions/${history.id}`)return json({...history,messages:full});
  if(path==='/project')return json({working_dir:'/fixture',project_hash:'fixture'});
  if(path==='/config')return json({providers:[],notifications:{enabled:false}});
  if(path==='/models')return json([{name:'fixture',model:'fixture',provider:'fixture',effort_levels:[]}]);
  if(path.endsWith('/transcript'))return json({session_id:id,turns:[]});
  if(path==='/live/mode')return json({mode:'build'});
  if(path==='/live/session')return json({success:true,session_id:id});
  if(path==='/live/switch_session'){switches++;return json({ok:true});}
  if(path==='/sessions'||path==='/projects/fixture/sessions')return json([meta,history]);
  if(path==='/chat/active'||path==='/projects'||path==='/skills'||path.startsWith('/workbench/'))return json([]);
  try{
    const file=resolve(root,path==='/'?'index.html':'.'+path);if(!file.startsWith(root+'/'))throw Error('path');
    res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)]??'application/octet-stream');res.end(await readFile(file));
  }catch{res.writeHead(404);res.end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
const emit=event=>{for(const client of clients)client.write(`data: ${JSON.stringify(event)}\n\n`);};
try {
 for(const width of [1200,390]){
  const page=await browser.newPage({viewport:{width,height:900}}), errors=[];
  page.on('pageerror',error=>errors.push(error.message));
  await page.addInitScript(()=>{localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');localStorage.setItem('atomcode.sidebar.view.v1','flat');});
  const original=`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`;
  await page.goto(original);
  const body=page.locator('.assistant-message-content').first();
  const link=name=>body.getByRole('link',{name,exact:true});
  await link('Image').waitFor();
  const settledUrl=page.url();
  assert.equal(await link('Website').getAttribute('href'),'https://example.com/output.png');
  assert.equal(await body.locator('a').filter({hasText:/^(Unsafe|Remote)$/}).count(),0,'blocked links are plain text');
  const modal=page.locator('dialog.file-preview-dialog[open]');
  const close=async()=>{await modal.getByRole('button',{name:'Close file preview'}).click();await modal.waitFor({state:'detached'});};
  await link('Image').click(); await modal.waitFor();
  await page.waitForFunction(()=>{const img=document.querySelector('.file-preview-body img');return img?.complete&&img.naturalWidth>0;});
  assert.deepEqual(previews.at(-1),{session:id,file:'/fixture/宣传 图.png'});
  const downloadEvent=page.waitForEvent('download');await modal.getByRole('link',{name:'Download'}).click();
  const download=await downloadEvent;assert.equal(download.suggestedFilename(),'宣传 图.png');
  assert.ok((await modal.boundingBox()).width<=width);
  await page.screenshot({path:`/tmp/atomcode-file-preview-${width}.png`});
  await page.evaluate(()=>document.documentElement.setAttribute('data-theme','dark'));
  await page.screenshot({path:`/tmp/atomcode-file-preview-dark-${width}.png`});
  await page.keyboard.press('Escape');await modal.waitFor({state:'detached'});
  assert.equal(await link('Image').evaluate(el=>document.activeElement===el),true);
  await link('Script').click();await modal.getByText(`${id}: print("generated")`,{exact:true}).waitFor();
  assert.deepEqual(previews.at(-1),{session:id,file:'./gen_share_card.py'});await close();
  const card=page.locator('.turn-artifacts');
  await card.getByText('Files modified this turn: 7',{exact:true}).waitFor();
  assert.equal(await card.locator('.turn-artifact-chip').count(),3);
  await card.getByRole('button',{name:'Show 4 more files'}).click();
  assert.equal(await card.locator('.turn-artifact-chip').count(),7);
  assert.equal(await card.locator('.turn-artifact-chip').nth(1).locator('.turn-artifact-path').innerText(),'src/deeply/nested/directory/generated-file-0.ts');
  await card.locator('.turn-artifact-added').filter({hasText:'+12'}).waitFor();
  assert.equal(await card.locator('.turn-artifact-removed').first().innerText(),'-3');
  assert.equal(await card.locator('.turn-artifact-chip').nth(1).locator('.turn-artifact-stats').count(),0);
  assert.equal(await card.getByText('+999',{exact:true}).count(),0,'unrelated files are not included');
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
  await card.getByRole('button',{name:'Show fewer files'}).click();
  await page.evaluate(()=>document.documentElement.setAttribute('data-theme','light'));
  await card.screenshot({path:`/tmp/atomcode-artifact-card-${width}.png`});
  await page.evaluate(()=>document.documentElement.setAttribute('data-theme','dark'));
  await card.screenshot({path:`/tmp/atomcode-artifact-card-dark-${width}.png`});
  await card.getByRole('button',{name:'View changes',exact:true}).click();
  await page.getByLabel('Git diff',{exact:true}).filter({hasText:'gen_share_card.py'}).waitFor();
  await page.getByRole('button',{name:'Close workspace panel'}).click();
  await card.locator('.turn-artifact-chip').first().click();await modal.locator('pre').waitFor();
  assert.deepEqual(previews.at(-1),{session:id,file:'./gen_share_card.py'});await close();
  await link('Windows').click();await modal.locator('pre').waitFor();
  assert.deepEqual(previews.at(-1),{session:id,file:'C:/workspace/result.py'});await close();
  await link('Raw HTML').click();await modal.locator('pre').waitFor();
  assert.match(await modal.locator('pre').innerText(),/<script>/);assert.equal(await page.evaluate(()=>window.fileExecuted),undefined);await close();
  for(const [name,error] of [['Missing','File not found'],['Outside','Path is outside'],['Large','2 MiB']]){
    await link(name).click();await modal.getByRole('alert').waitFor();assert.match(await modal.getByRole('alert').innerText(),new RegExp(error));await close();
  }
  await link('Slow').click();await modal.getByRole('status').waitFor();await close();
  await link('Script').click();await modal.locator('pre').waitFor();releaseSlow?.();
  await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
  assert.equal(await modal.getByText('Late stale file').count(),0);await close();
  assert.equal(page.url(),settledUrl,'local links never navigate away from chat');
  // Browse another session while the current one runs: requests must use the viewed session.
  emit({type:'state',running:true});await page.locator('.session-running-mark').waitFor();
  if(width<768){
    await page.locator('.mobile-menu-btn').click();
  }
  await page.locator('.session-item-main').filter({hasText:history.name}).click();
  await page.locator('.session-browse-card').waitFor();
  const before=switches;
  await page.locator('.assistant-message-content').getByRole('link',{name:'Script',exact:true}).click();
  await modal.getByText(`${history.id}: print("generated")`,{exact:true}).waitFor();
  assert.deepEqual(previews.at(-1),{session:history.id,file:'./gen_share_card.py'});assert.equal(switches,before);await close();
  assert.equal(await page.locator('.turn-artifacts').getByRole('button',{name:'View changes',exact:true}).count(),0);
  await page.locator('.turn-artifact-chip').first().click();await modal.locator('pre').waitFor();
  assert.deepEqual(previews.at(-1),{session:history.id,file:'./gen_share_card.py'});assert.equal(switches,before);await close();
  notGit=true;
  const missingGit=page.waitForResponse(response=>new URL(response.url()).pathname==='/workbench/git');
  await page.goto(original);
  await page.locator('.turn-artifacts').getByText('Files modified this turn: 7',{exact:true}).waitFor();
  await missingGit;
  assert.equal(await page.locator('.turn-artifacts-changes').count(),0);
  assert.equal(await page.locator('.turn-artifact-stats').count(),0);
  await page.locator('.turn-artifact-chip').first().click();await modal.locator('pre').waitFor();await close();
  notGit=false;
  assert.deepEqual(errors,[]);await page.close();
 }
 console.log('PASS: file links, decoded paths, image/text preview and download, errors, cancellation, history ownership, no rebind, mobile and dark layouts');
} finally {
 for(const client of clients)client.end();await browser.close();server.closeAllConnections();server.close();
}
