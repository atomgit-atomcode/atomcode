// Real app with synthetic catalog and SSE. Never touches user directories or sessions.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const alpha = 'a'.repeat(16), beta = 'b'.repeat(16);
const projects = [
  {hash:alpha,name:'Alpha',working_dir:'/fixture/alpha',session_count:1,created_at:1,last_updated:2},
  {hash:beta,name:'Beta',working_dir:'/fixture/beta',session_count:3,created_at:1,last_updated:1},
];
const meta = (id, hash) => ({id,name:`Session ${id}`,working_dir:`/fixture/${hash===alpha?'alpha':'beta'}`,project_hash:hash,created_at:1700000000,updated_at:1700000001,message_count:2});
let sessions = [meta('active',alpha),meta('b1',beta),meta('b2',beta),meta('b3',beta)];
let hideFails = false, clearFails = false, pinFails = false, projectListFails = false;
const clients = new Set();
const writes = [];
const server = createServer(async (req,res) => {
  const url = new URL(req.url,'http://localhost'), path=url.pathname;
  const json = value => {res.setHeader('Content-Type','application/json');res.end(JSON.stringify(value));};
  if(path==='/live') {
    res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});
    res.write(`data: ${JSON.stringify({type:'snapshot',session_id:'active',session_name:'Session active',project_hash:alpha,working_dir:'/fixture/alpha',provider:'fixture',model:'fixture',mode:'build',messages:[]})}\n\ndata: {"type":"state","running":false}\n\n`);
    clients.add(res);req.on('close',()=>clients.delete(res));return;
  }
  if(path==='/projects' && projectListFails){res.statusCode=503;return json({error:'catalog offline'});}
  if(path==='/projects')return json(projects.map(p=>({...p,session_count:sessions.filter(s=>s.project_hash===p.hash).length})));
  const project=projects.find(p=>path===`/projects/${p.hash}`);
  if(project && req.method==='DELETE') {
    writes.push(['hide',project.hash]);
    if(hideFails){res.statusCode=503;return json({error:'hide refused'});}
    project.hidden=true;return json({hidden:true});
  }
  if(project && req.method==='PATCH') {
    let body='';for await(const chunk of req)body+=chunk;
    const patch=JSON.parse(body);if(pinFails && 'pinned' in patch){res.statusCode=503;return json({error:'pin refused'});}Object.assign(project,patch);writes.push(['patch',project.hash,patch]);return json(project);
  }
  const deletion=/^\/projects\/([^/]+)\/sessions\/([^/]+)$/.exec(path);
  if(deletion && req.method==='DELETE') {
    writes.push(['delete-session',deletion[2]]);
    if(clearFails && deletion[2]==='b2'){res.statusCode=409;return json({error:'Session is in use',code:'SESSION_IN_USE'});}
    sessions=sessions.filter(s=>s.id!==deletion[2]);return json('deleted');
  }
  if(deletion) return json({...sessions.find(s=>s.id===deletion[2]),messages:[]});
  const bucket=/^\/projects\/([^/]+)\/sessions$/.exec(path);
  if(bucket)return json(sessions.filter(s=>s.project_hash===bucket[1]));
  if(path==='/sessions'||path==='/sessions/search')return json(sessions);
  if(path==='/project')return json({working_dir:'/fixture/alpha',project_hash:alpha});
  if(path==='/config')return json({providers:[],notifications:{enabled:false}});
  if(path==='/models')return json([{name:'fixture',model:'fixture',provider:'fixture',effort_levels:[]}]);
  if(path.startsWith('/sessions/resolve/'))return json(sessions[0]);
  if(path==='/live/mode')return json({mode:'build'});
  if(path==='/live/session')return json({success:true,session_id:'active'});
  if(path==='/chat/active'||path==='/skills')return json([]);
  if(path==='/fs/list')return json({path:url.searchParams.get('path')||'/fixture/alpha',dirs:[]});
  if(path==='/cd')return json({success:true,current_dir:'/fixture/alpha',project_hash:alpha,session_id:'active'});
  try {
    const file=resolve(root,path==='/'?'index.html':'.'+path);
    if(!file.startsWith(root+'/'))throw new Error('path');
    res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.css':'text/css'})[extname(file)]||'application/octet-stream');res.end(await readFile(file));
  }catch{res.statusCode=404;res.end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const base=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({executablePath:process.env.CONTEXT_BROWSER||'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
const errors=[];
try {
  for(const width of [1200,390]) {
    for(const p of projects){p.hidden=false;p.pinned=false;}
    sessions=[meta('active',alpha),meta('b1',beta),meta('b2',beta),meta('b3',beta)];
    const page=await browser.newPage({viewport:{width,height:900}});
    page.on('pageerror',error=>errors.push(error.message));
    await page.addInitScript(()=>{localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
    const openSidebar=async()=>{if(width<600 && !await page.locator('.app-sidebar.open').count())await page.locator('.mobile-menu-btn').click();};
    await page.goto(base+'/?sync=1');
    await page.locator('.message-input').waitFor();await openSidebar();
    const row=name=>page.locator('.sidebar-project').filter({has:page.locator('.sidebar-project-name',{hasText:name})});
    const openMenu=async name=>{await openSidebar();await row(name).locator('.sidebar-project-row').hover();await row(name).getByRole('button',{name:'Workspace actions',exact:true}).click();};
    await openMenu('Beta');
    assert.equal(await page.getByRole('menuitem').count(),3);
    const bounds=await page.getByRole('menu',{name:'Workspace actions'}).boundingBox();
    assert(bounds.x>=0&&bounds.y>=0&&bounds.x+bounds.width<=width+1,'menu fits viewport');
    await page.screenshot({path:`/tmp/atomcode-workspace-menu-${width}.png`});
    pinFails=true;await page.getByRole('menuitem',{name:'Pin workspace',exact:true}).click();
    await page.getByRole('menu',{name:'Workspace actions'}).getByRole('alert').waitFor();
    assert.equal(await page.locator('.sidebar-project-name').first().innerText(),'Alpha','failed pin must not reorder');
    pinFails=false;await page.getByRole('menuitem',{name:'Pin workspace',exact:true}).click();
    await page.waitForFunction(()=>document.querySelector('.sidebar-project-name')?.textContent==='Beta');
    await openMenu('Beta');hideFails=true;
    await page.getByRole('menuitem',{name:'Remove workspace',exact:true}).click();
    const dialog=page.getByRole('dialog',{name:'Remove workspace',exact:true});
    await dialog.getByRole('button',{name:'Remove workspace',exact:true}).click();
    await dialog.getByRole('alert').waitFor();assert.equal(await row('Beta').count(),1);
    hideFails=false;await dialog.getByRole('button',{name:'Remove workspace',exact:true}).click();
    await dialog.waitFor({state:'detached'});await row('Beta').waitFor({state:'detached'});
    assert.equal(sessions.filter(s=>s.project_hash===beta).length,3,'removing group keeps sessions');
    await page.reload();await page.locator('.message-input').waitFor();await openSidebar();
    assert.equal(await row('Beta').count(),0,'hidden group stays hidden after reload');
    // Re-add via the existing directory picker path (current Alpha is used below).
    projects[1].hidden=false;
    await page.reload();await page.locator('.message-input').waitFor();await openSidebar();
    await openMenu('Beta');await page.getByRole('menuitem',{name:'Clear sessions',exact:true}).click();
    const clearing=page.getByRole('dialog',{name:'Clear sessions',exact:true});
    clearFails=true;await clearing.getByRole('button',{name:'Clear sessions',exact:true}).click();
    await clearing.getByRole('alert').waitFor();
    assert.equal(await clearing.getByRole('status').innerText(),'Deleted 1 session(s)');
    assert.deepEqual(sessions.filter(s=>s.project_hash===beta).map(s=>s.id),['b2','b3']);
    clearFails=false;await clearing.getByRole('button',{name:'Clear sessions',exact:true}).click();
    await clearing.waitFor({state:'detached'});
    assert.equal(sessions.filter(s=>s.project_hash===beta).length,0);
    await row('Beta').waitFor();assert.equal(await row('Beta').count(),1,'clear retains empty group');
    await openMenu('Alpha');await page.getByRole('menuitem',{name:'Remove workspace',exact:true}).click();
    projectListFails=true;
    const failedRefresh=page.waitForResponse(response=>new URL(response.url()).pathname==='/projects' && response.status()===503);
    await page.getByRole('dialog',{name:'Remove workspace',exact:true}).getByRole('button',{name:'Remove workspace',exact:true}).click();
    await failedRefresh;
    await row('Alpha').waitFor({state:'detached'});
    projectListFails=false;
    assert(sessions.some(s=>s.id==='active'),'removing current workspace keeps its session');
    await page.reload();await page.locator('.message-input').waitFor();await openSidebar();
    assert.equal(await row('Alpha').count(),0,'active placeholder must not resurrect removed group');
    await page.getByRole('button',{name:'Add workspace',exact:true}).click();
    await page.locator('.cwd-picker').waitFor();
    const recentBeta=page.locator('.cwd-recent-list .list-row').filter({hasText:'/fixture/beta'});
    await recentBeta.waitFor();hideFails=true;
    await recentBeta.getByRole('button',{name:'Remove workspace',exact:true}).click();
    await page.locator('.cwd-picker .dir-note.error').waitFor();
    assert.equal(await recentBeta.count(),1,'failed recent-project removal must keep its row');
    hideFails=false;
    await page.locator('.cwd-picker .modal-footer .btn-primary').click();
    await page.locator('.cwd-picker').waitFor({state:'detached'});await openSidebar();
    await row('Alpha').waitFor();assert.equal(projects[0].hidden,false,'explicit re-add restores group');
    for(const client of clients)client.write('data: {"type":"state","running":true}\n\n');
    await page.getByText('Running', {exact:false}).first().waitFor();
    await openMenu('Alpha');await page.getByRole('menuitem',{name:'Clear sessions',exact:true}).click();
    const runningDialog=page.getByRole('dialog',{name:'Clear sessions',exact:true});
    const deletesBefore=writes.filter(w=>w[0]==='delete-session').length;
    await runningDialog.getByRole('button',{name:'Clear sessions',exact:true}).click();
    await runningDialog.getByRole('alert').filter({hasText:'running session'}).waitFor();
    assert.equal(writes.filter(w=>w[0]==='delete-session').length,deletesBefore,'running workspace refuses before deleting history');
    await runningDialog.getByRole('button',{name:'Cancel',exact:true}).last().click();
    await page.close();
  }
  assert.deepEqual(errors,[]);
  console.log('Workspace menu, pin/hide persistence, failure/retry, partial clear, current workspace and re-add passed at desktop/mobile widths.');
} finally {await browser.close();for(const client of clients)client.end();server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
