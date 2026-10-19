import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as os from 'node:os';
import * as path from 'node:path';
import * as http from 'node:http';
import { parseQuery, resolvePosition, runQuery, Document, EditorHost, Query } from '../../src/languageBridge/query';
import { startBridge } from '../../src/languageBridge/server';

const q = (patch: Partial<Query> = {}): Query => ({ operation: 'hover', file_path: 'a.cj', workspace_root: '/workspace', line: 1, symbol: '文档', ...patch });
assert.deepEqual(resolvePosition(['😀 文档'], q()), { line: 0, character: 3 });
assert.throws(() => resolvePosition(['文档 + 文档'], q()), /occurs 2/);
assert.deepEqual(resolvePosition(['文档 + 文档'], q({ occurrence: 2 })), { line: 0, character: 5 });
assert.throws(() => resolvePosition(['😀'], q({ symbol: undefined, character: 2 })), /surrogate/);
assert.throws(() => resolvePosition(['a'], q({ line: 2 })), /Line/);
assert.throws(() => parseQuery(q({ operation: 'rename' as Query['operation'] })), /Operation/);
assert.throws(() => parseQuery(q({ character: 1 })), /not both/);
assert.throws(() => parseQuery(q({ timeout_secs: 121 })), /120/);
assert.throws(() => parseQuery(q({ occurrence: 0 })), /positive/);

async function main() {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), 'atomcode-ide-test-'));
  const file = path.join(directory, 'a.cj');
  await fs.writeFile(file, 'disk content');
  let version = 7;
  const document: Document = { uri: `file://${file}`, languageId: 'cangjie', version: () => version, dirty: () => true, lines: () => ['😀 文档'] };
  const calls: string[] = [];
  const host: EditorHost = { open: async () => document, query: async (operation, _doc, position) => {
    calls.push(operation);
    if (position) assert.deepEqual(position, { line: 0, character: 3 });
    return [{ text: 'semantic result' }];
  } };
  const inspect = await runQuery(host, file, q({ operation: 'inspect' }));
  assert.deepEqual(calls, ['hover', 'definition', 'references']);
  assert.equal(inspect.document.dirty, true);
  assert.equal(inspect.document.version, 7);
  assert.equal(inspect.capabilities.hover, 'unknown');
  assert.equal(inspect.capabilities.diagnostics, 'unknown');
  assert.equal(inspect.diagnostics_collection_available, true);
  assert.equal(inspect.diagnostics_freshness, 'editor_snapshot_unverified');
  await assert.rejects(runQuery({ ...host, query: async () => { version++; return []; } }, file, q()), /changed/);
  const empty = await runQuery({ ...host, query: async () => [] }, file, q());
  assert.equal(empty.results[0].status, 'empty');
  const failed = await runQuery({ ...host, query: async () => { throw new Error('private provider detail'); } }, file, q());
  assert.equal(failed.results[0].status, 'provider_error');
  assert.ok(!JSON.stringify(failed).includes('private provider detail'));
  const bounded = await runQuery({ ...host, query: async () => Array.from({ length: 205 }, () => ({ item: 1 })) }, file, q());
  assert.equal(bounded.results[0].items.length, 200);
  assert.equal(bounded.results[0].omitted_at_least, 5);

  const bridge = await startBridge(path.join(directory, 'private'), [directory], host, 'test IDE');
  const binding = JSON.parse(await fs.readFile(bridge.connectionFile, 'utf8'));
  if (process.platform !== 'win32') assert.equal((await fs.stat(bridge.connectionFile)).mode & 0o777, 0o600);
  const send = (payload: unknown, token = binding.token, origin?: string, endpoint = binding.endpoint): Promise<{ code: number; body: Record<string, unknown> }> => new Promise((resolve, reject) => {
    const req = http.request(endpoint, { method: 'POST', headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json', ...(origin ? { origin } : {}) } }, res => {
      const chunks: Buffer[] = [];
      res.on('data', chunk => chunks.push(chunk));
      res.on('end', () => resolve({ code: res.statusCode!, body: JSON.parse(Buffer.concat(chunks).toString()) }));
    });
    req.on('error', reject);
    req.end(JSON.stringify(payload));
  });
  try {
    assert.equal((await send(q(), 'wrong')).code, 401);
    assert.equal((await send(q(), binding.token, 'http://evil.example')).code, 401);
    const result = await send(q({ file_path: file, workspace_root: directory, operation: 'inspect' }));
    assert.equal(result.code, 200);
    assert.equal(result.body.status, 'ok');
    assert.equal(result.body.window_id, binding.window_id);
    assert.equal(result.body.source, 'ide');
    assert.ok(!JSON.stringify(result).includes(binding.token));
    const outside = await fs.mkdtemp(path.join(os.tmpdir(), 'atomcode-ide-outside-'));
    try { assert.equal((await send(q({ file_path: file, workspace_root: outside }))).body.status, 'wrong_workspace'); }
    finally { await fs.rm(outside, { recursive: true, force: true }); }
    assert.equal((await send(q({ operation: 'rename' as Query['operation'] }))).body.status, 'invalid_request');
    assert.equal((await send(q({ symbol: 'x'.repeat(17000) }))).code, 413);
    const queue: Array<(value: unknown[]) => void> = [];
    let entered: (() => void) | undefined;
    const slow = await startBridge(path.join(directory, 'private'), [directory], {
      ...host, query: async () => new Promise<unknown[]>(resolve => { queue.push(resolve); entered?.(); }),
    }, 'slow fixture');
    const slowBinding = JSON.parse(await fs.readFile(slow.connectionFile, 'utf8'));
    assert.notEqual(slowBinding.window_id, binding.window_id);
    try {
      const request = q({ file_path: file, workspace_root: directory, timeout_secs: 1 });
      assert.equal((await send(request, slowBinding.token, undefined, slowBinding.endpoint)).body.status, 'timeout');
      // A timed-out provider still owns a slot. Aborting three more clients must
      // not free provider slots and allow an unbounded number of IDE commands.
      for (let i = 0; i < 3; i++) {
        const ready = new Promise<void>(resolve => { entered = resolve; });
        const req = http.request(slowBinding.endpoint, { method: 'POST', headers: { authorization: `Bearer ${slowBinding.token}` } }, res => res.resume());
        req.on('error', () => {});
        req.end(JSON.stringify(request));
        await ready;
        req.destroy();
      }
      entered = undefined;
      assert.equal(queue.length, 4);
      assert.equal((await send(request, slowBinding.token, undefined, slowBinding.endpoint)).body.status, 'not_ready');
    } finally {
      for (const finish of queue) finish([]);
      await slow.dispose();
    }

  } finally {
    await bridge.dispose();
    await assert.rejects(fs.stat(bridge.connectionFile));
    await fs.rm(directory, { recursive: true, force: true });
  }
}
void main().catch(error => { console.error(error); process.exitCode = 1; });
