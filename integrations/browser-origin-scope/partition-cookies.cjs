'use strict';
const { randomUUID } = require('node:crypto');
const { cookieHeader, synchronizeResponseCookies: ordinaryResponseCookies, withNativeCookieValidation, validateCookieHeaders, isPartitionedHeader } = require('./cookies.cjs');

// First-party metadata access only. CDP method names and the original context ID
// are backend-owned, never caller arguments or a model-visible CDP endpoint.
async function withCookieStorage(context, url, signal, closeBrowser, operation) {
  signal.throwIfAborted();
  if (url.protocol !== 'https:' || typeof closeBrowser !== 'function') throw new Error('Unsupported partition cookie lookup');
  let probe, page, ownedPage, target, storage, closing, failure, result;
  const stop = () => {
    if (!closing) {
      try { closing = Promise.resolve(closeBrowser()); }
      catch { closing = Promise.reject(new Error('Owned browser cleanup failed')); }
      closing.catch(() => {});
    }
    return closing;
  };
  const abort = () => { void stop(); };
  signal.addEventListener('abort', abort, { once: true });
  try {
    const browser = context.browser();
    // Native canonicalization supplies the schemeful site, including PSL/IP
    // handling. This empty temporary jar never receives existing credentials.
    probe = await browser.newContext({ acceptDownloads: false, serviceWorkers: 'block', permissions: [] });
    signal.throwIfAborted();
    const name = 'atomcode-site-probe-' + randomUUID();
    await probe.addCookies([{ name, value: '1', url: url.origin + '/', secure: true, httpOnly: true, sameSite: 'None', partitionKey: url.origin }]);
    signal.throwIfAborted();
    const candidates = await probe.cookies(url.origin + '/');
    const site = candidates.find(cookie => cookie.name === name)?.partitionKey;
    if (typeof site !== 'string' || !site.startsWith('https://')) throw new Error('Partition site canonicalization failed');
    await probe.close(); probe = undefined;
    signal.throwIfAborted();
    // Borrow an existing tab when available; an owned blank page otherwise.
    // No navigation is needed to discover the captured public context ID.
    page = context.pages().find(candidate => !candidate.isClosed());
    if (!page) { page = await context.newPage(); ownedPage = page; }
    signal.throwIfAborted();
    target = await context.newCDPSession(page);
    const info = (await target.send('Target.getTargetInfo')).targetInfo;
    if (typeof info?.browserContextId !== 'string' || !info.browserContextId)
      throw new Error('Missing captured browser context identity');
    signal.throwIfAborted();
    storage = await browser.newBrowserCDPSession();
    result = await operation({ browser, storage, browserContextId: info.browserContextId, site });
    signal.throwIfAborted();
  } catch { failure = true; }
  finally {
    for (const [resource, method] of [[storage, 'detach'], [target, 'detach'], [ownedPage, 'close'], [probe, 'close']]) {
      if (resource) { try { await resource[method](); } catch { failure = true; } }
    }
    if (failure || signal.aborted) await stop().catch(() => { failure = true; });
    signal.removeEventListener('abort', abort);
  }
  if (failure || signal.aborted) throw new Error('Partition cookie lookup failed or cancelled; do not retry automatically');
  return result;
}
async function firstPartyCookieHeader(context, url, signal, closeBrowser) {
  return withCookieStorage(context, url, signal, closeBrowser, async state => {
    const data = await state.storage.send('Storage.getCookies', { browserContextId: state.browserContextId });
    signal.throwIfAborted();
    return cookieHeader(data.cookies, url, Date.now() / 1000, state.site);
  });
}

function sameIdentity(cookie, candidate, site) {
  return cookie.name === candidate.name && cookie.domain === candidate.domain && cookie.path === candidate.path &&
    (cookie.partitionKeyOpaque === undefined || cookie.partitionKeyOpaque === false) && cookie.partitionKey?.topLevelSite === site && cookie.partitionKey.hasCrossSiteAncestor === false;
}

async function synchronizeBrowserCookies(context, url, headers, signal, closeBrowser) {
  signal.throwIfAborted();
  if (headers === undefined || (Array.isArray(headers) && headers.length === 0)) return;
  validateCookieHeaders(headers, closeBrowser);
  if (!headers.some(isPartitionedHeader)) return ordinaryResponseCookies(context, url, headers, signal, closeBrowser);
  if (url.protocol !== 'https:') throw new Error('Partitioned response sync requires HTTPS');
  return withCookieStorage(context, url, signal, closeBrowser, state =>
    withNativeCookieValidation(state.browser, url, signal, state.browserContextId, async parse => {
      for (const header of headers) {
        signal.throwIfAborted();
        if (!isPartitionedHeader(header)) {
          await ordinaryResponseCookies(context, url, [header], signal, closeBrowser);
          continue;
        }
        const parsed = await parse(header);
        if (!parsed || parsed.cookie.secure === false) continue; // Native-invalid or missing CHIPS Secure requirement.
        const candidate = parsed.cookie;
        if (candidate.secure !== true || typeof candidate.httpOnly !== 'boolean' || typeof candidate.session !== 'boolean' || !Number.isFinite(candidate.expires))
          throw new Error('Native cookie metadata uncertain');
        const param = { partitionKey: { topLevelSite: state.site, hasCrossSiteAncestor: false } };
        for (const key of ['name', 'value', 'domain', 'path', 'secure', 'httpOnly', 'sameSite', 'priority', 'sameParty', 'sourceScheme', 'sourcePort'])
          if (candidate[key] !== undefined) param[key] = candidate[key];
        if (parsed.deleted) param.expires = 1;
        else if (!candidate.session && candidate.expires !== -1) param.expires = candidate.expires;
        await state.storage.send('Storage.setCookies', { browserContextId: state.browserContextId, cookies: [param] });
        signal.throwIfAborted();
        const data = await state.storage.send('Storage.getCookies', { browserContextId: state.browserContextId });
        signal.throwIfAborted();
        const matches = data.cookies.filter(cookie => sameIdentity(cookie, candidate, state.site));
        if (parsed.deleted) {
          if (matches.length) throw new Error('Partitioned deletion uncertain');
        } else {
          if (matches.length !== 1) throw new Error('Partitioned write identity uncertain');
          const actual = matches[0];
          for (const key of ['name', 'value', 'domain', 'path', 'secure', 'httpOnly', 'sameSite', 'priority', 'sameParty', 'sourceScheme', 'sourcePort'])
            if (actual[key] !== candidate[key]) throw new Error('Partitioned write metadata uncertain');
          if (typeof actual.session !== 'boolean' || !Number.isFinite(actual.expires) || actual.session !== candidate.session || Math.abs(actual.expires - candidate.expires) > 1)
            throw new Error('Partitioned write expiry uncertain');
        }
      }
    }));
}
module.exports = { firstPartyCookieHeader, synchronizeBrowserCookies };
