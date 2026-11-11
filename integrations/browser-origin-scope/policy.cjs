'use strict';

const http = require('node:http');
const net = require('node:net');

const CAPABILITY = 'atomcode/browser-origin-scope';
const POLICY_ENV = 'ATOMCODE_BROWSER_ORIGIN_POLICY';
const TOOLS = new Set([
  'browser_close', 'browser_resize', 'browser_handle_dialog', 'browser_fill_form',
  'browser_press_key', 'browser_type', 'browser_navigate', 'browser_navigate_back',
  'browser_take_screenshot', 'browser_snapshot', 'browser_click', 'browser_drag',
  'browser_hover', 'browser_select_option', 'browser_tabs', 'browser_wait_for',
]);

function parsePolicy(raw) {
  if (typeof raw !== 'string' || Buffer.byteLength(raw) > 65536)
    throw new Error('A bounded explicit browser origin policy is required');
  let policy;
  try { policy = JSON.parse(raw); } catch { throw new Error('Invalid browser origin policy JSON'); }
  if (!policy || Array.isArray(policy) || policy.version !== 1 ||
      Object.keys(policy).some(key => !['version', 'origins'].includes(key)) ||
      !Array.isArray(policy.origins) || policy.origins.length > 256)
    throw new Error('Expected version 1 and at most 256 exact HTTP(S) origins; unsupported permissions are refused');
  const origins = new Set();
  for (const value of policy.origins) {
    if (typeof value !== 'string' || value.length > 2048 || value.trim() !== value || /[\x00-\x20\x7f]/.test(value) ||
        !/^https?:\/\/[^/?#@\\]+\/?$/i.test(value))
      throw new Error('Expected exact HTTP(S) origins, without credentials, paths or wildcards');
    let url;
    try { url = new URL(value); } catch { throw new Error('Invalid browser origin'); }
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password ||
        url.pathname !== '/' || url.search || url.hash || value.includes('*') || value.includes('\\'))
      throw new Error('Expected exact HTTP(S) origins, without credentials, paths or wildcards');
    origins.add(url.origin);
  }
  return origins;
}

function permittedUrl(value, origins, protocols = ['http:', 'https:']) {
  if (typeof value !== 'string' || /[\x00-\x20\x7f]/.test(value)) return null;
  try {
    const url = new URL(value);
    if (!protocols.includes(url.protocol) || url.username || url.password) return null;
    const origin = url.protocol === 'ws:' ? `http://${url.host}` :
      url.protocol === 'wss:' ? `https://${url.host}` : url.origin;
    return origins.has(origin) ? url : null;
  } catch { return null; }
}

function allowedCall(params, origins) {
  if (!params || !TOOLS.has(params.name)) return false;
  const args = params.arguments === undefined ? {} : params.arguments;
  if (!args || typeof args !== 'object' || Array.isArray(args)) return false;
  if (params.name === 'browser_navigate' && !permittedUrl(args.url, origins)) return false;
  // The backend's file guard is convenience, not an execution boundary. This
  // interface permits generated artifacts, never caller-chosen filesystem paths.
  if (Object.hasOwn(args, 'filename') || Object.hasOwn(args, '_meta')) return false;
  return true;
}

function hostname(url) { return url.hostname.replace(/^\[|\]$/g, ''); }

async function createProxy(origins) {
  const stats = { httpAllowed: 0, httpDenied: 0, connectAllowed: 0, connectDenied: 0, upgradeAllowed: 0, upgradeDenied: 0 };
  const sockets = new Set();
  const track = socket => {
    sockets.add(socket);
    socket.on('error', () => {});
    socket.on('close', () => sockets.delete(socket));
    socket.setTimeout(30000, () => socket.destroy());
    return socket;
  };
  const refuse = res => { res.writeHead(403, { 'Content-Type': 'text/plain', Connection: 'close' }); res.end('Browser origin policy refused this request'); };
  const server = http.createServer((req, res) => {
    const url = permittedUrl(req.url, origins, ['http:']);
    if (!url) { stats.httpDenied++; return refuse(res); }
    stats.httpAllowed++;
    const headers = { ...req.headers, host: url.host };
    delete headers['proxy-authorization'];
    delete headers['proxy-connection'];
    const upstream = http.request({ hostname: hostname(url), port: url.port || 80,
      path: url.pathname + url.search, method: req.method, headers, agent: false }, response => {
      // Do not persist download artifacts. HTTPS transport is opaque here;
      // acceptDownloads:false independently refuses accepted downloads there.
      if (/^\s*attachment(?:;|$)/i.test(String(response.headers['content-disposition'] || ''))) {
        response.destroy();
        return refuse(res);
      }
      res.writeHead(response.statusCode || 502, response.headers);
      response.pipe(res);
    });
    upstream.on('socket', track);
    upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end('Browser proxy request failed'); });
    req.on('aborted', () => upstream.destroy());
    res.on('close', () => upstream.destroy());
    req.pipe(upstream);
  });
  server.on('connection', track);
  server.on('clientError', (_error, socket) => socket.destroy());
  const tunnelOrigins = new WeakMap();
  const tunnelParser = http.createServer((_req, res) => refuse(res));
  tunnelParser.on('clientError', (_error, socket) => socket.destroy());
  tunnelParser.on('upgrade', (req, client, head) => websocket(req, client, head, tunnelOrigins.get(client)));
  server.on('connect', (req, client, head) => {
    if (!/^(?:\[[0-9a-fA-F:]+\]|[A-Za-z0-9._-]+):[0-9]+$/.test(req.url)) {
      stats.connectDenied++; client.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n'); return;
    }
    const tlsTarget = permittedUrl(`https://${req.url}/`, origins, ['https:']);
    const plainTarget = permittedUrl(`http://${req.url}/`, origins, ['http:']);
    if (!tlsTarget && !plainTarget) {
      stats.connectDenied++; client.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n'); return;
    }
    // Chromium also uses CONNECT for plain ws://. Do not turn permission for
    // one scheme into a blind TCP tunnel for another scheme on the same port.
    // Peek before opening upstream; TLS must start as TLS, plain tunnels must
    // pass Node's HTTP parser and the exact-origin WebSocket upgrade checks.
    let prefix = Buffer.alloc(0);
    const deadline = setTimeout(() => client.destroy(), 5000);
    client.once('close', () => clearTimeout(deadline));
    const firstBytes = data => {
      prefix = Buffer.concat([prefix, data]);
      if (prefix.length < 3) return;
      client.removeListener('data', firstBytes); client.pause(); clearTimeout(deadline);
      if (prefix.length > 65536) { stats.connectDenied++; client.destroy(); return; }
      if (prefix[0] === 0x16 && prefix[1] === 0x03 && tlsTarget) {
        stats.connectAllowed++;
        const remote = track(net.connect(Number(tlsTarget.port || 443), hostname(tlsTarget)));
        remote.once('connect', () => {
          if (client.destroyed) return remote.destroy();
          remote.write(prefix); client.pipe(remote); remote.pipe(client);
        });
        remote.on('error', () => client.destroy());
        client.on('close', () => remote.destroy()); remote.on('close', () => client.destroy());
      } else if (prefix.subarray(0, 3).toString() === 'GET' && plainTarget) {
        tunnelOrigins.set(client, plainTarget.origin);
        tunnelParser.emit('connection', client);
        client.unshift(prefix); client.resume();
      } else { stats.connectDenied++; client.destroy(); }
    };
    client.on('data', firstBytes);
    client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
    if (head.length) firstBytes(head);
  });
  // Both absolute-form proxy upgrades and parsed plain CONNECT upgrades use
  // this boundary. A tunnel is bound to its authority as well as the allow-list.
  function websocket(req, client, head, tunnelOrigin) {
    let raw = req.url;
    if (tunnelOrigin) {
      try { raw = new URL(raw, tunnelOrigin).href; } catch { raw = ''; }
    }
    const url = permittedUrl(raw, origins, ['http:', 'ws:']);
    if (!url || (tunnelOrigin && url.origin !== tunnelOrigin) || req.method !== 'GET' ||
        String(req.headers.upgrade).toLowerCase() !== 'websocket') {
      stats.upgradeDenied++; client.destroy(); return;
    }
    stats.upgradeAllowed++;
    const headers = { ...req.headers, host: url.host };
    delete headers['proxy-authorization']; delete headers['proxy-connection'];
    const upstream = http.request({ hostname: hostname(url), port: url.port || 80,
      path: url.pathname + url.search, method: 'GET', headers, agent: false });
    upstream.on('socket', track);
    upstream.once('upgrade', (response, remote, remoteHead) => {
      if (client.destroyed) return remote.destroy();
      const lines = [`HTTP/1.1 ${response.statusCode} Switching Protocols`];
      for (let i = 0; i < response.rawHeaders.length; i += 2)
        lines.push(`${response.rawHeaders[i]}: ${response.rawHeaders[i + 1]}`);
      client.write(lines.join('\r\n') + '\r\n\r\n');
      if (remoteHead.length) client.write(remoteHead);
      if (head.length) remote.write(head);
      client.pipe(remote); remote.pipe(client);
      client.on('close', () => remote.destroy()); remote.on('close', () => client.destroy());
    });
    upstream.on('response', response => { response.destroy(); client.destroy(); });
    upstream.on('error', () => client.destroy());
    client.on('close', () => upstream.destroy());
    upstream.end();
  }
  server.on('upgrade', websocket);
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    stats,
    async close() {
      for (const socket of sockets) socket.destroy();
      await new Promise(resolve => server.close(resolve));
    },
  };
}

