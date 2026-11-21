// Isolated UI fixture; never touches a real session or MCP connection.
import { createServer } from 'vite';
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
const fixture = `import {h,render} from 'preact'; import {Sidebar} from '/src/components/Sidebar.tsx'; import {SettingsProvider} from '/src/settings.tsx'; import '/src/styles/theme.css'; import '/src/styles/app.css'; window.selectSession = session => render(h(SettingsProvider,null,h(Sidebar,{activeSessionId:session,onSelect(){},onNew(){},onOpenSettings(){},cwd:'/fixture'})),document.getElementById('root')); window.selectSession('a');`;
const server = await createServer({root:resolve(import.meta.dirname,'..'),server:{host:'127.0.0.1',port:0},plugins:[{
  name:'mcp-fixture',configureServer(s){s.middlewares.use((req,res,next)=>{if(req.url!='/__mcp_fixture')return next();res.setHeader('Content-Type','text/html');res.end('<html><body><div id="root"></div><script type="module" src="/__mcp_fixture.js"></script></body></html>');});},
  resolveId(id){if(id==='/__mcp_fixture.js')return '\0mcp-fixture.js';},load(id){if(id==='\0mcp-fixture.js')return fixture;},
}]});
await server.listen();
const browser=await chromium.launch({executablePath:process.env.MCP_SESSION_BROWSER??'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true});
try {
 const page=await browser.newPage({viewport:{width:1100,height:700}}), errors=[],writes=[],states={a:false,b:false};
 let fail=false,hold=false,release;
 page.on('pageerror',e=>{ errors.push(e.message); console.error(e.message); });
 await page.route('**/live/mcp/session*',async route=>{
  if(route.request().method()==='GET')return route.fulfill({json:{paused:states[new URL(route.request().url()).searchParams.get('session')]}});
  const body=route.request().postDataJSON();writes.push(body);
  if(fail)return route.fulfill({status:409,json:{error:'Finish the current task first'}});
  if(hold)await new Promise(resolve=>{release=resolve;});
  if(!hold)states[body.session]=body.paused;return route.fulfill({json:{paused:body.paused}});
 });
 await page.route('**/mcp/status',r=>r.fulfill({json:{servers:[{name:'fixture',status:'connected',tool_count:1}],trusted:true,blocked:[]}}));
 for(const path of ['sessions','projects','skills'])await page.route(url=>url.pathname===`/${path}`,r=>r.fulfill({json:[]}));
 await page.route('**/auth/status',r=>r.fulfill({json:{authenticated:false}}));
 await page.goto(server.resolvedUrls.local[0]+'__mcp_fixture');

 await page.locator('.sidebar-action').filter({hasText:'MCP'}).click();
 const pause=()=>page.locator('.mcp-menu button').filter({hasText:/暂停全部|Pause all/});
 const resume=()=>page.locator('.mcp-menu button').filter({hasText:/恢复|Resume/});
 await pause().waitFor();await pause().click();await resume().waitFor();assert.deepEqual(writes.at(-1),{session:'a',paused:true});
 await resume().click();await pause().waitFor();assert.deepEqual(writes.at(-1),{session:'a',paused:false});
 fail=true;await pause().click();await page.getByRole('alert').waitFor();assert.match(await page.getByRole('alert').textContent(),/Finish the current task/);assert.equal(await pause().count(),1);fail=false;
 hold=true;await pause().click();await page.waitForFunction(()=>document.querySelector('.mcp-menu button')?.disabled);
 await page.evaluate(()=>window.selectSession('b'));
 await page.waitForFunction(()=>document.querySelector('.mcp-session-control')?.textContent.includes('本会话 MCP 已开启'));
 await page.evaluate(()=>window.selectSession('a'));
 await page.waitForFunction(()=>document.querySelector('.mcp-session-control')?.textContent.includes('本会话 MCP 已开启'));
 while(!release)await new Promise(resolve=>setTimeout(resolve,5));release();await page.waitForFunction(()=>!document.querySelector('.mcp-menu button')?.disabled);await pause().waitFor();
 assert.equal(states.b,false);assert.deepEqual(writes.at(-1),{session:'a',paused:true});
 await page.screenshot({path:'/tmp/atomcode-mcp-session.png'});assert.deepEqual(errors,[]);
 console.log('MCP pause/resume, busy refusal and stale-session isolation passed');
}finally{await browser.close();await server.close();}
