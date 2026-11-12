'use strict';
const { randomUUID } = require('node:crypto');
const validationContexts = new WeakSet();

function isPartitionedHeader(value) {
  return value.split(';').slice(1).some(attribute => attribute.split('=')[0].trim().toLowerCase() === 'partitioned');
}
function validateCookieHeaders(headers, closeBrowser) {
  if (!Array.isArray(headers) || headers.some(value => typeof value !== 'string' || /[\r\n\0]/.test(value)) ||
      Buffer.byteLength(headers.join('\n')) > 16384 || typeof closeBrowser !== 'function')
    throw new Error('Unsupported response cookie headers');
}

// Let the native browser validate HTTP cookie semantics. This page never
// fetches the remote URL and never receives its body or arbitrary headers.
async function synchronizeResponseCookies(context, url, headers, signal, closeBrowser) {
  signal.throwIfAborted();
  if (headers === undefined || (Array.isArray(headers) && headers.length === 0)) return;
  validateCookieHeaders(headers, closeBrowser);
  // Raw fulfillment is allowed only in factory-created empty validation jars.
  // Ordinary original-context callers still cannot silently downgrade CHIPS.
  if (headers.some(isPartitionedHeader) && !validationContexts.has(context))
    throw new Error('Partitioned response cookie synchronization needs validated storage');
  let page, closing, failure, fulfilled = false;
  const stop = () => {
    if (!closing) {
      try { closing = Promise.resolve(closeBrowser()); }
      catch { closing = Promise.reject(new Error('Owned browser cleanup failed')); }
      closing.catch(() => {}); // Observed immediately; awaited below.
    }
    return closing;
  };
  const abort = () => { void stop(); };
  signal.addEventListener('abort', abort, { once: true });
  try {
    signal.throwIfAborted();
    page = await context.newPage();
    signal.throwIfAborted();
    await page.route('**/*', async route => {
      try {
        const request = route.request();
        if (signal.aborted || fulfilled || request.url() !== url.href || request.method() !== 'GET' ||
            !request.isNavigationRequest() || request.frame() !== page.mainFrame()) {
          await route.abort(); return;
        }
        fulfilled = true;
        await route.fulfill({ status: 200, headers: {
          'content-type': 'text/plain', 'cache-control': 'no-store',
          'content-security-policy': "default-src 'none'; sandbox",
          'set-cookie': headers.join('\n'),
        }, body: '' });
      } catch {
        failure = true;
        await route.abort().catch(() => {});
      }
    });
    signal.throwIfAborted();
    const response = await page.goto(url.href, { waitUntil: 'load', timeout: 5000 });
    if (!fulfilled || !response || response.status() !== 200 || response.url() !== url.href)
      throw new Error('Response cookie synchronization uncertain');
  } catch { failure = true; }
  finally {
    if (page) {
      try { await page.close(); } catch { failure = true; }
    }
    if (failure || signal.aborted) await stop().catch(() => { failure = true; });
    signal.removeEventListener('abort', abort);
  }
  if (failure || signal.aborted) throw new Error('Response cookie synchronization failed or cancelled; do not retry automatically');
}

