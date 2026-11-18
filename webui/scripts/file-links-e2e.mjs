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

[Raw HTML](./sample.html) · [Guide](./docs/guide.md) · [Both](./both.ts) · [Unchanged](./clean.ts) · [Symlink parent](file:///fixture/alias/../target.txt)`;
const full=[{role:'user',content:'Show the generated files',source_seq:2},
  {role:'assistant',tool_calls:[{id:'generated-script',name:'write_file',arguments:'{"file_path":"./gen_share_card.py","content":"print(1)"}'}]},
  {role:'tool',tool_result:{call_id:'generated-script',success:true,summary:'Written'}},
  ...Array.from({length:6},(_,i)=>[
    {role:'assistant',tool_calls:[{id:`extra-${i}`,name:'write_file',arguments:JSON.stringify({file_path:`/fixture/src/deeply/nested/directory/generated-file-${i}.ts`,content:'export {}'})}]},
    {role:'tool',tool_result:{call_id:`extra-${i}`,success:true,summary:'Written'}},
  ]).flat(),
  {role:'assistant',content}];
const clients=new Set(), previews=[], diffs=[], writes=[];
let switches=0, releaseSlow, notGit=false;
const snapshot=()=>({type:'snapshot',session_id:id,session_name:meta.name,project_hash:'fixture',working_dir:'/fixture',provider:'fixture',model:'fixture',mode:'build',messages:full});
const server=createServer(async(req,res)=>{
  const url=new URL(req.url,'http://localhost'), path=url.pathname;
  if(req.method && !['GET','HEAD'].includes(req.method) && path.startsWith('/workbench/')) writes.push({method:req.method,path});
  const json=data=>{res.setHeader('Content-Type','application/json');res.end(JSON.stringify(data));};
  if(path==='/live'){
    res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(`data: ${JSON.stringify(snapshot())}\n\ndata: {"type":"state","running":false}\n\n`);
    clients.add(res);req.on('close',()=>clients.delete(res));return;
  }
  if(path==='/workbench/text' && req.method==='GET')return json({content:'original text',revision:'fixture-revision'});
  if(path==='/workbench/preview'){
    const session=url.searchParams.get('session_id'), file=url.searchParams.get('path');
    previews.push({session,file});
    const relative=file.replace(/^\/fixture\//,'').replace(/^\.\//,'');
    if(relative==='missing.png'||file==='/outside.png'||relative==='large.png'){
      res.statusCode=400;return json({error:relative==='missing.png'?'File not found':relative==='large.png'?'Preview is limited to 2 MiB':'Path is outside this session workspace'});
    }
    if(relative==='alias/../target.txt'||relative==='target.txt'){
      res.setHeader('Content-Type','text/plain');
      return res.end(relative==='alias/../target.txt'?'Resolved through symlink parent':'Incorrect lexical target');
    }
    if(relative==='slow.txt'){releaseSlow=()=>{res.setHeader('Content-Type','text/plain');res.end('Late stale file');};return;}
    if(file.endsWith('.png')){res.setHeader('Content-Type','image/png');res.end(Buffer.from(png,'base64'));return;}
    if(relative==='docs/guide.md'){res.setHeader('Content-Type','text/markdown');res.end('# Guide\n\n[Child](./child.txt)');return;}
    res.setHeader('Content-Type','text/plain');
    res.end(file.endsWith('.html')?'<script>window.fileExecuted=true</script>':`${session}: print("generated")`);return;
  }
  if(path==='/workbench/git'){
    if(notGit){res.statusCode=400;return json({error:'No Git repository',code:'workspace_not_git_repository'});}
    return json([{path:'unrelated.txt',index:' ',worktree:'M',stats:{additions:999,deletions:999}},
      {path:'both.ts',index:'M',worktree:'M',stats:{additions:2,deletions:1}},
      {path:'gen_share_card.py',index:'M',worktree:' ',stats:{additions:12,deletions:3}},
      {path:'src/deeply/nested/directory/generated-file-1.ts',index:' ',worktree:'M',stats:{additions:33,deletions:4}}]);
  }
  if(path==='/workbench/diff'){
    diffs.push({session:url.searchParams.get('session_id'),path:url.searchParams.get('path'),staged:url.searchParams.get('staged')});
    if(url.searchParams.get('path')==='gen_share_card.py')assert.equal(url.searchParams.get('staged'),'true','a staged-only artifact opens the staged diff');
    return json({diff:`diff --git a/${url.searchParams.get('path')} b/${url.searchParams.get('path')}\n+fixture ${url.searchParams.get('staged')==='true'?'index':'worktree'} change`});
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
  const panel=page.locator('.workbench:visible');
  const preview=()=>panel.locator('.workbench-preview');
  const close=async()=>{await panel.getByRole('button',{name:'Close workspace panel',exact:true}).click();await panel.waitFor({state:'hidden'});};
  const noModal=async()=>assert.equal(await page.locator('dialog.file-preview-dialog').count(),0,'all local file entries use workspace viewer');
  const read=async(name)=>{await link(name).click();await panel.waitFor();await preview().getByText(`${id}: print("generated")`,{exact:true}).waitFor();await noModal();};
  await link('Image').click();await panel.waitFor();
  await panel.locator('.workbench-preview img').waitFor();
  await page.waitForFunction(()=>{const img=document.querySelector('.workbench-preview img');return img?.complete&&img.naturalWidth>0;});
  assert.equal(previews.at(-1).session,id);assert.match(previews.at(-1).file,/宣传 图\.png$/);
  const downloadEvent=page.waitForEvent('download');await panel.getByRole('link',{name:'Download',exact:true}).click();
  const download=await downloadEvent;assert.equal(download.suggestedFilename(),'宣传 图.png');
  const box=await panel.boundingBox();assert.ok(box.width<=width);
  if(width<850)assert.ok(Math.abs(box.width-width)<2,'mobile viewer fills viewport');
  await noModal();
  await page.screenshot({path:`/tmp/atomcode-unified-viewer-${width}.png`});
  await page.evaluate(()=>document.documentElement.setAttribute('data-theme','dark'));
  await page.screenshot({path:`/tmp/atomcode-unified-viewer-dark-${width}.png`});
  await close();await page.waitForFunction(()=>document.activeElement?.textContent==='Image');
  await read('Script');await close();
  const card=page.locator('.turn-artifacts');
  await card.getByText('Files modified this turn: 7',{exact:true}).waitFor();
  assert.equal(await card.locator('.turn-artifact-chip').count(),3);
  const alignment=await card.evaluate(el=>{const body=el.closest('.timeline-message')?.querySelector('.assistant-message-content');return {card:el.getBoundingClientRect().width,body:body?.getBoundingClientRect().width};});
  assert.ok(alignment.body&&Math.abs(alignment.card-alignment.body)<3,`card width: ${JSON.stringify(alignment)}`);
  await card.getByRole('button',{name:'Show 4 more files'}).click();
  assert.equal(await card.locator('.turn-artifact-chip').count(),7);
  await card.locator('.turn-artifact-added').filter({hasText:'+12'}).waitFor();
  assert.equal(await card.getByText('+999',{exact:true}).count(),0);
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
  await card.getByRole('button',{name:'Show fewer files'}).click();
  await card.screenshot({path:`/tmp/atomcode-artifact-card-${width}.png`});
  await card.getByRole('button',{name:'View changes',exact:true}).click();
  await panel.getByLabel('Current Git diff',{exact:true}).filter({hasText:'gen_share_card.py'}).waitFor();await close();
  await card.locator('.turn-artifact-chip').first().click();await preview().getByText(`${id}: print("generated")`,{exact:true}).waitFor();await noModal();
  await close();await page.locator('.workbench-toggle').click();
  await preview().getByText(`${id}: print("generated")`,{exact:true}).waitFor();await close();
  await card.locator('.turn-artifact-diff').first().click();
  await panel.getByLabel('Current Git diff',{exact:true}).filter({hasText:'gen_share_card.py'}).waitFor();await close();
  await read('Both');
  await page.screenshot({path:`/tmp/atomcode-unified-content-${width}.png`});
  await panel.getByRole('button',{name:'Current Git diff',exact:true}).click();
  await panel.getByLabel('Current Git diff',{exact:true}).filter({hasText:'fixture worktree change'}).waitFor();
  await panel.getByRole('button',{name:'Staged',exact:true}).click();
  await panel.getByLabel('Current Git diff',{exact:true}).filter({hasText:'fixture index change'}).waitFor();
  assert.deepEqual(diffs.at(-1),{session:id,path:'both.ts',staged:'true'});
  await page.screenshot({path:`/tmp/atomcode-unified-diff-${width}.png`});
  await panel.getByRole('button',{name:'Content',exact:true}).click();await preview().locator('pre').waitFor();await close();
  await read('Both');
  const editor=panel.getByRole('textbox',{name:'File editor',exact:true});
  await panel.getByRole('button',{name:'Edit',exact:true}).click();
  await editor.fill('unsaved viewer draft');
  const writesBeforePreview=writes.length;
  page.once('dialog', dialog=>dialog.accept());
  await close();
  await link('Both').click();
  await preview().getByText('unsaved viewer draft',{exact:true}).waitFor();
  assert.equal(await editor.count(),0,'explicit content opening leaves edit mode');
  await panel.getByRole('button',{name:'Edit',exact:true}).click();
  assert.equal(await editor.inputValue(),'unsaved viewer draft');
  assert.equal(writes.length,writesBeforePreview,'preview writes no draft to disk');
  await editor.fill('original text');
  await close();
  await link('Symlink parent').click();
  await preview().getByText('Resolved through symlink parent',{exact:true}).waitFor();
  assert.equal(previews.at(-1).file,'/fixture/alias/../target.txt');
  await close();
  await read('Unchanged');await panel.getByRole('button',{name:'Current Git diff',exact:true}).click();
  await panel.getByText('This file has no current Git changes',{exact:true}).waitFor();await close();
  await read('Windows');assert.equal(previews.at(-1).file,'C:/workspace/result.py');await close();
  await link('Raw HTML').click();await preview().locator('pre').waitFor();
  assert.match(await preview().locator('pre').innerText(),/<script>/);assert.equal(await page.evaluate(()=>window.fileExecuted),undefined);await close();
  await link('Guide').click();await preview().getByRole('heading',{name:'Guide',exact:true}).waitFor();
  await preview().getByRole('link',{name:'Child',exact:true}).click();
  await preview().getByText(`${id}: print("generated")`,{exact:true}).waitFor();
  assert.equal(previews.at(-1).session,id);assert.match(previews.at(-1).file,/docs\/(?:\.\/)?child\.txt$/);await close();
  for(const [name,error] of [['Missing','File not found'],['Outside','Path is outside'],['Large','2 MiB']]){
    await link(name).click();await panel.getByRole('alert').filter({hasText:new RegExp(error)}).waitFor();await noModal();await close();
  }
  await link('Slow').click();await preview().getByRole('status').waitFor();await close();
  await read('Script');releaseSlow?.();
  await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
  assert.equal(await panel.getByText('Late stale file').count(),0);await close();
  assert.equal(page.url(),settledUrl,'local links never navigate chat');
  emit({type:'state',running:true});await page.locator('.session-running-mark').waitFor();
  if(width<768)await page.locator('.mobile-menu-btn').click();
  await page.locator('.session-item-main').filter({hasText:history.name}).click();
  await page.locator('.session-browse-card').waitFor();
  const before=switches;const writesBefore=writes.length;
  await page.locator('.messages-container').getByRole('link',{name:'Script',exact:true}).click();
  await preview().getByText(`${history.id}: print("generated")`,{exact:true}).waitFor();
  assert.equal(previews.at(-1).session,history.id);assert.equal(switches,before);
  assert.equal(await panel.getByRole('button',{name:'Edit',exact:true}).count(),0);
  assert.equal(await panel.getByRole('button',{name:'Terminal',exact:true}).count(),0);
  await panel.getByText(/read.only/i).first().waitFor();await noModal();
  await panel.getByRole('button',{name:'Current Git diff',exact:true}).click();
  await panel.getByLabel('Current Git diff',{exact:true}).filter({hasText:'gen_share_card.py'}).waitFor();
  assert.equal(diffs.at(-1).session,history.id,'historical diff retains its session owner');
  assert.equal(switches,before);assert.equal(writes.length,writesBefore);
  await panel.getByRole('button',{name:'Content',exact:true}).click();await preview().locator('pre').waitFor();
  await page.screenshot({path:`/tmp/atomcode-unified-history-${width}.png`});await close();
  await page.locator('.turn-artifact-chip').first().click();await preview().locator('pre').waitFor();
  assert.equal(previews.at(-1).session,history.id);assert.equal(switches,before);assert.equal(writes.length,writesBefore);await close();
  notGit=true;
  await page.goto(original);await link('Script').waitFor();await read('Script');
  await panel.getByRole('button',{name:'Current Git diff',exact:true}).click();
  await panel.getByText(/not a Git repository/).waitFor();await close();notGit=false;

  assert.deepEqual(errors,[]);await page.close();
 }
 console.log('PASS: unified file viewer, content/diff/index, nested Markdown, download, errors, cancellation, read-only history, no rebind, mobile and dark layouts');
} finally {
 for(const client of clients)client.end();await browser.close();server.closeAllConnections();server.close();
}
