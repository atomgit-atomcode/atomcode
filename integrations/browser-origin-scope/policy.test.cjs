'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const net = require('node:net');
const { once } = require('node:events');
const { parsePolicy, permittedUrl, allowedCall, createProxy, PolicyTransport, TOOLS } = require('./policy.cjs');

async function listen(server) { server.listen(0, '127.0.0.1'); await once(server, 'listening'); return server.address().port; }
function close(server) { server.closeAllConnections?.(); return new Promise(resolve => server.close(resolve)); }
function request(proxy, url) {
  return new Promise((resolve, reject) => {
    http.get({ hostname: '127.0.0.1', port: new URL(proxy).port, path: url, agent: false }, res => {
      let body = ''; res.on('data', data => body += data); res.on('end', () => resolve({ status: res.statusCode, body }));
    }).on('error', reject);
  });
}
function connect(proxy, authority) {
  return new Promise((resolve, reject) => {
    const client = net.connect(Number(new URL(proxy).port), '127.0.0.1');
    client.on('error', reject);
    client.once('connect', () => client.write(`CONNECT ${authority} HTTP/1.1\r\nHost: ${authority}\r\n\r\n`));
    client.once('data', data => resolve({ client, response: data.toString() }));
  });
}

test('policy is explicit, bounded and exact; unsupported permissions fail closed', () => {
  for (const raw of [undefined, '', 'null', '[]', '{}', '{', JSON.stringify({version:2,origins:[]}),
    JSON.stringify({version:1,origins:[],uploads:'allow'}), JSON.stringify({version:1,origins:'*'}),
    JSON.stringify({version:1,origins:Array(257).fill('https://example.test')}), ' '.repeat(65537)])
    assert.throws(() => parsePolicy(raw));
  for (const origin of ['*','https://*.example.test','file:///tmp','http://a.test/path','http://a.test/?q=x',
    'http://a.test/#fragment','http://a.test/?','http://a.test/#','http://@a.test','http:////a.test','http://user:password@a.test',' http://a.test','http://a.test\n','http:\\a.test'])
    assert.throws(() => parsePolicy(JSON.stringify({version:1,origins:[origin]})), origin);
  const origins = parsePolicy(JSON.stringify({version:1,origins:['https://EXAMPLE.test:443/','https://example.test']}));
  assert.deepEqual([...origins], ['https://example.test']);
  assert.ok(permittedUrl('https://example.test/path?query=1', origins));
  for (const url of ['http://example.test', 'https://example.test.evil', 'https://example.test:444',
    'https://example.test@evil.test', 'javascript:alert(1)', 'data:text/html,hi', 'file:///tmp/x'])
    assert.equal(permittedUrl(url, origins), null, url);
  assert.equal(parsePolicy('{"version":1,"origins":[]}').size, 0);
});

test('direct tools, navigation schemes and caller file/meta overrides are gated', () => {
  const origins = new Set(['https://example.test']);
  for (const name of ['browser_evaluate','browser_run_code','browser_file_upload','browser_route','browser_cdp','unknown'])
    assert.equal(allowedCall({name,arguments:{}}, origins), false);
  for (const args of [null, [], true, 1, 'invalid'])
    assert.equal(allowedCall({name:'browser_close',arguments:args}, origins), false);
  assert.equal(allowedCall({name:'browser_navigate',arguments:{url:'https://example.test/a'}}, origins), true);
  assert.equal(allowedCall({name:'browser_navigate',arguments:{url:'https://denied.test'}}, origins), false);
  for (const name of TOOLS) {
    assert.equal(allowedCall({name,arguments:{filename:'anything.png'}}, origins), false);
    assert.equal(allowedCall({name,arguments:{_meta:{cwd:'/tmp'}}}, origins), false);
  }
});

test('transport filters discovery independently of SDK request IDs and refuses direct dispatch', async () => {
  const sent = [], delivered = [];
  const inner = { async start() {}, async send(message) { sent.push(message); }, async close() {} };
  let closed = 0;
  const gate = new PolicyTransport(inner, new Set(['https://example.test']), async () => { closed++; });
  gate.onmessage = message => delivered.push(message);
  await gate.start();
  inner.onmessage({jsonrpc:'2.0',id:1,method:'tools/list'});
  await gate.send({jsonrpc:'2.0',id:1,method:'roots/list'}); // Opposite-direction ID collision.
  await gate.send({jsonrpc:'2.0',id:1,result:{tools:[{name:'browser_snapshot'},{name:'browser_evaluate'}]}});
  assert.deepEqual(sent[1].result.tools, [{name:'browser_snapshot'}]);
  inner.onmessage({jsonrpc:'2.0',id:2,method:'tools/call',params:{name:'browser_evaluate',arguments:{function:'() => 1'}}});
  assert.equal(delivered.length, 1);
  assert.equal(sent[2].error.code, -32602);
  inner.onmessage({jsonrpc:'2.0',id:3,method:'tools/call',params:{name:'browser_close'}});
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(closed, 1);
  assert.equal(sent[3].result.content[0].type, 'text');
  assert.equal(delivered.length, 1);
});

