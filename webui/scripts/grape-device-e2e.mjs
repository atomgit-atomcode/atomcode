// Real native CLI + local Grape device routes + isolated headless consent browser.
// No model calls, real account, remote repository or user credential is used.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdtemp,readFile,stat,rm,mkdir} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {chromium} from 'playwright';

const binary=resolve(process.argv[2] || 'target/debug/atomcode');
const source=process.env.GRAPE_SOURCE;
assert(source,'Set GRAPE_SOURCE to the Grape worktree containing the device routes');
const dir=await mkdtemp(join(tmpdir(),'atomcode-grape-device-'));
await mkdir(join(dir,'user'));
const child=spawn(process.env.GRAPE_PYTHON || 'python3',[join(source,'tests/fixtures/atomcode_device_server.py')],{
  cwd:source,env:{...process.env,DEVOPS_DB_PATH:join(dir,'grape.db'),DEVOPS_DB_BACKEND:'sqlite',DEVOPS_WORKSPACE_DIR:join(dir,'workspace'),JWT_SECRET:'test-only-device-smoke-signing-key-1234567890'},stdio:['ignore','pipe','pipe'],
});
let buffer='';
const ready=new Promise((resolve,reject)=>{
  child.on('error',reject);child.on('exit',()=>reject(new Error('Local fixture exited before readiness')));
  child.stdout.on('data',chunk=>{buffer+=chunk;for(const line of buffer.split('\n'))if(line.startsWith('ATOMCODE_DEVICE_SMOKE='))resolve(JSON.parse(line.slice('ATOMCODE_DEVICE_SMOKE='.length)));});
});
const env={...process.env,ATOMCODE_HOME:join(dir,'user'),ATOMCODE_PROXY_MODE:'no_proxy'};
function cli(args){
  const process=spawn(binary,['--no-telemetry','grape',...args],{env,stdio:['ignore','pipe','pipe']});
  let output='',errors='';
  const done=new Promise((resolve,reject)=>{process.on('error',reject);process.on('exit',code=>resolve({code,output,errors}));});
  process.stdout.on('data',chunk=>{output+=chunk;});process.stderr.on('data',chunk=>{errors+=chunk;});
  return {process,done,url:()=>errors.match(/http:\/\/127\.0\.0\.1:\d+\/api\/auth\/device\/verify\?user_code=[A-Z2-9-]+/)?.[0]};
}
async function until(fn){for(let n=0;n<200;n++){const result=await fn();if(result)return result;await new Promise(resolve=>setTimeout(resolve,50));}throw new Error('Local consent step timed out');}
let browser,active;
try{
  const fixture=await Promise.race([ready,new Promise((_,reject)=>setTimeout(()=>reject(new Error('Fixture startup timed out')),20000).unref())]);
  await until(()=>fetch(fixture.url+'/api/auth/device/verify').then(r=>r.ok).catch(()=>false));
  browser=await chromium.launch({executablePath:process.env.GRAPE_BROWSER || (process.platform==='darwin'?'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome':undefined),headless:true});
  active=cli(['connect','--server',fixture.url,'--no-browser']);
  const url=await until(()=>active.url());
  const page=await browser.newPage({viewport:{width:1000,height:800}});
  await page.goto(url);
  await page.getByRole('button',{name:'登录 Grape（可选择 GitCode）',exact:true}).waitFor();
  await page.evaluate(token=>localStorage.setItem('token',token),fixture.browser_token);
  await page.reload();
  await page.getByText('以 atomcode_device_smoke 的身份连接',{exact:true}).waitFor();
  assert.equal(active.process.exitCode,null,'opening a logged-in page must not auto-approve');
  await page.screenshot({path:'/tmp/atomcode-grape-consent.png'});
  await page.getByRole('button',{name:'授权连接',exact:true}).click();
  const connected=await Promise.race([active.done,new Promise((_,reject)=>setTimeout(()=>reject(new Error('CLI did not finish consent')),15000).unref())]);
  assert.equal(connected.code,0,connected.errors);
  const path=join(dir,'user','grape-auth.json');
  let credential=JSON.parse(await readFile(path,'utf8'));
  assert.equal(credential.username,'atomcode_device_smoke');
  assert(!(connected.output+connected.errors).includes(credential.secret),'credential must never be printed');
  if(process.platform!=='win32')assert.equal((await stat(path)).mode&0o777,0o600);
  let headers={Authorization:'Bearer '+credential.secret};
  assert.equal((await fetch(fixture.url+'/api/repos/managed',{headers})).status,200);
  assert.equal((await fetch(fixture.url+'/api/settings/api-keys',{method:'POST',headers})).status,403);
  const status=await cli(['status']).done;assert.equal(status.code,0);assert(!status.output.includes(credential.secret));
  // Reauthorization replaces the local credential and revokes its predecessor.
  active=cli(['connect','--server',fixture.url,'--no-browser']);
  await page.goto(await until(()=>active.url()));
  await page.getByRole('button',{name:'授权连接',exact:true}).click();
  const rotated=await active.done;assert.equal(rotated.code,0,rotated.errors);
  assert.equal((await fetch(fixture.url+'/api/repos/managed',{headers})).status,401);
  credential=JSON.parse(await readFile(path,'utf8'));
  headers={Authorization:'Bearer '+credential.secret};
  assert.equal((await fetch(fixture.url+'/api/repos/managed',{headers})).status,200);
  assert(!(rotated.output+rotated.errors).includes(credential.secret));
  const before=await readFile(path,'utf8');
  if (process.platform !== 'win32') {
    active=cli(['connect','--server',fixture.url,'--no-browser']);
    await until(()=>active.url());
    active.process.kill('SIGINT');
    const cancelled=await Promise.race([active.done,new Promise((_,reject)=>setTimeout(()=>reject(new Error('CLI did not stop after interrupt')),3000).unref())]);
    assert.notEqual(cancelled.code,0);
    assert.equal(await readFile(path,'utf8'),before,'cancelled authorization preserves the existing credential');
  }
  active=cli(['connect','--server',fixture.url,'--no-browser']);
  await page.goto(await until(()=>active.url()));
  await page.getByRole('button',{name:'拒绝',exact:true}).click();
  const denied=await active.done;assert.notEqual(denied.code,0);assert.equal(await readFile(path,'utf8'),before);
  const disconnected=await cli(['disconnect']).done;assert.equal(disconnected.code,0,disconnected.errors);
  assert.equal((await fetch(fixture.url+'/api/repos/managed',{headers})).status,401);
  assert.equal(JSON.parse((await cli(['status']).done).output).connected,false);
  await page.close();
  console.log('Grape device consent: browser approval, native credential persistence, scoped use, key rotation, denial preservation and revocation passed.');
}finally{
  active?.process.kill('SIGTERM');child.kill('SIGTERM');await browser?.close();await rm(dir,{recursive:true,force:true});
}
