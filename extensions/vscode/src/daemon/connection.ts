import { createHash } from 'crypto';
import * as net from 'net';
import { daemonIdentityMatches } from './identity';
import type { ExpectedDaemonIdentity, RunningDaemonIdentity } from './identity';

/** Stable across IDE installations of the same build; never use an arbitrary free port. */
export function daemonPorts(preferred: number, current: number, identity: string): number[] {
  const first = 20000 + createHash('sha256').update(`${preferred}:${identity}`).digest().readUInt16BE(0) % 15000;
  return [...new Set([current, preferred, ...Array.from({ length: 8 }, (_, i) => 20000 + (first - 20000 + i) % 15000)])];
}

export function isPortOccupied(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = net.connect({ host: '127.0.0.1', port });
    let done = false;
    const finish = (occupied: boolean) => {
      if (done) return;
      done = true;
      socket.destroy();
      resolve(occupied);
    };
    socket.once('connect', () => finish(true));
    socket.once('error', (error: NodeJS.ErrnoException) => finish(error.code !== 'ECONNREFUSED'));
    socket.setTimeout(500, () => finish(true)); // Unknown is occupied, never permission to replace.
  });
}

/** OS-owned startup mutex: released on close/crash, without stale pidfile reclamation. */
export async function acquireLaunchLock(port: number): Promise<(() => Promise<void>) | null> {
  let lockPort = 40000 + port % 20000;
  if (lockPort === port) lockPort = 60000 + port % 5000;
  const server = net.createServer((socket) => socket.destroy());
  return new Promise((resolve) => {
    server.once('error', () => resolve(null));
    server.listen({ host: '127.0.0.1', port: lockPort, exclusive: true }, () => {
      server.unref();
      resolve(() => new Promise<void>((done) => server.close(() => done())));
    });
  });
}

export interface DaemonConnectionOps {
  health(port: number): Promise<RunningDaemonIdentity | null>;
  occupied(port: number): Promise<boolean>;
  lock(port: number): Promise<(() => Promise<void>) | null>;
  start(port: number): Promise<boolean>;
  wait(): Promise<void>;
  active(): boolean;
}

/** Connect or launch; there is deliberately no shutdown or external-process kill operation. */
export async function findCompatibleDaemon(
  ports: number[], expected: ExpectedDaemonIdentity, autoStart: boolean, ops: DaemonConnectionOps,
): Promise<number | null> {
  // Reuse an existing compatible fallback even if the preferred port became free.
  const observed = new Map<number, RunningDaemonIdentity | null>();
  for (const port of ports) {
    if (!ops.active()) return null;
    const health = await ops.health(port);
    observed.set(port, health);
    if (health && daemonIdentityMatches(health, expected)) return ops.active() ? port : null;
  }
  if (!autoStart) return null;
  for (const port of ports) {
    if (!ops.active()) return null;
    if (observed.get(port)) continue;
    let unlock = await ops.lock(port);
    if (!unlock) {
      // Another IDE may be starting here. Wait instead of launching a duplicate.
      const deadline = Date.now() + 10_000;
      for (let i = 0; i < 100 && ops.active() && Date.now() < deadline; i++) {
        await ops.wait();
        const ready = await ops.health(port);
        if (ready) {
          if (daemonIdentityMatches(ready, expected)) return ops.active() ? port : null;
          break;
        }
        unlock = await ops.lock(port);
        if (unlock) break;
      }
    }
    if (!unlock) continue;
    try {
      if (!ops.active()) return null;
      const ready = await ops.health(port);
      if (ready) {
        if (daemonIdentityMatches(ready, expected)) return ops.active() ? port : null;
        continue;
      }
      if (await ops.occupied(port)) continue;
      if (!ops.active()) return null;
      if (await ops.start(port)) {
        const started = await ops.health(port);
        if (started && daemonIdentityMatches(started, expected) && ops.active()) return port;
      }
    } finally { await unlock(); }
  }
  return null;
}