// Public MCP Transport interface; no private SDK handlers/state are changed.
class PolicyTransport {
  constructor(inner, origins, closeBrowser) { this.inner = inner; this.origins = origins; this.closeBrowser = closeBrowser; }
  async start() {
    this.inner.onmessage = (message, extra) => {
      if (message.method === 'tools/call' && !Object.hasOwn(message, 'id')) return;
      if (message.method === 'tools/call' && !allowedCall(message.params, this.origins)) {
        if (Object.hasOwn(message, 'id')) void this.inner.send({ jsonrpc: '2.0', id: message.id,
          error: { code: -32602, message: 'Browser policy refused this tool or its arguments' } }).catch(error => this.onerror?.(error));
        return;
      }
      if (message.method === 'tools/call' && message.params.name === 'browser_close' && this.closeBrowser) {
        void this.closeBrowser().then(() => this.inner.send({ jsonrpc: '2.0', id: message.id,
          result: { content: [{ type: 'text', text: 'Closed the owned isolated browser' }] } }),
          () => this.inner.send({ jsonrpc: '2.0', id: message.id, error: { code: -32603, message: 'Owned browser cleanup failed' } }))
          .catch(error => this.onerror?.(error));
        return;
      }
      this.onmessage?.(message, extra);
    };
    this.inner.onerror = error => this.onerror?.(error);
    this.inner.onclose = () => this.onclose?.();
    await this.inner.start();
  }
  async send(message, options) {
    if (!Object.hasOwn(message, 'method') && Array.isArray(message.result?.tools)) {
      const tools = message.result.tools.filter(tool => TOOLS.has(tool.name)).map(tool => {
        if (!tool.inputSchema?.properties) return tool;
        // The published contract must match the execution gate. Otherwise a
        // model following the upstream schema can request a forbidden path.
        const properties = { ...tool.inputSchema.properties };
        delete properties.filename;
        delete properties._meta;
        const inputSchema = { ...tool.inputSchema, properties };
        if (Array.isArray(inputSchema.required))
          inputSchema.required = inputSchema.required.filter(key => key !== 'filename' && key !== '_meta');
        return { ...tool, inputSchema };
      });
      message = { ...message, result: { ...message.result, tools } };
    }
    return this.inner.send(message, options);
  }
  async close() { await this.inner.close(); }
}

module.exports = { CAPABILITY, POLICY_ENV, TOOLS, parsePolicy, permittedUrl, allowedCall, createProxy, PolicyTransport };
