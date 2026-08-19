#!/usr/bin/env node
// L3 最小化前端验证：mock atomcode daemon 的 /live SSE 与最小 REST 端点。
// 让浏览器打开真实 webui SPA 连它，验证「终态事件 → 浏览器通知」全链路，
// 无需启动完整 atomcode。
//
// 用法：
//   1. cd webui && npm run build   （先产出 webui/dist，本脚本也会 serve 静态文件）
//   2. node scripts/mock-live-server.mjs [--port 13457] [--delay-ms 3000] [--stop-reason natural]
//   3. 浏览器打开 http://127.0.0.1:<port>/?sync=1&session=mock-session
//      —— 页面加载后连接 /live，收到 snapshot → state(running:true) →
//         delay 后 state(running:false, stop_reason)。切到后台标签，应弹浏览器通知。
//
// 端点：/live(SSE) /config /project /sessions /models /skills /health + 静态资源(SPA)。
// 无鉴权：真实 daemon 的 token/cookie 校验在此省略，仅用于前端联调。

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = fileURLToPath(new URL('.', import.meta.url));
const DIST = normalize(join(__dirname, '..', 'dist'));
const PORT = Number(process.argv.find((a) => a.startsWith('--port='))?.split('=')[1] ?? 13457);
const DELAY_MS = Number(process.argv.find((a) => a.startsWith('--delay-ms='))?.split('=')[1] ?? 3000);
const BETWEEN_MS = Number(process.argv.find((a) => a.startsWith('--between-ms='))?.split('=')[1] ?? 6000);
const STOP_REASON = process.argv.find((a) => a.startsWith('--stop-reason='))?.split('=')[1] ?? 'natural';

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.map': 'application/json',
};

function json(res, status, body) {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(body));
}

async function serveStatic(res, pathname) {
  let rel = pathname === '/' ? 'index.html' : pathname.replace(/^\/+/, '');
  // 防目录穿越：只允许 dist 内的文件。
  const file = normalize(join(DIST, rel));
  if (!file.startsWith(DIST) || !existsSync(file)) {
    // SPA fallback → index.html（模拟真实 daemon 的 asset_or_index）。
    const index = join(DIST, 'index.html');
    if (existsSync(index)) {
      const data = await readFile(index);
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      res.end(data);
      return;
    }
    res.writeHead(404, { 'content-type': 'text/plain' });
    res.end('webui not built: run `cd webui && npm run build` first');
    return;
  }
  const data = await readFile(file);
  const type = MIME[extname(file)] ?? 'application/octet-stream';
  res.writeHead(200, { 'content-type': type });
  res.end(data);
}

