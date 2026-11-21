import { getProxyUrlForTarget } from './proxy-config.mjs';

let cachedDispatcher = undefined;
let cachedDispatcherProxyUrl = null;

async function importUndici() {
  try {
    /* eslint-disable-next-line n/no-missing-import */
    return await import('node:undici');
  } catch {
    return await import('undici');
  }
}

export async function getProxyDispatcher(targetUrl) {
  const proxyUrl = getProxyUrlForTarget(targetUrl);
  if (!proxyUrl) return undefined;

  if (cachedDispatcher && cachedDispatcherProxyUrl === proxyUrl) {
    return cachedDispatcher;
  }

  const { ProxyAgent } = await importUndici();
  cachedDispatcher = new ProxyAgent(proxyUrl);
  cachedDispatcherProxyUrl = proxyUrl;
  return cachedDispatcher;
}

export function clearProxyDispatcherCache() {
  cachedDispatcher = undefined;
  cachedDispatcherProxyUrl = null;
}

export async function createProxyWebSocket(url, protocols) {
  const proxyUrl = getProxyUrlForTarget(url);
  if (!proxyUrl) {
    return new globalThis.WebSocket(url, protocols);
  }

  const dispatcher = await getProxyDispatcher(url);
  const { WebSocket: UndiciWebSocket } = await importUndici();

  const wsOptions = { dispatcher };
  if (protocols) {
    if (Array.isArray(protocols)) {
      wsOptions.protocols = protocols;
    } else {
      wsOptions.protocols = [protocols];
    }
  }

  return new UndiciWebSocket(url, wsOptions);
}

export async function getWebSocketImpl(targetUrl) {
  const proxyUrl = getProxyUrlForTarget(targetUrl);
  if (!proxyUrl) return globalThis.WebSocket;

  const dispatcher = await getProxyDispatcher(targetUrl);
  const { WebSocket: UndiciWebSocket } = await importUndici();

  class ProxyWebSocket extends UndiciWebSocket {
    constructor(url, protocols) {
      const options = { dispatcher };
      if (protocols) {
        options.protocols = Array.isArray(protocols) ? protocols : [protocols];
      }
      super(url, options);
    }
  }

  return ProxyWebSocket;
}
