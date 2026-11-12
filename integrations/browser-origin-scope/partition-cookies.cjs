'use strict';
const { randomUUID } = require('node:crypto');
const { cookieHeader } = require('./cookies.cjs');

// First-party metadata reads only. CDP method names and the original context ID
// are backend-owned, never caller arguments or a model-visible CDP endpoint.
async function firstPartyCookieHeader(context, url, signal, closeBrowser) {
  signal.throwIfAborted();
  if (url.protocol !== 'https:' || typeof closeBrowser !== 'function') throw new Error('Unsupported partition cookie lookup');
  let probe, page, target, storage, closing, failure, result;
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
    // about:blank is never navigated: no second remote request or source-page
    // execution is needed to discover the captured context's public protocol ID.
    page = await context.newPage();
    signal.throwIfAborted();
    target = await context.newCDPSession(page);
    const info = (await target.send('Target.getTargetInfo')).targetInfo;
    if (typeof info?.browserContextId !== 'string' || !info.browserContextId)
      throw new Error('Missing captured browser context identity');
    signal.throwIfAborted();
    storage = await browser.newBrowserCDPSession();
    const data = await storage.send('Storage.getCookies', { browserContextId: info.browserContextId });
    signal.throwIfAborted();
    result = cookieHeader(data.cookies, url, Date.now() / 1000, site);
  } catch { failure = true; }
  finally {
    for (const [resource, method] of [[storage, 'detach'], [target, 'detach'], [page, 'close'], [probe, 'close']]) {
      if (resource) { try { await resource[method](); } catch { failure = true; } }
    }
    if (failure || signal.aborted) await stop().catch(() => { failure = true; });
    signal.removeEventListener('abort', abort);
  }
  if (failure || signal.aborted) throw new Error('Partition cookie lookup failed or cancelled; do not retry automatically');
  return result;
}
module.exports = { firstPartyCookieHeader };