test('HTTP proxy forwards allowed targets only and never follows redirects itself', async () => {
  let allowed = 0, denied = 0;
  const target = http.createServer((req,res) => { allowed++; if (req.url === '/attachment') res.setHeader('Content-Disposition','attachment; filename=x.txt'); res.end('allowed'); });
  const forbidden = http.createServer((_req,res) => { denied++; res.end('forbidden'); });
  const port = await listen(target), deniedPort = await listen(forbidden);
  const origin = `http://127.0.0.1:${port}`;
  const proxy = await createProxy(new Set([origin]));
  try {
    assert.deepEqual(await request(proxy.url, origin + '/ok'), {status:200,body:'allowed'});
    for (const url of [`http://127.0.0.1:${deniedPort}/no`, `http://localhost:${port}/no`, origin.replace('http:', 'https:')])
      assert.equal((await request(proxy.url, url)).status, 403);
    assert.equal((await request(proxy.url, origin + '/attachment')).status, 403);
    assert.equal(allowed, 2); assert.equal(denied, 0);
  } finally { await proxy.close(); await close(target); await close(forbidden); }
});

test('HTTPS CONNECT validates the exact origin before opening any upstream TCP connection', async () => {
  let accepted = 0, forbidden = 0;
  const target = net.createServer(socket => { accepted++; socket.on('error', () => {}); socket.on('data', data => socket.write(data)); });
  const denied = net.createServer(socket => { forbidden++; socket.destroy(); });
  const port = await listen(target), deniedPort = await listen(denied);
  const proxy = await createProxy(new Set([`https://127.0.0.1:${port}`]));
  try {
    for (const authority of [`127.0.0.1:${deniedPort}`, `localhost:${port}`, `user@127.0.0.1:${port}`, `127.0.0.1:${port}/bad`]) {
      const result = await connect(proxy.url, authority); assert.match(result.response,/403/); result.client.destroy();
    }
    const result = await connect(proxy.url, `127.0.0.1:${port}`);
    assert.match(result.response,/200 Connection Established/);
    const bytes = Buffer.from([0x16,0x03,0x03,0x00,0x01,0x00]);
    const echo = once(result.client, 'data'); result.client.write(bytes);
    assert.deepEqual((await echo)[0], bytes);
    result.client.destroy();
    assert.equal(accepted, 1); assert.equal(forbidden, 0);
  } finally { await proxy.close(); await close(target); await close(denied); }
});

test('owned browser closes its proxy, serializes replacement after cleanup and refuses permanent-close restart', async () => {
  const { ownedBrowser } = require('./backend.cjs');
  let launches = 0, release;
  const firstClose = new Promise(resolve => { release=resolve; });
  const proxies = [];
  const owner = ownedBrowser({async launch(options) {
    launches++; const index=launches; proxies.push(options.proxy.server);
    assert.equal(options.proxy.bypass, '<-loopback>');
    assert.ok(options.args.includes('--disable-quic'));
    return { async newContext(options) { assert.deepEqual(options,{acceptDownloads:false,serviceWorkers:'block',permissions:[]}); return {index}; },
      async close() { if (index===1) await firstClose; } };
  }}, new Set(), false);
  assert.equal((await owner.get()).index,1);
  const closing=owner.close(); const next=owner.get();
  await new Promise(resolve=>setImmediate(resolve)); assert.equal(launches,1);
  release(); await closing; assert.equal((await next).index,2);
  await owner.close(true);
  await assert.rejects(owner.get(),/closing/);
  for (const proxy of proxies) await assert.rejects(request(proxy,'http://not-allowed.test'),/ECONNREFUSED/);
});

test('failed launch closes its proxy before returning an error', async () => {
  const { ownedBrowser } = require('./backend.cjs');
  let proxy;
  const owner=ownedBrowser({async launch(options) { proxy=options.proxy.server; throw new Error('fixture failed launch'); }},new Set(),false);
  await assert.rejects(owner.get(),/fixture failed launch/);
  await assert.rejects(request(proxy,'http://not-allowed.test'),/ECONNREFUSED/);
  await owner.close(true);
});

