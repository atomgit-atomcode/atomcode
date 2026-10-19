import * as http from 'node:http';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { randomBytes, randomUUID, timingSafeEqual } from 'node:crypto';
import { EditorHost, parseQuery, QueryError, runQuery } from './query';

const MAX_BODY = 16 * 1024;
const MAX_RESPONSE = 1024 * 1024;
export function within(root: string, file: string): boolean {
  const relative = path.relative(root, file);
  return relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative));
}

export async function startBridge(directory: string, roots: string[], host: EditorHost, ide: string) {
  const workspaceRoots = await Promise.all(roots.map(root => fs.realpath(root)));
  if (!workspaceRoots.length) throw new Error('Open a workspace before enabling the language bridge.');
  const token = randomBytes(32).toString('hex');
  const windowId = randomUUID();
  let active = 0;
  let disposed = false;
  const server = http.createServer((request, response) => {
    const reply = (code: number, body: object) => {
      if (response.destroyed || response.writableEnded) return;
      let data = JSON.stringify({ protocol_version: 1, window_id: windowId, source: 'ide', ...body });
      if (Buffer.byteLength(data) > MAX_RESPONSE) data = JSON.stringify({ protocol_version: 1, window_id: windowId, source: 'ide', status: 'response_too_large' });
      response.writeHead(code, { 'content-type': 'application/json', 'cache-control': 'no-store' });
      response.end(data);
    };
    const auth = Buffer.from(request.headers.authorization ?? '');
    const expected = Buffer.from(`Bearer ${token}`);
    // Do not expose this endpoint to browser pages (including authenticated extension webviews).
    if (request.headers.origin || auth.length !== expected.length || !timingSafeEqual(auth, expected)) {
      reply(401, { status: 'unauthorized' });
      request.resume();
      return;
    }
    if (request.method !== 'POST' || request.url !== '/query') {
      reply(404, { status: 'not_found' });
      request.resume();
      return;
    }
    if (active >= 4 || disposed) {
      reply(503, { status: 'not_ready' });
      request.resume();
      return;
    }
    active++;
    let released = false;
    let running = false;
    const release = () => { if (!released) { active--; released = true; } };
    response.once('close', () => { if (!running) release(); });
    const chunks: Buffer[] = [];
    let bytes = 0;
    request.on('data', (chunk: Buffer) => {
      bytes += chunk.length;
      if (bytes > MAX_BODY) {
        reply(413, { status: 'invalid_request', message: 'Query exceeds request limit.' });
      } else chunks.push(chunk);
    });
    request.once('error', () => { if (!running) release(); response.destroy(); });
    request.once('end', () => {
      void (async () => {
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          if (bytes > MAX_BODY || response.destroyed || response.writableEnded) return;
          let input: unknown;
          try { input = JSON.parse(Buffer.concat(chunks).toString('utf8')); }
          catch { throw new QueryError('invalid_request', 'Invalid JSON query.'); }
          const q = parseQuery(input);
          const task = async () => {
            const root = await fs.realpath(q.workspace_root);
            const file = await fs.realpath(q.file_path);
            if (!workspaceRoots.some(workspace => within(workspace, root)) || !within(root, file)) throw new QueryError('wrong_workspace', 'The query does not belong to this IDE workspace.');
            return runQuery(host, file, q);
          };
          running = true;
          // VS Code provider commands cannot be cancelled through executeCommand.
          // Keep the slot until the underlying task settles, even after timeout/disconnect.
          const pending = task();
          void pending.then(release, release);
          const deadline = new Promise<never>((_, reject) => {
            timer = setTimeout(() => reject(new QueryError('timeout', 'IDE query timed out; the provider may still be indexing.')), (q.timeout_secs ?? 20) * 1000);
          });
          reply(200, await Promise.race([pending, deadline]));
        } catch (error) {
          reply(200, error instanceof QueryError ? { status: error.status, message: error.message } : { status: 'unavailable', message: 'IDE document query failed.' });
        } finally {
          if (timer) clearTimeout(timer);
          if (!running) release();
        }
      })();
    });
  });
  server.headersTimeout = 5000;
  server.requestTimeout = 10000;
  server.maxHeadersCount = 30;
  let connectionFile: string | undefined;
  let createdFile = false;
  try {
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject);
      server.listen(0, '127.0.0.1', () => { server.removeListener('error', reject); resolve(); });
    });
    const address = server.address();
    if (!address || typeof address === 'string') throw new Error('Bridge could not bind loopback.');
    await fs.mkdir(directory, { recursive: true, mode: 0o700 });
    connectionFile = path.join(directory, `${windowId}.json`);
    const output = await fs.open(connectionFile, 'wx', 0o600);
    createdFile = true;
    try { await output.writeFile(JSON.stringify({
      protocol_version: 1, window_id: windowId, ide,
      endpoint: `http://127.0.0.1:${address.port}/query`, token,
      workspace_roots: workspaceRoots,
    })); } finally { await output.close(); }
  } catch (error) {
    server.close();
    server.closeAllConnections();
    if (createdFile && connectionFile) await fs.rm(connectionFile, { force: true });
    throw error;
  }
  server.on('error', () => {
    disposed = true;
    server.close();
    server.closeAllConnections();
    void fs.rm(connectionFile!, { force: true }).catch(() => {});
  });
  return {
    connectionFile,
    get active() { return !disposed; },
    async dispose() {
      if (disposed) return;
      disposed = true;
      server.close();
      server.closeAllConnections();
      await fs.rm(connectionFile!, { force: true });
    },
  };
}
