// What an old browser engine is missing, and whether this page can be reached
// at all — the two things a person who opened the webui in another browser
// needs said before anything else on the page.

/** CSS this page relies on, as `CSS.supports` conditions, with the Chromium
 *  version that brought each. */
const REQUIRED: Array<[condition: string, chromium: number]> = [
  ['(color: color-mix(in srgb, red, blue))', 111],
  ['(height: 100dvh)', 108],
  ['selector(:has(a))', 105],
];

/**
 * The oldest Chromium that draws this page as designed, when this browser
 * lacks something it uses; `null` when nothing is missing (or the browser
 * cannot be asked).
 *
 * The page keeps working without these — every one has a fallback in the
 * stylesheets — but it looks wrong in places, and a person on Huawei's or an
 * older 360/QQ browser should be told why rather than left to think the
 * service is broken.
 */
export function missingCssSince(supports?: (condition: string) => boolean): number | null {
  const ask =
    supports ??
    (typeof CSS !== 'undefined' && typeof CSS.supports === 'function'
      ? (condition: string) => CSS.supports(condition)
      : null);
  if (!ask) return null;
  let needed: number | null = null;
  for (const [condition, chromium] of REQUIRED) {
    let ok = true;
    try {
      ok = ask(condition);
    } catch {
      ok = false;
    }
    if (!ok) needed = Math.max(needed ?? 0, chromium);
  }
  return needed;
}

/** Remembered per browser, so the notice is said once and not every load. */
const DISMISSED_KEY = 'atomcode.compatNoticeDismissed';

export function compatNoticeDismissed(): boolean {
  try {
    return localStorage.getItem(DISMISSED_KEY) === '1';
  } catch {
    return false;
  }
}

export function dismissCompatNotice(): void {
  try {
    localStorage.setItem(DISMISSED_KEY, '1');
  } catch {
    /* private mode: it comes back next load, which is acceptable */
  }
}

/**
 * Whether this browser may use the daemon at all.
 *
 * Probe a protected endpoint rather than conflating an account OAuth failure
 * or an offline relay with WebUI access. Network/5xx failures are unavailable,
 * never evidence that access has been reauthorized.
 */
export type WebuiAccess = 'allowed' | 'denied' | 'unavailable';

export async function probeWebuiAccess(fetchFn: typeof fetch = fetch, token = ''): Promise<WebuiAccess> {
  const headers: Record<string, string> = { 'X-AtomCode-Client': 'webui' };
  if (token) headers.Authorization = 'Bearer ' + token;
  // The app waits on this before it draws anything, so it must not hang: a
  // daemon that is slow to answer gets the app, which reports for itself.
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 3000);
  try {
    const resp = await fetchFn('/project', { headers, signal: controller.signal });
    if (resp.status === 401 || resp.status === 403) return 'denied';
    return resp.ok ? 'allowed' : 'unavailable';
  } catch {
    return 'unavailable';
  } finally {
    clearTimeout(timer);
  }
}

/** Initial transport failures leave the app to report/reconnect for itself. */
export async function signedIn(fetchFn: typeof fetch = fetch, token = ''): Promise<boolean> {
  return (await probeWebuiAccess(fetchFn, token)) !== 'denied';
}
