// Local-only first-launch/launcher smoke. Downloads pinned npm dependencies, no cloud API calls.
import { mkdtempSync, readFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
const binary = resolve(process.argv[2] || 'target/debug/atomcode');
const root = mkdtempSync(join(tmpdir(), 'atomcode-preinstall-'));
const env = { ...process.env, ATOMCODE_HOME: root, ATOMCODE_OFFLINE: '1' };
try {
  const demo = spawnSync(binary, ['--tui', '--demo'], { env, encoding: 'utf8', timeout: 20000 });
  assert.equal(demo.status, 0, demo.stderr);
  assert.equal(existsSync(join(root, 'mcp.json')), false, 'demo has no installation side effects');
  // Fresh home has no model account: headless launch may fail, but local preinstallation precedes it.
  const launch = spawnSync(binary, ['--no-telemetry', '-p', 'local smoke'], { env, encoding: 'utf8', timeout: 20000 });
  assert.ok(!launch.error, launch.error?.message);
  const read = () => JSON.parse(readFileSync(join(root, 'mcp.json'), 'utf8'));
  const server = read().mcpServers.huaweicloud;
  assert.equal(server.disabled, true);
  assert.equal(server.trust, false);
  assert.deepEqual(server.autoApprove, []);
  assert.equal(existsSync(join(root, 'integrations/huaweicloud-attempt')), false, 'offline startup must not run npm');
  assert.equal(existsSync(join(root, 'skills/huaweicloud-preview/SKILL.md')), true);
  const prepared = spawnSync(binary, ['cloud', 'prepare', '--user-dir', root], { env, encoding: 'utf8', timeout: 180000 });
  assert.equal(prepared.status, 0, prepared.error?.message || prepared.stderr);
  assert.equal(read().mcpServers.huaweicloud.disabled, true, 'preparing never enables');
  // Simulate an interrupted/missing cache and let the MCP launch itself retry installation.
  const adapter = join(server.args[0].slice(0, -4), 'node_modules');
  // The launcher has a .mjs extension; its sibling directory holds the revisioned runtime.
  rmSync(adapter, { recursive: true, force: true });
  const requests = [
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
    { jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} },
  ].map(value => JSON.stringify(value)).join('\n') + '\n';
  const connected = spawnSync(server.command, server.args, { env, input: requests, encoding: 'utf8', timeout: 180000 });
  assert.equal(connected.status, 0, connected.error?.message || connected.stderr);
  const replies = connected.stdout.trim().split('\n').map(line => JSON.parse(line));
  assert.ok(replies.find(reply => reply.id === 2)?.result?.tools?.length > 10);
  const background = join(root, 'background');
  const bgEnv = { ...env, ATOMCODE_HOME: background, ATOMCODE_OFFLINE: '0' };
  // ACP receives no turn. Stop the idle host after two seconds; its local worker must survive.
  const acp = spawnSync(binary, ['--no-telemetry', 'acp'], { env: bgEnv, input: '', encoding: 'utf8', timeout: 2000 });
  assert.ok(!acp.error || acp.error.code === 'ETIMEDOUT', acp.error?.message);
  const bgServer = JSON.parse(readFileSync(join(background, 'mcp.json'), 'utf8')).mcpServers.huaweicloud;
  assert.equal(bgServer.disabled, true);
  const dependency = join(bgServer.args[0].slice(0, -4), 'node_modules/undici/package.json');
  const deadline = Date.now() + 120000;
  while (!existsSync(dependency) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 100));
  assert.ok(existsSync(dependency), 'background preparation must finish after parent exits');
  console.log('disabled registration, offline protection, dependency repair, launcher MCP and detached background install: OK');
} finally { rmSync(root, { recursive: true, force: true }); }
