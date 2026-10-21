/** Browser-only relay credentials. No URL query, cookies or localStorage. */
export interface RelayCredentials { route: string; token: string }
const STORAGE_KEY = 'atomcode.webuiRelay.v1';
const credential = /^[A-Za-z0-9._-]{16,256}$/;

export function validRelayCredentials(value: unknown): value is RelayCredentials {
  const v = value as Partial<RelayCredentials> | null;
  return !!v && typeof v.route === 'string' && credential.test(v.route)
    && typeof v.token === 'string' && credential.test(v.token);
}

export function loadRelayCredentials(
  hash: string,
  storage?: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>,
): RelayCredentials | null {
  const params = new URLSearchParams(hash.replace(/^#/, ''));
  if (params.has('relay')) {
    const value = { route: params.get('relay'), token: params.get('token') };
    // A replacement pairing must never silently reuse the old machine.
    try { storage?.removeItem(STORAGE_KEY); } catch { /* private mode */ }
    if (!validRelayCredentials(value)) return null;
    try { storage?.setItem(STORAGE_KEY, JSON.stringify(value)); } catch { /* memory-only */ }
    return value;
  }
  try {
    const saved: unknown = JSON.parse(storage?.getItem(STORAGE_KEY) ?? 'null');
    return validRelayCredentials(saved) ? saved : null;
  } catch { return null; }
}

const relayPage = typeof location !== 'undefined' && location.pathname.startsWith('/webui/');
let relay: RelayCredentials | null = null;
if (relayPage) {
  let storage: Storage | undefined;
  try { storage = sessionStorage; } catch { /* memory-only */ }
  relay = loadRelayCredentials(location.hash, storage);
  if (new URLSearchParams(location.hash.slice(1)).has('relay')) {
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

/** An unpaired relay page must ask for a fresh link instead of falling back
 * to endpoints on the relay server as though it were the local daemon. */
export function createApiFetch(
  remote: boolean,
  credentials: RelayCredentials | null,
  fetchFn: typeof fetch = (input, init) => fetch(input, init),
): typeof fetch {
  return (input, init) => {
    if (remote && !credentials) return Promise.resolve(new Response(null, { status: 401 }));
    const args = credentials ? relayRequest(credentials, input, init) : [input, init] as const;
    return fetchFn(...args);
  };
}

export const apiFetch = createApiFetch(relayPage, relay);
