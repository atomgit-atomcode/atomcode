// Optional local install smoke test. Downloads pinned npm deps; never calls cloud APIs.
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';

if (!process.argv[2]) throw new Error('Usage: node scripts/huaweicloud-smoke.mjs /path/to/atomcode');
const binary = resolve(process.argv[2]);
const root = mkdtempSync(join(tmpdir(), 'atomcode-huaweicloud-smoke-'));
const env = { ...process.env, ATOMCODE_HOME: root };
function run(args) {
  const result = spawnSync(binary, ['cloud', ...args], { env, encoding: 'utf8', timeout: 180000 });
  assert.equal(result.status, 0, `${args[0]} failed: ${result.error?.message || result.stderr}`);
  console.log(`${args[0]}: OK`);
}
try {
  writeFileSync(join(root, 'mcp.json'), JSON.stringify({ mcpServers: { other: { command: 'other', env: { EXAMPLE: 'preserved' } } } }));
  run(['status']);
  assert.equal(existsSync(join(root, 'integrations')), false, 'status must not install or activate');
  run(['enable']); run(['doctor']);
  const read = () => JSON.parse(readFileSync(join(root, 'mcp.json'), 'utf8'));
  const server = read().mcpServers.huaweicloud;
  assert.equal(server.trust, false); assert.deepEqual(server.autoApprove, []); assert.equal(server.timeout_ms, 300000);
  assert.equal(read().mcpServers.other.env.EXAMPLE, 'preserved');
  const requests = [
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
    { jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} },
    { jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'huaweicloud_retrieve_skill', arguments: { name: 'huawei-sandbox' } } },
  ].map(value => JSON.stringify(value)).join('\n') + '\n';
  const result = spawnSync(server.command, server.args, { env, input: requests, encoding: 'utf8', timeout: 20000 });
  assert.equal(result.status, 0, result.error?.message || result.stderr);
  const replies = result.stdout.trim().split('\n').map(line => JSON.parse(line));
  const tools = replies.find(reply => reply.id === 2)?.result?.tools;
  assert.ok(tools?.length > 10, 'installed server must return its tool catalog');
  assert.ok(!tools.some(tool => ['huaweicloud_auth_init', 'huaweicloud_sandbox_credentials', 'huaweicloud_run_approved_command'].includes(tool.name)));
  assert.equal(replies.find(reply => reply.id === 3)?.result?.isError, false);
  console.log(`installed MCP: ${tools.length} tools; sandbox guidance loads`);
  run(['disable']);
  assert.equal(read().mcpServers.huaweicloud.disabled, true);
  assert.equal(existsSync(join(root, 'skills/huaweicloud-preview/SKILL.md')), false);
  run(['enable']);
  assert.equal(read().mcpServers.huaweicloud.disabled, false);
  console.log('local lifecycle passed; no cloud API operations performed');
} finally { rmSync(root, { recursive: true, force: true }); }
