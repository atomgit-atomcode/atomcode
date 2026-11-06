// Synthetic host, real WebUI: opening and reviewing hooks must not execute scripts.
import { chromium } from 'playwright';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve(import.meta.dirname, '../dist');
const id = 'hooks-fixture';
let executions = 0;
let chatSubmissions = 0;
let lastBody;
let responseGate = Promise.resolve();
const configured = {id:'configured-hook',name:'audit-script',event:'PreToolUse',matcher:'bash',command:'echo safe <script>not markup</script>',args:['explicit-script.py'],scope:'project',source:'/fixture/.hooks.json',timeout_ms:10000,disabled:false,testable:true};
const disabled = {...configured,id:'disabled-hook',name:'disabled-script',disabled:true,testable:false};
const meta = { id, name: 'Context summary', working_dir: '/fixture', project_hash: 'fixture', created_at: 1700000000, updated_at: 1700000001, message_count: 2 };
const messages = [{ role: 'user', content: 'Check the resource boundary' }, { role: 'assistant', content: 'The resource read boundary is ready.' }];
const clients = new Set();
let baseline = 3;
const snapshot = () => ({ type: 'snapshot', session_id: id, session_name: meta.name, project_hash: 'fixture', working_dir: '/fixture', provider: 'fixture', model: 'fixture', mode: 'build', summary_count: baseline, messages });
const server = createServer(async (req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (path === '/live') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
    res.write(`data: ${JSON.stringify(snapshot())}\n\n`);
    res.write('data: {"type":"state","running":false}\n\n');
    clients.add(res); req.on('close', () => clients.delete(res)); return;
  }
  if (path === '/project') return json({ working_dir: '/fixture', project_hash: 'fixture' });
  if (path === '/config') return json({ providers: [], notifications: { enabled: false } });
  if (path === '/models') return json([{ name: 'fixture', model: 'fixture', provider: 'fixture', effort_levels: [] }]);
  if (path.startsWith('/sessions/resolve/')) return json(meta);
  if (path === '/projects/fixture/sessions/' + id) return json({ ...meta, messages, summary_count: baseline });
  if (path.endsWith('/transcript')) return json({ session_id: id, turns: [] });
  if (path === '/live/mode') return json({ mode: 'build' });
  if (path === '/live/session') return json({ success: true, session_id: id });
  if (path === '/chat/active') return json([]);
  if (path === '/sessions' || path === '/projects/fixture/sessions') return json([meta]);
  if (path === '/hooks') return json({cwd:'/fixture',items:[configured,disabled],files:[{scope:'project',path:'/fixture/.hooks.json',status:{status:'malformed',error:'fixture parse error'}}],plugins:[{plugin:'untrusted-plugin',trusted:false,invalid:false,hooks:1,events:['Stop']}]});
  if (path === '/hooks/test') {
    let body='';for await(const chunk of req) body+=chunk;
    lastBody=JSON.parse(body);executions++;
    await responseGate;
    assert.deepEqual(lastBody,{id:'configured-hook'});
    return json({item:configured,result:{completed:true,exit_code:2,stdout:'diagnostic stdout',stderr:'deliberate refusal'}});
  }
  if (path === '/chat') {chatSubmissions++;return json({});}
  if (path === '/projects' || path === '/skills') return json([]);
  try {
    const file = resolve(root, path === '/' ? 'index.html' : '.' + path);
    if (!file.startsWith(root + '/')) throw new Error('path');
    const data = await readFile(file);
    res.setHeader('Content-Type', ({ '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css' })[extname(file)] ?? 'application/octet-stream'); res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: process.env.HOOKS_BROWSER ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
const errors = [];
try {
  for (const width of [1200,390]) {
    const page = await browser.newPage({viewport:{width,height:900}});
    page.on('pageerror',error=>errors.push(error.message));
    await page.addInitScript(()=>{localStorage.setItem('atomcode.lang','en');localStorage.setItem('atomcode.onboarding.done','1');});
    await page.goto(`http://127.0.0.1:${server.address().port}/?sync=1&session=${id}`);
    const before=executions;
    const input=page.locator('.message-input');
    await input.fill('/hooks ');
    await input.press('Enter');
    const dialog=page.getByRole('dialog',{name:'Hooks configuration'});
    await dialog.waitFor();
    await dialog.getByText('audit-script',{exact:true}).first().waitFor();
    assert.equal(executions,before);
    await dialog.getByText('args: ["explicit-script.py"]',{exact:true}).waitFor();
    await dialog.getByText('Paths, diagnostics and plugin trust',{exact:true}).click();
    await dialog.getByText('fixture parse error',{exact:false}).waitFor();
    await dialog.getByText('untrusted-plugin',{exact:false}).waitFor();
    const test=dialog.getByRole('button',{name:'Run script test (up to 30 seconds)',exact:true});
    await dialog.getByRole('button').filter({hasText:'disabled-script'}).click();
    assert(await test.isDisabled());
    assert.equal(executions,before);
    await dialog.getByRole('button').filter({hasText:'audit-script'}).click();
    let finishTest;
    responseGate = new Promise(resolve => { finishTest = resolve; });
    await test.click();
    await dialog.getByRole('button',{name:'Running script test…',exact:true}).waitFor();
    await page.keyboard.press('Tab');
    assert(await page.evaluate(()=>Boolean(document.activeElement?.closest('[role="dialog"]'))));
    await page.keyboard.press('Escape');
    assert(await dialog.isVisible(),'Escape must not escape a running script-test dialog');
    finishTest();
    await dialog.getByText('diagnostic stdout',{exact:true}).waitFor();
    await dialog.getByText('deliberate refusal',{exact:true}).waitFor();
    assert.equal(executions,before+1);
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
    await page.screenshot({path:`/tmp/atomcode-hooks-${width}.png`,fullPage:true});
    await page.keyboard.press('Escape');
    assert(await dialog.isVisible(), 'Escape must not dismiss the completed hooks dialog');
    await page.mouse.click(5, 5);
    assert(await dialog.isVisible(), 'backdrop click must not dismiss hooks');
    await dialog.locator('.modal-close').click();
    await dialog.waitFor({state:'hidden'});
    await page.close();
  }
  assert.deepEqual(errors,[]);
  assert.equal(chatSubmissions,0);
  console.log('Passed: /hooks dialog, diagnostics/trust, no automatic execution, disabled test guard, exact ID, result display and Escape at 1200/390px.');
} finally {
  await browser.close();for(const client of clients) client.end();await new Promise(resolve=>server.close(resolve));
}
