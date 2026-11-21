#!/usr/bin/env node
'use strict';
// Opt-in negative-control experiment, NOT a backend or supported permission mode.
// Uses only generated loopback fixtures, an isolated Chrome and owned scratch files.
const fs=require('node:fs'),path=require('node:path'),os=require('node:os');
const https=require('node:https'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const {spawnSync}=require('node:child_process');const {once}=require('node:events');
const {createRequire}=require('node:module');const {loadBackend}=require('./backend.cjs');
const CHILD_SOURCE=String.raw`const fs=require('node:fs'),path=require('node:path');
const settings=JSON.parse(fs.readFileSync(process.argv[2]));
const {loadBackend}=require(path.join(settings.moduleRoot,'backend.cjs'));
const {createProxy}=require(path.join(settings.moduleRoot,'policy.cjs'));
(async()=>{

 const external=loadBackend(settings.backendRoot);const proxy=await createProxy(new Set(settings.accessOrigins));
 const browser=await external.chromium.launch({channel:'chrome',headless:true,proxy:{server:proxy.url,bypass:'<-loopback>'},args:['--ignore-certificate-errors-spki-list='+settings.pin,'--disable-quic','--disable-background-networking','--dns-prefetch-disable','--force-webrtc-ip-handling-policy=disable_non_proxied_udp']});
 fs.writeFileSync(settings.metadata,JSON.stringify({chrome:browser.version(),node:process.version,dependency_hashes:external.sourceHashes}));
 const context=await browser.newContext({acceptDownloads:true,serviceWorkers:'block',permissions:[]});
 function record(value){fs.appendFileSync(settings.events,JSON.stringify(value)+'\n');}
 context.on('page',page=>page.prependListener('download',download=>{
  const origin=new URL(download.url()).origin;const allowed=origin===settings.downloadOrigin;
  record({kind:'download',url:download.url(),allowed});
  if(!allowed&&settings.delay){const until=Date.now()+1000;while(Date.now()<until){};}
  if(!allowed)void download.cancel().then(()=>record({kind:'cancelled',url:download.url()}),()=>record({kind:'cancel-error',url:download.url()}));
 }));
 const server=await external.createConnection({browser:{isolated:false},webmcp:false,extension:false,capabilities:[],ignoreRoots:true,allowUnrestrictedFileAccess:false,saveSession:false,imageResponses:'allow',outputDir:settings.output,timeouts:{action:5000,navigation:10000,idle:0}},()=>context);
 await server.connect(new external.StdioServerTransport());
 let stopping;const stop=()=>stopping||=(async()=>{const deadline=setTimeout(()=>process.exit(1),5000);await server.close().catch(()=>{});await browser.close();await proxy.close();clearTimeout(deadline);process.exitCode=0;})();
 process.stdin.once('end',()=>void stop());for(const signal of ['SIGTERM','SIGINT'])process.once(signal,()=>void stop());
})().catch(()=>{process.stderr.write('Controlled download experiment failed\n');process.exitCode=1;});
`;
function children(pid) {
  const ps=spawnSync('ps',['-axo','pid=,ppid='],{encoding:'utf8'});assert.equal(ps.status,0);
  const pairs=ps.stdout.trim().split('\n').map(line=>line.trim().split(/\s+/).map(Number));
  const found=new Set([pid]);for(let n=0;n<pairs.length;n++){
    const size=found.size;for(const [child,parent]of pairs)if(found.has(parent))found.add(child);
    if(found.size===size)break;
  }return [...found];
}
function alive(pid){try{process.kill(pid,0);return true;}catch(error){if(error.code==='ESRCH')return false;throw error;}}
async function gone(pids){const deadline=Date.now()+10000;while(pids.some(alive)){
  assert.ok(Date.now()<deadline,'owned captured process did not exit');await new Promise(resolve=>setTimeout(resolve,25));
}}
function listFiles(dir){return fs.readdirSync(dir,{recursive:true}).map(name=>path.join(dir,name)).filter(name=>fs.statSync(name).isFile());}
async function listen(server){server.listen(0,'127.0.0.1');await once(server,'listening');return server.address().port;}
async function characterize(root,reportPath){
  assert.equal(process.platform,'darwin','this measured characterization currently requires the recorded macOS host');
  const external=loadBackend(root);assert.ok(path.isAbsolute(reportPath));assert.ok(!fs.existsSync(reportPath),'report must be a fresh path');
  const scratch=fs.mkdtempSync(path.join(os.tmpdir(),'atomcode-download-characterization-'));fs.chmodSync(scratch,0o700);
  const report={passed:false,purpose:'reject cancellation-only download authorization',artifact_root:scratch,
    production_policy_changed:false,full_matrix_completed:false,node:process.version,platform:process.platform,
    dependency_hashes:external.sourceHashes,verifier_sha256:crypto.createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'),phases:[]};
  const key=path.join(scratch,'key.pem'),cert=path.join(scratch,'cert.pem');
  assert.equal(spawnSync('openssl',['req','-x509','-newkey','rsa:2048','-nodes','-keyout',key,'-out',cert,'-days','1','-subj','/CN=localhost'],{stdio:'ignore'}).status,0);
  const tls={key:fs.readFileSync(key),cert:fs.readFileSync(cert)};const x509=new crypto.X509Certificate(tls.cert);
  const pin=crypto.createHash('sha256').update(x509.publicKey.export({type:'spki',format:'der'})).digest('base64');
  const requests=[];const allowedBytes=Buffer.from('ALLOWED_DOWNLOAD_FIXTURE'),deniedBytes=Buffer.from('DENIED_DOWNLOAD_FIXTURE');
  const denied=https.createServer(tls,(req,res)=>{requests.push('denied:'+req.url);res.writeHead(200,{'Content-Disposition':'attachment; filename="fixture-denied.bin"','Content-Type':'application/octet-stream'});res.end(deniedBytes);});
  const bad='https://127.0.0.1:'+await listen(denied);
  const allowed=https.createServer(tls,(req,res)=>{requests.push('allowed:'+req.url);if(req.url==='/redirect'){res.writeHead(302,{Location:bad+'/redirected'});res.end();return;}res.writeHead(200,{'Content-Disposition':'attachment; filename="fixture-allowed.bin"','Content-Type':'application/octet-stream'});res.end(allowedBytes);});
  const good='https://127.0.0.1:'+await listen(allowed);const requireBackend=createRequire(path.join(root,'package.json'));
  const {Client,StdioClientTransport}=requireBackend('playwright-core/lib/utilsBundle');
  try{
    for(const delay of [false,true]){
      const label=delay?'delayed':'ordinary';const dir=path.join(scratch,label);fs.mkdirSync(dir,{mode:0o700});const output=path.join(dir,'files');fs.mkdirSync(output,{mode:0o700});
      const settings={moduleRoot:__dirname,backendRoot:root,pin,delay,accessOrigins:[good,bad],downloadOrigin:good,output,events:path.join(dir,'events.jsonl'),metadata:path.join(dir,'metadata.json')};
      const config=path.join(dir,'settings.json'),backend=path.join(dir,'test-only-backend.cjs');fs.writeFileSync(config,JSON.stringify(settings));fs.writeFileSync(backend,CHILD_SOURCE);
      let stderr='';const transport=new StdioClientTransport({command:process.execPath,args:[backend,config],stderr:'pipe'});transport.stderr?.on('data',data=>stderr+=data.toString());
      const client=new Client({name:'download-authorization-characterization',version:'1'},{capabilities:{}});const phase={label,cases:[],captured_pids:[]};report.phases.push(phase);
      try{
        await client.connect(transport);phase.captured_pids=children(transport.pid);assert.ok(phase.captured_pids.length>1);
        const cases=delay?[['delayed-denied',bad+'/delayed']]:[['denied',bad+'/one'],['allowed',good+'/one'],['redirect-denied',good+'/redirect'],...Array.from({length:5},(_,n)=>['fast-denied-'+n,bad+'/fast-'+n])];
        for(const [name,url]of cases){
          const result=await client.callTool({name:'browser_navigate',arguments:{url}});await new Promise(resolve=>setTimeout(resolve,250));
          const files=listFiles(output),permitted=files.filter(file=>fs.readFileSync(file).equals(allowedBytes)),forbidden=files.filter(file=>fs.readFileSync(file).equals(deniedBytes));
          const observed={name,tool_error:Boolean(result.isError),allowed_payloads:permitted.length,denied_payloads:forbidden.length,
            denied_artifacts:forbidden.map(file=>({file:path.relative(scratch,file),sha256:crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')}))};phase.cases.push(observed);
          if(!delay)assert.equal(forbidden.length,0,'ordinary samples should reproduce the misleading positive result');
          if(name==='allowed')assert.equal(permitted.length,1);
          if(delay)assert.equal(forbidden.length,1,'delayed completed artifact must disprove cancellation-only authorization');
        }
        phase.events=fs.readFileSync(settings.events,'utf8').trim().split('\n').map(JSON.parse);phase.runtime=JSON.parse(fs.readFileSync(settings.metadata));
        assert.equal(phase.events.filter(event=>event.kind==='cancelled').length,delay?1:7);
      }finally{
        await client.close().catch(()=>{});await gone(phase.captured_pids);phase.remaining_pids=[];phase.stderr_bytes=Buffer.byteLength(stderr);fs.writeFileSync(path.join(dir,'stderr.log'),stderr);
      }
    }
    report.requests=requests;report.passed=true;
  }finally{
    denied.closeAllConnections();allowed.closeAllConnections();await Promise.all([new Promise(resolve=>denied.close(resolve)),new Promise(resolve=>allowed.close(resolve))]);
    report.completed_at=new Date().toISOString();fs.mkdirSync(path.dirname(reportPath),{recursive:true});fs.writeFileSync(reportPath,JSON.stringify(report,null,2)+'\n',{flag:'wx',mode:0o600});
  }
  console.log('Characterization confirmed: ordinary samples hide a completed-download race; delayed cancel resolves but one denied payload is saved.');
}
if(require.main===module){const argv=process.argv.slice(2);assert.equal(argv.length,4,'--backend-root <installed pinned package> --report <new absolute report>');assert.equal(argv[0],'--backend-root');assert.equal(argv[2],'--report');characterize(argv[1],argv[3]).catch(()=>{console.error('Controlled download characterization failed; inspect the owned report/fixture.');process.exitCode=1;});}