test('failed explicit browser close has an RPC error terminal rather than disappearing', async () => {
  const sent=[];
  const inner={async start(){},async send(message){sent.push(message);},async close(){}};
  const gate=new PolicyTransport(inner,new Set(),async()=>{throw new Error('fixture cleanup failed');});
  await gate.start();
  inner.onmessage({jsonrpc:'2.0',id:9,method:'tools/call',params:{name:'browser_close'}});
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(sent[0].id,9); assert.equal(sent[0].error.code,-32603);
  assert.ok(!JSON.stringify(sent).includes('fixture cleanup failed'));
});

test('a CONNECT permission cannot downgrade HTTPS to plain HTTP or upgrade HTTP to TLS', async () => {
  let connections=0;
  const target=net.createServer(socket=>{connections++;socket.destroy();});
  const port=await listen(target);
  for (const scheme of ['https','http']) {
    const proxy=await createProxy(new Set([`${scheme}://127.0.0.1:${port}`]));
    try {
      const {client,response}=await connect(proxy.url,`127.0.0.1:${port}`);
      assert.match(response,/200/);
      const closed=once(client,'close');
      if (scheme==='https') client.write('GET / HTTP/1.1\r\nHost: other.test\r\n\r\n');
      else client.write(Buffer.from([0x16,0x03,0x03,0x00,0x01,0x00]));
      await closed;
      assert.equal(connections,0);
      assert.equal(proxy.stats.connectDenied,1);
    } finally { await proxy.close(); }
  }
  await close(target);
});

test('permanent close fences an accepted but not-yet-started launch', async () => {
  const {ownedBrowser}=require('./backend.cjs');
  let launched=0;
  const owner=ownedBrowser({async launch(){launched++;throw new Error('must not start');}},new Set(),false);
  const opening=owner.get(); const closing=owner.close(true);
  await assert.rejects(opening,/closing/); await closing;
  assert.equal(launched,0);
});

test('tool-call notifications cannot close a browser without a request terminal', async () => {
  let closed=0, delivered=0;
  const inner={async start(){},async send(){throw new Error('must not reply to notification');},async close(){}};
  const gate=new PolicyTransport(inner,new Set(),async()=>{closed++;});
  gate.onmessage=()=>{delivered++;}; await gate.start();
  inner.onmessage({jsonrpc:'2.0',method:'tools/call',params:{name:'browser_close'}});
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(closed,0);assert.equal(delivered,0);
});

test('artifact ownership refuses foreign contents before chmod or retention and protects leaf symlinks', () => {
  const fs=require('node:fs'), os=require('node:os'), path=require('node:path');
  const {prepareOutput}=require('./backend.cjs');
  const temp=fs.mkdtempSync(path.join(os.tmpdir(),'atomcode-origin-artifacts-'));
  try {
    assert.throws(()=>prepareOutput(path.parse(temp).root));
    const foreign=path.join(temp,'foreign');fs.mkdirSync(foreign,{mode:0o750});
    fs.writeFileSync(path.join(foreign,'keep.txt'),'keep');
    const mode=fs.statSync(foreign).mode;
    assert.throws(()=>prepareOutput(foreign),/nonempty/);
    assert.equal(fs.statSync(foreign).mode,mode);
    assert.deepEqual(fs.readdirSync(foreign),['keep.txt']);
    assert.equal(fs.readFileSync(path.join(foreign,'keep.txt'),'utf8'),'keep');
    const root=path.join(temp,'owned');
    const files=prepareOutput(root);assert.equal(files,path.join(root,'files'));
    fs.writeFileSync(path.join(files,'owned.txt'),'owned');
    assert.equal(prepareOutput(root),files);assert.equal(fs.readFileSync(path.join(files,'owned.txt'),'utf8'),'owned');
    if (process.platform!=='win32') {
      const linked=path.join(temp,'linked');fs.symlinkSync(root,linked);
      assert.throws(()=>prepareOutput(linked),/real directory/);
      fs.renameSync(files,path.join(root,'saved-files'));fs.symlinkSync(foreign,files);
      assert.throws(()=>prepareOutput(root),/real directory/);
      assert.equal(fs.readFileSync(path.join(foreign,'keep.txt'),'utf8'),'keep');
    }
  } finally {fs.rmSync(temp,{recursive:true,force:true});}
});