// /live SSE：连接后先发 snapshot（空会话），然后**循环**发回合——
// state(running:true) → DELAY_MS 后 state(running:false, stop_reason) →
// 停 BETWEEN_MS → 下一轮。循环让测试者无需精确计时：授权/切后台后，最多等
// DELAY_MS + BETWEEN_MS 就能收到下一个终态。终态间隔 = DELAY_MS + BETWEEN_MS
// 必须 > 5000ms（前端同 session 5s 去重窗口），默认 3000+6000=9000ms 满足。
// 可加 ?session=<id> 指定会话 id。
function serveLive(req, res) {
  const url = new URL(req.url, 'http://localhost');
  const sessionId = url.searchParams.get('session') ?? 'mock-session';
  res.writeHead(200, {
    'content-type': 'text/event-stream',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  const send = (event) => {
    console.log(`[mock /live] → ${JSON.stringify(event)}`);
    res.write(`data: ${JSON.stringify(event)}\n\n`);
  };
  let closed = false;
  let round = 0;

  send({
    type: 'snapshot',
    messages: [],
    session_id: sessionId,
    project_hash: 'mock-project',
    provider: 'mock',
    mode: 'build',
    session_name: 'Mock session',
  });

  const nextRound = () => {
    if (closed) return;
    round += 1;
    send({ type: 'state', running: true });
    setTimeout(() => {
      if (closed) return;
      send({ type: 'state', running: false, stop_reason: STOP_REASON, message: 'mock turn finished' });
      console.log(`[mock /live] 第 ${round} 轮终态已发，${BETWEEN_MS}ms 后下一轮`);
      setTimeout(nextRound, BETWEEN_MS);
    }, DELAY_MS);
  };
  nextRound();

  // 15s keepalive（与真实 daemon 对齐，避免前端看门狗判死）。
  const keepalive = setInterval(() => res.write(': keepalive\n\n'), 15000);
  req.on('close', () => {
    closed = true;
    clearInterval(keepalive);
  });
  console.log(`[mock /live] 客户端已连接 session=${sessionId}，循环回合 delay=${DELAY_MS}ms between=${BETWEEN_MS}ms`);
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  const pathname = url.pathname;
  try {
    if (pathname === '/live') return serveLive(req, res);
    if (pathname === '/health') return json(res, 200, { ok: true });
    if (pathname === '/project') {
      return json(res, 200, { working_dir: __dirname, project_hash: 'mock-project', name: 'mock' });
    }
    if (pathname === '/config') {
      return json(res, 200, {
        path: '/mock/atomcode.toml',
        default_provider: 'mock',
        default_workdir: __dirname,
        providers: [],
        notifications: { enabled: true, min_duration_secs: 8, bell: true },
      });
    }
    if (pathname === '/sessions') return json(res, 200, []);
    if (pathname === '/sessions/by-working-dir') return json(res, 200, []);
    if (pathname === '/sessions/resolve') return json(res, 200, null);
    if (pathname.startsWith('/sessions/resolve/')) return json(res, 404, { ok: false, error: 'mock: session not found' });
    if (pathname === '/projects') return json(res, 200, []);
    if (pathname === '/models') return json(res, 200, []);
    if (pathname === '/skills') return json(res, 200, []);
    if (pathname === '/chat/active') return json(res, 200, []);
    if (pathname === '/mcp/status') return json(res, 200, { servers: [], trusted: true, blocked: [] });
    // 其余未知 API 路径一律返回 JSON 404，避免落到 SPA fallback 返回 index.html，
    // 导致前端把 HTML 当 JSON 解析（如 resolveSession 报 Unexpected token '<'）。
    if (pathname.startsWith('/sessions/') || pathname.startsWith('/projects/') || pathname.startsWith('/chat/')) {
      return json(res, 404, { ok: false, error: `mock: unknown API path ${pathname}` });
    }
    return serveStatic(res, pathname);
  } catch (err) {
    res.writeHead(500, { 'content-type': 'text/plain' });
    res.end(`mock server error: ${err.message}`);
  }
});

// 端口自动扫描：与真实 daemon 的 bind_scanning 对齐——首选 PORT，被占时向上
// 探测（最多 +20）。真实环境里 13457/13458/13459 常被多个 atomcode 实例占用，
// 直接崩在 EADDRINUSE 会让测试工具不可用。
const MAX_SCAN = 20;

function tryListen(port) {
  return new Promise((resolve, reject) => {
    const onListening = () => {
      server.removeListener('error', onError);
      resolve(port);
    };
    const onError = (err) => {
      server.removeListener('listening', onListening);
      reject(err);
    };
    server.once('listening', onListening);
    server.once('error', onError);
    server.listen(port, '127.0.0.1');
  });
}

async function bindWithScanning() {
  let port = PORT;
  for (let i = 0; i <= MAX_SCAN; i += 1) {
    try {
      return await tryListen(port);
    } catch (err) {
      if (err.code === 'EADDRINUSE' && i < MAX_SCAN) {
        console.log(`  端口 ${port} 被占用，尝试 ${port + 1} …`);
        port += 1;
        continue;
      }
      throw err;
    }
  }
  throw new Error(`端口 ${PORT}~${PORT + MAX_SCAN} 全部被占用，请换 --port 重试`);
}

bindWithScanning()
  .then((boundPort) => {
    console.log(`mock atomcode daemon (webui 验证用) listening on http://127.0.0.1:${boundPort}`);
    console.log(`  静态根: ${DIST}（若 404 提示未构建，先 cd webui && npm run build）`);
    console.log(`  /live delay=${DELAY_MS}ms stop_reason=${STOP_REASON}`);
    console.log(`  打开: http://127.0.0.1:${boundPort}/?sync=1&session=mock-session`);
  })
  .catch((err) => {
    console.error(`mock server 启动失败: ${err.message}`);
    process.exit(1);
  });
