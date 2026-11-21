#!/usr/bin/env node
import { stdin, stdout } from 'node:process';
import { rmSync, existsSync, readFileSync } from 'node:fs';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { TOOL_DEFINITIONS, callTool } from './tools.mjs';

try {
  const { readProxyConfig } = await import('./proxy/proxy-config.mjs');
  const proxyConfig = readProxyConfig();
  if (proxyConfig) {
    if (proxyConfig.https_proxy || proxyConfig.HTTPS_PROXY) {
      process.env.HTTPS_PROXY = process.env.HTTPS_PROXY || proxyConfig.https_proxy || proxyConfig.HTTPS_PROXY;
    }
    if (proxyConfig.http_proxy || proxyConfig.HTTP_PROXY) {
      process.env.HTTP_PROXY = process.env.HTTP_PROXY || proxyConfig.http_proxy || proxyConfig.HTTP_PROXY;
    }
  }
} catch {}

// The MCP server is now loaded by a live agent session. Clear the install marker
// in this plugin dir so `doctor` no longer reports "restart needed".
try {
  const pluginDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const marker = resolve(pluginDir, '.installed');
  if (existsSync(marker)) rmSync(marker, { force: true });
} catch {}

const __dirname = dirname(fileURLToPath(import.meta.url));
const pluginRoot = resolve(__dirname, '..');
const packageRoot = resolve(pluginRoot, '..', '..');
let pkgVersion = '0.0.0';
for (const base of [pluginRoot, packageRoot]) {
  try {
    const version = JSON.parse(readFileSync(join(base, 'package.json'), 'utf8')).version;
    if (version) {
      pkgVersion = version;
      break;
    }
  } catch {}
}

let buffer = Buffer.alloc(0);
let useContentLengthFraming = true;

stdin.on('data', (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  readFrames();
});

function readFrames() {
  while (true) {
    const headerEnd = buffer.indexOf('\r\n\r\n');
    if (headerEnd !== -1) {
      useContentLengthFraming = true;
      const consumed = parseContentLengthFrame(headerEnd);
      if (!consumed) return;
      continue;
    }

    const lf = buffer.indexOf('\n');
    if (lf !== -1) {
      useContentLengthFraming = false;
      const line = buffer.subarray(0, lf).toString('utf8').trim();
      buffer = buffer.subarray(lf + 1);
      if (line) void handleMessage(JSON.parse(line));
      continue;
    }

    return;
  }
}

function parseContentLengthFrame(headerEnd) {
  const header = buffer.subarray(0, headerEnd).toString('utf8');
  const match = header.match(/Content-Length:\s*(\d+)/i);
  if (!match) {
    buffer = Buffer.alloc(0);
    return true;
  }
  const length = Number(match[1]);
  const bodyStart = headerEnd + 4;
  const bodyEnd = bodyStart + length;
  if (buffer.length < bodyEnd) return false;
  const body = buffer.subarray(bodyStart, bodyEnd).toString('utf8');
  buffer = buffer.subarray(bodyEnd);
  void handleMessage(JSON.parse(body));
  return true;
}

async function handleMessage(message) {
  if (!Object.hasOwn(message, 'id')) {
    if (message.method === 'notifications/initialized') return;
    return;
  }
  try {
    const result = await dispatch(message.method, message.params || {});
    writeMessage({ jsonrpc: '2.0', id: message.id, result });
  } catch (error) {
    writeMessage({
      jsonrpc: '2.0',
      id: message.id,
      error: {
        code: -32603,
        message: error.message,
      },
    });
  }
}

async function dispatch(method, params) {
  if (method === 'initialize') {
    return {
      protocolVersion: params.protocolVersion || '2024-11-05',
      capabilities: {
        tools: {},
      },
      serverInfo: {
        name: 'huaweicloud-devkit',
        version: pkgVersion,
      },
    };
  }

  if (method === 'tools/list') {
    return { tools: TOOL_DEFINITIONS };
  }

  if (method === 'tools/call') {
    const result = await callTool(params.name, params.arguments || {});
    return {
      content: [
        {
          type: 'text',
          text: JSON.stringify(result, null, 2),
        },
      ],
      isError: false,
    };
  }

  if (method === 'resources/list') {
    return { resources: [] };
  }

  throw new Error(`Unsupported method: ${method}`);
}

function writeMessage(message) {
  const json = JSON.stringify(message);
  if (useContentLengthFraming) {
    stdout.write(`Content-Length: ${Buffer.byteLength(json, 'utf8')}\r\n\r\n${json}`);
  } else {
    stdout.write(json + '\n');
  }
}
