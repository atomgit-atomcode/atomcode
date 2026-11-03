import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as http from 'node:http';
import * as os from 'node:os';
import * as path from 'node:path';
import { DaemonClient } from '../../src/daemon/client';

/**
 * 401 重读：daemon 重启后 token 文件被新 token 覆盖。客户端在收到 401 后
 * 必须失效缓存、重新读取磁盘上的 token 并重试一次（client.ts request()）。
 * 模拟方式：本地 HTTP 服务首请求返回 401，并在处理时把 token 文件改写为
 * 新 token；客户端第二次请求应携带新 token 并得到 200。
 */
async function testClientInvalidatesTokenAndRetriesWithFreshToken() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'ac-token-retry-'));
  const savedHome = process.env.ATOMCODE_HOME;
  process.env.ATOMCODE_HOME = home;

  let port = 0;
  const seenAuth: string[] = [];
  const server = http.createServer((req, res) => {
    seenAuth.push(req.headers.authorization ?? '(none)');
    if (seenAuth.length === 1) {
      // First request carries the stale token; simulate the daemon restart by
      // rewriting the token file with a fresh token, then reject with 401 so
      // the client invalidates its cache and re-reads.
      fs.writeFileSync(
        path.join(home, `daemon-${port}.json`),
        JSON.stringify({ pid: 2, port, token: 'tok-B' }),
      );
      res.writeHead(401, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'unauthorized' }));
    } else {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ ok: true }));
    }
  });

  try {
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    const address = server.address();
    assert.ok(address && typeof address !== 'string');
    port = address.port;

    // Initial token file (pre-restart).
    fs.writeFileSync(
      path.join(home, `daemon-${port}.json`),
      JSON.stringify({ pid: 1, port, token: 'tok-A' }),
    );

    const client = new DaemonClient(port);
    await client.getConfig();

    assert.deepEqual(seenAuth, ['Bearer tok-A', 'Bearer tok-B']);
  } finally {
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
    if (savedHome === undefined) {
      delete process.env.ATOMCODE_HOME;
    } else {
      process.env.ATOMCODE_HOME = savedHome;
    }
    fs.rmSync(home, { recursive: true, force: true });
  }
}

void Promise.resolve()
  .then(testClientInvalidatesTokenAndRetriesWithFreshToken)
  .catch((error) => {
    console.error(error);
    process.exit(1);
  });
