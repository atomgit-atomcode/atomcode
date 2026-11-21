import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, symlinkSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createDispatch, redact } from './adapter.mjs';

const definitions = [
  'huaweicloud_auth_init', 'huaweicloud_sandbox_credentials', 'huaweicloud_run_approved_command',
  'huaweicloud_sandbox_connect', 'huaweicloud_sandbox_upload_project', 'huaweicloud_service_catalog',
].map(name => ({ name, inputSchema: { type: 'object' } }));
test('catalog excludes credential and unrestricted cloud write tools and preserves approvals', async () => {
  let called = false;
  const dispatch = createDispatch(async () => ({ TOOL_DEFINITIONS: definitions, callTool: () => { called = true; } }));
  const result = await dispatch('tools/list');
  assert.deepEqual(result.tools.map(tool => tool.name), ['huaweicloud_sandbox_connect', 'huaweicloud_sandbox_upload_project', 'huaweicloud_service_catalog', 'huaweicloud_sandbox_preview']);
  assert.equal(result.tools[0].annotations.readOnlyHint, false);
  assert.equal(result.tools[2].annotations.readOnlyHint, true);
  for (const name of ['huaweicloud_auth_init', 'huaweicloud_sandbox_credentials', 'huaweicloud_run_approved_command']) {
    await assert.rejects(dispatch('tools/call', { name }), /not available/);
  }
  assert.equal(called, false);
});
test('upload excludes private local files and errors propagate with secrets redacted', async () => {
  let args;
  const dispatch = createDispatch(async () => ({ callTool: (_name, input) => { args = input; return { ok: false, credentials: { ak: 'private' }, message: 'SK=private' }; } }));
  const result = await dispatch('tools/call', { name: 'huaweicloud_sandbox_upload_project', arguments: { local_dir: '/fixture', workspace_id: 'fixture', exclude: ['dist'] } });
  assert.ok(args.exclude.includes('.env')); assert.ok(args.exclude.includes('.git')); assert.ok(args.exclude.includes('dist'));
  assert.equal(result.isError, true); assert.ok(!result.content[0].text.includes('private'));
  const failed = createDispatch(async () => ({ callTool() { throw new Error('password=private'); } }));
  const error = await failed('tools/call', { name: 'huaweicloud_sandbox_connect' });
  assert.equal(error.isError, true); assert.ok(!error.content[0].text.includes('private'));
  assert.equal(redact({ credentialsConfigured: true }).credentialsConfigured, true);
});
test('real upstream catalog and sandbox guidance load without cloud requests', async () => {
  const dispatch = createDispatch();
  const catalog = await dispatch('tools/list');
  assert.ok(catalog.tools.length > 10);
  assert.ok(!catalog.tools.some(tool => tool.name === 'huaweicloud_sandbox_credentials'));
  const skill = await dispatch('tools/call', { name: 'huaweicloud_retrieve_skill', arguments: { name: 'huawei-sandbox' } });
  assert.equal(skill.isError, false); assert.match(skill.content[0].text, /Sandbox/);
});
test('stdio JSONL and fragmented Content-Length requests receive valid responses', async () => {
  for (const framed of [false, true]) {
    const child = spawn(process.execPath, [new URL('./adapter.mjs', import.meta.url).pathname], { stdio: ['pipe', 'pipe', 'pipe'] });
    const chunks = []; const errors = [];
    child.stdout.on('data', data => chunks.push(data)); child.stderr.on('data', data => errors.push(data));
    const message = JSON.stringify({ jsonrpc: '2.0', id: 7, method: 'initialize', params: { protocolVersion: '2024-11-05' } });
    const payload = framed ? `Content-Length: ${Buffer.byteLength(message)}\r\n\r\n${message}` : message + '\n';
    child.stdin.write(payload.slice(0, 4));
    const list = JSON.stringify({ jsonrpc: '2.0', id: 8, method: 'tools/list', params: {} });
    child.stdin.end(payload.slice(4) + (framed ? `Content-Length: ${Buffer.byteLength(list)}\r\n\r\n${list}` : list + '\n'));
    const code = await new Promise((resolve, reject) => { child.on('error', reject); child.on('exit', resolve); });
    assert.equal(code, 0, Buffer.concat(errors).toString());
    const raw = Buffer.concat(chunks).toString();
    const replies = [];
    if (framed) {
      let rest = raw;
      while (rest.length) {
        const end = rest.indexOf('\r\n\r\n');
        const length = Number(/Content-Length: (\d+)/.exec(rest.slice(0, end))[1]);
        replies.push(JSON.parse(rest.slice(end + 4, end + 4 + length)));
        rest = rest.slice(end + 4 + length);
      }
    } else replies.push(...raw.trim().split('\n').map(line => JSON.parse(line)));
    const reply = replies.find(reply => reply.id === 7);
    assert.equal(reply.result.serverInfo.name, 'atomcode-huaweicloud');
    assert.ok(replies.find(reply => reply.id === 8)?.result.tools.length > 10);
  }
});

