/** Browser-only relay credentials. No URL query, cookies or localStorage. */
export interface RelayCredentials { route: string; token: string }
const STORAGE_KEY = 'atomcode.webuiRelay.v1';
const credential = /^[A-Za-z0-9._-]{16,256}$/;

export function validRelayCredentials(value: unknown): value is RelayCredentials {
  const v = value as Partial<RelayCredentials> | null;
  return !!v && typeof v.route === 'string' && credential.test(v.route)
    && typeof v.token === 'string' && credential.test(v.token);
}

/** Short fragment keys encode lowercase hex credential bytes as Base64URL. */
function compactHex(value: string | null): string | null {
  if (!value || !/^[A-Za-z0-9_-]{11,171}$/.test(value) || value.length % 4 === 1) return null;
  try {
    const bytes = atob(value.replace(/-/g, '+').replace(/_/g, '/'));
    const canonical = btoa(bytes).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    if (canonical !== value) return null;
    return Array.from(bytes, ch => ch.charCodeAt(0).toString(16).padStart(2, '0')).join('');
  } catch { return null; }
}

function hasPairing(params: URLSearchParams): boolean {
  return params.has('relay') || params.has('r') || params.has('t');
}

export function loadRelayCredentials(
  hash: string,
  storage?: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>,
): RelayCredentials | null {
  const params = new URLSearchParams(hash.replace(/^#/, ''));
  if (hasPairing(params)) {
    const short = params.has('r') || params.has('t');
    const value = short
      ? { route: compactHex(params.get('r')), token: compactHex(params.get('t')) }
      : { route: params.get('relay'), token: params.get('token') };
    const mixed = short && (params.has('relay') || params.has('token'));
    // A replacement pairing must never silently reuse the old machine.
    try { storage?.removeItem(STORAGE_KEY); } catch { /* private mode */ }
    if (mixed || !validRelayCredentials(value)) return null;
    try { storage?.setItem(STORAGE_KEY, JSON.stringify(value)); } catch { /* memory-only */ }
    return value;
  }
  try {
    const saved: unknown = JSON.parse(storage?.getItem(STORAGE_KEY) ?? 'null');
    return validRelayCredentials(saved) ? saved : null;
  } catch { return null; }
}

/** Only scrub the pairing fragment after this tab can actually restore it.
 * Private mode/quota/read-only storage must not turn a working link into an
 * unrecoverable page after a browser reload or mobile page eviction. */
export function relayPairingSaved(
  credentials: RelayCredentials | null,
  storage?: Pick<Storage, 'getItem'>,
): boolean {
  if (!credentials || !storage) return false;
  try {
    const saved: unknown = JSON.parse(storage.getItem(STORAGE_KEY) ?? 'null');
    return validRelayCredentials(saved)
      && saved.route === credentials.route && saved.token === credentials.token;
  } catch { return false; }
}

const relayPage = typeof location !== 'undefined' && location.pathname.startsWith('/webui/');
let relay: RelayCredentials | null = null;
if (relayPage) {
  let storage: Storage | undefined;
  try { storage = sessionStorage; } catch { /* memory-only */ }
  relay = loadRelayCredentials(location.hash, storage);
  if (hasPairing(new URLSearchParams(location.hash.slice(1))) && relayPairingSaved(relay, storage)) {
    history.replaceState(null, '', location.pathname + location.search);
  }
}

/** Never send either credential to another origin, or attach ambient cookies. */
export function relayRequest(
  credentials: RelayCredentials,
  input: RequestInfo | URL,
  init?: RequestInit,
): [string, RequestInit] {
  if (typeof input !== 'string' || !input.startsWith('/') || input.startsWith('//') || input.includes('\\')) {
    throw new TypeError('Relay requests must use a local API path');
  }
  const headers = new Headers(init?.headers);
  headers.set('X-Atom-Token', credentials.route);
  headers.set('Authorization', 'Bearer ' + credentials.token);
  return ['/api' + input, { ...init, headers, credentials: 'omit', redirect: 'error' }];
}

/** Fetch did not receive an HTTP response from the WebUI transport endpoint.
 * It says nothing about provider health or whether an accepted turn has stopped.
 * Do not include the URL, credentials or raw browser error in this message.
 */
export class ApiTransportError extends Error {
  readonly code = 'webui_transport_unreachable';
  readonly endpoint: 'local' | 'relay';
  constructor(endpoint: 'local' | 'relay') {
    super(endpoint === 'local'
      ? 'Cannot reach the AtomCode local service. Check the AtomCode process and service address, then refresh. / 无法访问 AtomCode 本地服务，请检查 AtomCode 进程和服务地址，恢复后刷新页面。'
      : 'Cannot reach the WebUI relay. Check the relay connection and the AtomCode host, then reconnect. / 无法访问 WebUI 中继，请检查中继连接和 AtomCode 宿主，恢复后重新连接。');
    this.name = 'ApiTransportError';
    this.endpoint = endpoint;
  }
}

/** An unpaired relay page must ask for a fresh link instead of falling back
 * to endpoints on the relay server as though it were the local daemon. */
export function createApiFetch(
  remote: boolean,
  credentials: RelayCredentials | null,
  fetchFn: typeof fetch = (input, init) => fetch(input, init),
): typeof fetch {
  return async (input, init) => {
    if (remote && !credentials) return Promise.resolve(Response.json({
      error: 'Relay is not paired. Open a fresh pairing link or scan the QR code again. / 中继未配对，请重新打开配对链接或扫码。',
      code: 'relay_unpaired',
    }, { status: 401 }));
    const args = credentials ? relayRequest(credentials, input, init) : [input, init] as const;
    try {
      return await fetchFn(...args);
    } catch (error) {
      // User cancellation is not an outage. HTTP failures remain Responses and
      // keep their normal status-based handling; never relabel a provider error.
      if (init?.signal?.aborted || (error instanceof Error && error.name === 'AbortError')) throw error;
      if (error instanceof TypeError || (error instanceof Error && error.name === 'NetworkError')) {
        throw new ApiTransportError(remote || credentials !== null ? 'relay' : 'local');
      }
      throw error;
    }
  };
}

export const apiFetch = createApiFetch(relayPage, relay);

/** UI may explain a rejected existing link without exposing either secret. */
export function hasRelayPairing(): boolean { return relay !== null; }