// Native parsing is isolated from original credentials. This factory never
// accepts an existing context to mark as a validation jar.
async function withNativeCookieValidation(browser, url, signal, originalId, operation) {
  let scratch, page, target, storage, failure, result;
  try {
    signal.throwIfAborted();
    scratch = await browser.newContext({ acceptDownloads: false, serviceWorkers: 'block', permissions: [] });
    signal.throwIfAborted();
    page = await scratch.newPage();
    target = await scratch.newCDPSession(page);
    const info = (await target.send('Target.getTargetInfo')).targetInfo;
    if (typeof info?.browserContextId !== 'string' || !info.browserContextId || info.browserContextId === originalId)
      throw new Error('Validation context identity uncertain');
    storage = await browser.newBrowserCDPSession();
    validationContexts.add(scratch);
    const read = async () => {
      signal.throwIfAborted();
      const data = await storage.send('Storage.getCookies', { browserContextId: info.browserContextId });
      signal.throwIfAborted();
      if (!Array.isArray(data.cookies) || data.cookies.length > 1 || data.cookies.some(cookie => cookie.partitionKey !== undefined || cookie.partitionKeyOpaque === true))
        throw new Error('Native validation semantics changed');
      return data.cookies[0];
    };
    const apply = async header => {
      await synchronizeResponseCookies(scratch, url, [header], signal, () => scratch.close());
      return read();
    };
    // Detect changed interception behavior instead of relying on a historical
    // bug or assuming that protocol success means a cookie was accepted.
    const nonce = 'atomcode-cookie-validation-' + randomUUID();
    const control = await apply(nonce + '=1; Secure; HttpOnly; SameSite=None; Partitioned; Path=/');
    if (!control || control.name !== nonce || control.value !== '1' || !control.secure)
      throw new Error('Native validation control failed');
    const parse = async header => {
      signal.throwIfAborted();
      await scratch.clearCookies();
      const cookie = await apply(header);
      if (cookie) return { cookie, deleted: false };
      const pieces = header.split(';');
      const seededHeader = [pieces[0], ...pieces.slice(1).filter(attribute => !['expires', 'max-age'].includes(attribute.split('=')[0].trim().toLowerCase()))].join(';');
      if (seededHeader === header) return undefined;
      const seeded = await apply(seededHeader);
      if (!seeded) return undefined;
      // Expired cookies delete the native seed; malformed ignored headers do
      // not. This preserves precedence without writing a date/cookie parser.
      if (await apply(header)) return undefined;
      return { cookie: seeded, deleted: true };
    };
    result = await operation(parse);
    signal.throwIfAborted();
  } catch { failure = true; }
  finally {
    if (scratch) validationContexts.delete(scratch);
    for (const [resource, method] of [[storage, 'detach'], [target, 'detach'], [page, 'close'], [scratch, 'close']]) {
      if (resource) { try { await resource[method](); } catch { failure = true; } }
    }
  }
  if (failure || signal.aborted) throw new Error('Native cookie validation failed or cancelled');
  return result;
}

// Scope every header. A partition site is supplied only by native canonicalization;
// public cookie strings without ancestry metadata are never guessed.
function cookieHeader(cookies, url, now = Date.now() / 1000, partitionSite) {
  if (!Array.isArray(cookies)) throw new Error('Invalid browser cookie response');
  const applicable = cookies.filter(cookie => {
    if (!cookie || typeof cookie.domain !== 'string' || typeof cookie.path !== 'string') return false;
    const domain = cookie.domain.toLowerCase();
    const host = url.hostname.toLowerCase();
    const domainMatches = domain.startsWith('.') ? host === domain.slice(1) || host.endsWith(domain) : host === domain;
    return domainMatches && cookie.path.startsWith('/') &&
      (url.pathname === cookie.path || url.pathname.startsWith(cookie.path.endsWith('/') ? cookie.path : cookie.path + '/')) &&
      (!cookie.secure || url.protocol === 'https:') &&
      (cookie.expires === -1 || (typeof cookie.expires === 'number' && cookie.expires > now)) &&
      (cookie.partitionKeyOpaque === undefined || cookie.partitionKeyOpaque === false) &&
      (cookie.partitionKey === undefined || (url.protocol === 'https:' && typeof cookie.partitionKey === 'object' && cookie.partitionKey !== null &&
        partitionSite !== undefined && cookie.partitionKey.topLevelSite === partitionSite && cookie.partitionKey.hasCrossSiteAncestor === false));
  });
  // Most specific paths first, retaining the browser's order for ties.
  applicable.sort((a, b) => b.path.length - a.path.length);
  const values = applicable.map(cookie => {
    if (typeof cookie.name !== 'string' || typeof cookie.value !== 'string' ||
        !/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(cookie.name) ||
        !/^[\x21\x23-\x2b\x2d-\x3a\x3c-\x5b\x5d-\x7e]*$/.test(cookie.value))
      throw new Error('Unsupported browser cookie encoding');
    return cookie.name + '=' + cookie.value;
  });
  const result = values.join('; ');
  if (Buffer.byteLength(result) > 16384) throw new Error('Browser cookie header exceeds limit');
  return result;
}

module.exports = { synchronizeResponseCookies, cookieHeader, validateCookieHeaders, isPartitionedHeader, withNativeCookieValidation };
