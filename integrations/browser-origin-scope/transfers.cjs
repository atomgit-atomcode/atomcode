'use strict';

const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const https = require('node:https');
const { randomUUID, createHash } = require('node:crypto');
const { Transform, Writable } = require('node:stream');
const { pipeline } = require('node:stream/promises');

// Backend-owned publication boundary. Transport cancellation and shutdown
// must drain jobs here before disposing the owned browser.
class ControlledDownloads {
  constructor({ files, authorize, getContext, maxBytes = 64 * 1024 * 1024, timeoutMs = 15000 }) {
    if (!path.isAbsolute(files) || !Number.isSafeInteger(maxBytes) || maxBytes <= 0 ||
        !Number.isSafeInteger(timeoutMs) || timeoutMs <= 0 || typeof authorize !== 'function')
      throw new Error('Invalid controlled download configuration');
    this.files = files;
    this.authorize = authorize;
    this.getContext = getContext;
    this.maxBytes = maxBytes;
    this.timeoutMs = timeoutMs;
    this.jobs = new Map();
    this.closed = false;
    this.draining = 0;
    this.checkDirectory(files);
    this.staging = path.join(path.dirname(files), 'download-staging');
    try { fs.mkdirSync(this.staging, { mode: 0o700 }); }
    catch (error) { if (error.code !== 'EEXIST') throw error; }
    this.checkDirectory(this.staging);
  }

  checkDirectory(directory) {
    const stat = fs.lstatSync(directory);
    if (!stat.isDirectory() || (process.getuid && stat.uid !== process.getuid()))
      throw new Error('Download directories must be real and owned');
    if (process.platform !== 'win32') fs.chmodSync(directory, 0o700);
  }

  execute(id, args) {
    if (this.closed || this.draining || this.jobs.has(id)) return Promise.reject(new Error('Download unavailable'));
    if (!args || typeof args !== 'object' || Array.isArray(args) ||
        Object.keys(args).some(key => !['url', 'use_browser_context'].includes(key)) ||
        !Object.hasOwn(args, 'url') || (Object.hasOwn(args, 'use_browser_context') && typeof args.use_browser_context !== 'boolean'))
      return Promise.reject(new Error('Expected only a download URL'));
    const controller = new AbortController();
    const job = { controller };
    this.jobs.set(id, job);
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    job.promise = this.transfer(args.url, controller.signal, args.use_browser_context === true).finally(() => {
      clearTimeout(timer);
      this.jobs.delete(id);
    });
    return job.promise;
  }

  has(id) { return this.jobs.has(id); }

  cancel(id) { this.jobs.get(id)?.controller.abort(); }

  close() { return this.drain(async () => {}, true); }

  async drain(closeBrowser, permanent = false) {
    if (permanent) this.closed = true;
    this.draining++;
    try {
      const pending = [...this.jobs.values()];
      for (const job of pending) job.controller.abort();
      await Promise.allSettled(pending.map(job => job.promise));
      await closeBrowser();
    } finally { this.draining--; }
  }

  permitted(value) {
    if (typeof value !== 'string' || value.length > 16384 || /[\x00-\x20\x7f\\]/.test(value))
      throw new Error('Download URL refused');
    let url;
    try { url = new URL(value); } catch { throw new Error('Download URL refused'); }
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password ||
        this.authorize(url.origin) !== true) throw new Error('Download origin refused');
    url.hash = '';
    return url;
  }

  async transfer(value, signal, authenticated) {
    let temporary;
    let handle;
    let response;
    try {
      let url = this.permitted(value);
      // Capture one existing context; never launch or adopt a replacement.
      const context = authenticated ? await abortable(() => this.getContext?.(), signal) : undefined;
      signal.throwIfAborted();
      if (authenticated && !context) throw new Error('No owned browser context');
      const visited = new Set();
      for (let hop = 0; ; hop++) {
        signal.throwIfAborted();
        if (hop > 10 || visited.has(url.href)) throw new Error('Download redirect limit');
        visited.add(url.href);
        const headers = { 'Accept-Encoding': 'identity' };
        if (context) {
          const cookies = await abortable(() => context.cookies(url.href), signal);
          signal.throwIfAborted();
          const cookie = cookieHeader(cookies, url);
          if (cookie) headers.Cookie = cookie;
        }
        response = await new Promise((resolve, reject) => {
          const request = (url.protocol === 'https:' ? https : http).get(url, {
            agent: false, signal, headers, rejectUnauthorized: true,
          }, resolve);
          request.once('error', reject);
        });
        // Install immediately, including while headers are inspected.
        response.on('error', () => {});
        if ([301, 302, 303, 307, 308].includes(response.statusCode)) {
          const location = response.headers.location;
          response.destroy();
          if (typeof location !== 'string') throw new Error('Download redirect refused');
          url = this.permitted(new URL(location, url).href);
          continue;
        }
        break;
      }
      if (response.statusCode !== 200 ||
          !['', 'identity'].includes(String(response.headers['content-encoding'] || '').toLowerCase()))
        throw new Error('Download response refused');
      const length = response.headers['content-length'];
      if (length !== undefined && (!/^\d+$/.test(length) || Number(length) > this.maxBytes))
        throw new Error('Download exceeds size limit');
      this.checkDirectory(this.files);
      this.checkDirectory(this.staging);
      const name = `download-${randomUUID()}.bin`;
      temporary = path.join(this.staging, `${randomUUID()}.part`);
      handle = await fs.promises.open(temporary, 'wx', 0o600);
      const hash = createHash('sha256');
      let bytes = 0;
      const limit = new Transform({ transform: (chunk, _encoding, done) => {
        bytes += chunk.length;
        if (bytes > this.maxBytes) return done(new Error('Download exceeds size limit'));
        hash.update(chunk);
        done(null, chunk);
      } });
      const output = new Writable({ write(chunk, _encoding, done) {
        handle.writeFile(chunk).then(() => done(), done);
      } });
      await pipeline(response, limit, output, { signal });
      if (!response.complete) throw new Error('Incomplete download');
      await handle.sync();
      await handle.close();
      handle = undefined;
      signal.throwIfAborted();
      // No await between the final cancellation check and exclusive commit.
      // A late cancellation/result loss cannot undo a committed artifact.
      const destination = path.join(this.files, name);
      fs.linkSync(temporary, destination);
      return { path: destination, bytes, sha256: hash.digest('hex') };
    } catch {
      throw new Error('Download failed or cancelled; do not automatically retry: the request may have had effects');
    } finally {
      response?.destroy();
      if (handle) await handle.close().catch(() => {});
      if (temporary) await fs.promises.unlink(temporary).catch(() => {});
    }
  }
}

// A cancelled metadata read must not hold transfer shutdown open. The original
// SDK promise stays observed, and its late value cannot resume dispatch.
function abortable(read, signal) {
  signal.throwIfAborted();
  return new Promise((resolve, reject) => {
    const aborted = () => reject(new Error('Download cancelled'));
    signal.addEventListener('abort', aborted, { once: true });
    Promise.resolve().then(() => { signal.throwIfAborted(); return read(); }).then(resolve, reject)
      .finally(() => signal.removeEventListener('abort', aborted));
  });
}

// BrowserContext.cookies(url) already scopes cookies. Recheck before building
// a header, and never infer the top-level partition for CHIPS cookies.
function cookieHeader(cookies, url, now = Date.now() / 1000) {
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
      cookie.partitionKey === undefined;
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

module.exports = { ControlledDownloads, cookieHeader };