test('entry point starts when launched through a symlink', { skip: process.platform === 'win32' }, () => {
  const dir = mkdtempSync(join(tmpdir(), 'cloud-symlink-'));
  try {
    const link = join(dir, 'server.mjs');
    symlinkSync(new URL('./adapter.mjs', import.meta.url), link);
    const result = spawnSync(process.execPath, [link], { encoding: 'utf8', input: JSON.stringify({jsonrpc:'2.0',id:1,method:'initialize'})+'\n' });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(JSON.parse(result.stdout).id, 1);
  } finally { rmSync(dir, {recursive:true,force:true}); }
});

import { createPreview } from './preview.mjs';
test('preview keeps authentication internal and returns only a verified URL', async () => {
  let command;
  const preview = createPreview(async () => ({
    credentials: () => ({ak:'private-ak',sk:"private'sk"}),
    execute: async (_workspace, input) => { command=input; return {exitCode:0,stdout:'echo private-ak\nTunnel URL: https://demo-8080.cn-north-4-bridge.myhuaweicloud.com\n'}; },
    verify: async () => ({status:200}),
  }));
  const result = await preview({workspace_id:'fixture',port:8080});
  assert.equal(result.ok,true); assert.match(result.url,/^https:/);
  assert.ok(!JSON.stringify(result).includes('private'));
  assert.ok(command.includes('HISTFILE=/dev/null'));
  assert.ok(command.includes("'private'\"'\"'sk'"));
  assert.equal((await preview({workspace_id:'fixture',port:'8080; rm -rf /'})).ok,false);
  const unsupported = createPreview(async () => ({ credentials:()=>({ak:'a',sk:'s',securityToken:'t'}), execute:()=>assert.fail('must not execute') }));
  assert.equal((await unsupported({workspace_id:'fixture',port:80})).ok,false);
  const failed = createPreview(async () => ({ credentials:()=>({ak:'a',sk:'s'}), execute:()=>{throw Error('secret echo');} }));
  assert.ok(!JSON.stringify(await failed({workspace_id:'fixture',port:80})).includes('secret echo'));
});

test('remote execution requires its own workspace rather than shared connection state', async () => {
  let called = false;
  const dispatch = createDispatch(async () => ({callTool(){called=true;}}));
  const result = await dispatch('tools/call', {name:'huaweicloud_sandbox_exec_one_shot',arguments:{command:'pwd'}});
  assert.equal(result.isError,true); assert.equal(called,false);
});

test('configured credential values never return through terminal output or errors', async () => {
  const dispatch = createDispatch(async () => ({
    privateSecrets:()=>['local-secret-value'],
    callTool:()=>({stdout:'raw local-secret-value without a field name'}),
  }));
  const result = await dispatch('tools/call',{name:'huaweicloud_sandbox_exec_one_shot',arguments:{workspace_id:'fixture',command:'pwd'}});
  assert.ok(!result.content[0].text.includes('local-secret-value'));
});

test('a remote nonzero exit is a tool failure rather than a successful deployment', async () => {
  const dispatch = createDispatch(async () => ({callTool:()=>({exitCode:1,stdout:'',stderr:'build failed'})}));
  const result = await dispatch('tools/call',{name:'huaweicloud_sandbox_exec_one_shot',arguments:{workspace_id:'fixture',command:'build'}});
  assert.equal(result.isError,true);
});

test('real readonly CLI works with a local profile and no Devkit credentials', {
  skip: process.platform === 'win32',
}, () => {
  const dir = mkdtempSync(join(tmpdir(), 'cloud-profile-'));
  try {
    const preload = join(dir, 'home.mjs');
    writeFileSync(preload, "import os from 'node:os'; import {syncBuiltinESMExports} from 'node:module'; os.homedir=()=>process.env.HUAWEICLOUD_HOME; syncBuiltinESMExports();\n");
    const hcloud = join(dir, 'hcloud');
    writeFileSync(hcloud, '#!/usr/bin/env node\nconsole.log(JSON.stringify({servers:[]}));\n', {mode:0o700});
    const request = {jsonrpc:'2.0',id:1,method:'tools/call',params:{name:'huaweicloud_run_readonly_command',arguments:{args:['ECS','ListServersDetails']}}};
    const result = spawnSync(process.execPath, ['--import', preload, new URL('./adapter.mjs', import.meta.url).pathname], {
      encoding:'utf8',cwd:dir,timeout:20000,input:JSON.stringify(request)+'\n',
      env:{...process.env,HUAWEICLOUD_HOME:dir,HW_ACCESS_KEY:'',HW_SECRET_KEY:'',HW_SECURITY_TOKEN:'',HCLOUD_BIN:hcloud},
    });
    assert.equal(result.status,0,result.stderr);
    const reply=JSON.parse(result.stdout);
    assert.equal(reply.result.isError,false,JSON.stringify(reply));
    assert.match(reply.result.content[0].text,/servers/);
  } finally { rmSync(dir,{recursive:true,force:true}); }
});
