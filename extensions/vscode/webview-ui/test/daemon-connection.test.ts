import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as os from 'node:os';
import * as http from 'node:http';
import { daemonPorts, findCompatibleDaemon, acquireLaunchLock } from '../../src/daemon/connection';
import type { DaemonConnectionOps } from '../../src/daemon/connection';
import { DaemonClient } from '../../src/daemon/client';

const expected = { version: 'new', binaryHash: 'new-hash' };
const healthy = { version: 'new', binary_hash: 'new-hash' };
const foreign = { version: 'old', binary_hash: 'old-hash' };
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

async function main() {
  const stable = daemonPorts(13456, 13456, 'same-build');
  assert.deepEqual(stable, daemonPorts(13456, 13456, 'same-build'));
  assert.notDeepEqual(stable, daemonPorts(13456, 13456, 'different-build'));
  assert.equal(new Set(stable).size, stable.length);

  for (const responsive of [true, false]) {
    const started: number[] = [];
    const health = new Map([[13456, foreign]]);
    const ops: DaemonConnectionOps = {
      health: async (port) => port === 13456 && !responsive ? null : health.get(port) ?? null,
      occupied: async (port) => port === 13456,
      lock: async () => async () => {},
      start: async (port) => { started.push(port); health.set(port, healthy); return true; },
      wait: tick, active: () => true,
    };
    assert.equal(await findCompatibleDaemon([13456, 23456], expected, true, ops), 23456);
    assert.deepEqual(started, [23456], 'never launch on an occupied external endpoint');
    assert.deepEqual(health.get(13456), foreign, 'healthy and wedged external processes are untouched');
    assert.equal(await findCompatibleDaemon([13456, 23456], expected, false, ops), 23456);
    assert.equal(await findCompatibleDaemon([13456], expected, false, ops), null);
  }

  // Two IDE windows racing the same pool launch once and share the compatible daemon.
  const health = new Map<number, typeof healthy>();
  const locks = new Set<number>();
  let starts = 0;
  const ops: DaemonConnectionOps = {
    health: async (port) => health.get(port) ?? null,
    occupied: async (port) => health.has(port),
    lock: async (port) => {
      if (locks.has(port)) return null;
      locks.add(port);
      return async () => { locks.delete(port); };
    },
    start: async (port) => { starts++; await tick(); health.set(port, healthy); return true; },
    wait: tick, active: () => true,
  };
  assert.deepEqual(await Promise.all([
    findCompatibleDaemon([23456, 23457], expected, true, ops),
    findCompatibleDaemon([23456, 23457], expected, true, ops),
  ]), [23456, 23456]);
  assert.equal(starts, 1);
  assert.equal(locks.size, 0);
  assert.equal(await findCompatibleDaemon([23456], expected, true, { ...ops, active: () => false }), null);

  assert.equal(await findCompatibleDaemon([23457, 23456], expected, true, ops), 23456,
    'reuse the fallback even if an earlier port became free');
  assert.equal(starts, 1);
  const shutdownRace = { ...ops, health: async () => { return null; },
    occupied: async () => false,
    start: async () => { throw new Error('must not start after disposal'); },
    active: (() => { let checks = 0; return () => ++checks < 3; })(),
  };
  assert.equal(await findCompatibleDaemon([23456], expected, true, shutdownRace), null);

  // The real startup mutex has OS lifetime and requires no pidfile cleanup.
  let release: (() => Promise<void>) | null = null;
  let chosen = 0;
  for (let port = 23000; port < 23100 && !release; port++) {
    release = await acquireLaunchLock(port); chosen = port;
  }
  assert.ok(release);
  assert.equal(await acquireLaunchLock(chosen), null);
  await release();
  const reacquired = await acquireLaunchLock(chosen);
  assert.ok(reacquired); await reacquired();

  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-port-token-'));
  const oldHome = process.env.ATOMCODE_HOME;
  process.env.ATOMCODE_HOME = home;
  const tokens: string[] = [];
  let finishOld: (() => void) | undefined;
  let acknowledgeOld!: () => void;
  const oldStarted = new Promise<void>((resolve) => { acknowledgeOld = resolve; });
  const server = (version: string) => http.createServer((req, res) => {
    tokens.push(String(req.headers.authorization));
    if (req.url === '/models' && version === 'a') {
      finishOld = () => { res.writeHead(401); res.end('{}'); };
      acknowledgeOld(); return;
    }
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ service: 'atomcode-daemon', version }));
  });
  const a = server('a'); const b = server('b');
  const listen = (s: http.Server) => new Promise<number>((resolve) => s.listen(0, '127.0.0.1', () => resolve((s.address() as { port: number }).port)));
  const close = (s: http.Server) => new Promise<void>((resolve) => s.close(() => resolve()));
  try {
    const pa = await listen(a); const pb = await listen(b);
    fs.writeFileSync(path.join(home, `daemon-${pa}.json`), JSON.stringify({ token: 'a-token' }));
    fs.writeFileSync(path.join(home, `daemon-${pb}.json`), JSON.stringify({ token: 'b-token' }));
    const client = new DaemonClient(pa);
    assert.equal((await client.health()).version, 'a');
    const pending = assert.rejects(client.listModels(), /endpoint changed/);
    await oldStarted;
    client.setPort(pb); finishOld!();
    await pending;
    assert.equal(client.getPort(), pb);
    assert.equal((await client.health()).version, 'b');
    assert.deepEqual(tokens, ['Bearer a-token', 'Bearer a-token', 'Bearer b-token']);
  } finally {
    await Promise.all([close(a), close(b)]);
    if (oldHome === undefined) delete process.env.ATOMCODE_HOME; else process.env.ATOMCODE_HOME = oldHome;
    fs.rmSync(home, { recursive: true, force: true });
  }

  const processSource = fs.readFileSync(path.join(process.cwd(), 'src/daemon/process.ts'), 'utf8');
  assert.ok(!processSource.includes('client.shutdown('));
  assert.ok(!processSource.includes('process.kill('));
  assert.ok(!processSource.includes('taskkill'));
}
main().catch((error) => { console.error(error); process.exitCode = 1; });
