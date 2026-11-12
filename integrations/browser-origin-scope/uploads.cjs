'use strict';
const http = require('node:http');
const https = require('node:https');
const { randomUUID } = require('node:crypto');
const { readStaged } = require('./upload-store.cjs');

// Single-dispatch multipart sender. File selection is a separate explicit host
// operation. No redirects, caller headers, paths, cookies or response body leak.
class ControlledUploads {
  constructor({ directory, authorize, timeoutMs = 15000, maxResponseBytes = 262144, maxConcurrent = 2 }) {
    if (typeof directory !== 'string' || typeof authorize !== 'function' ||
        !Number.isSafeInteger(timeoutMs) || timeoutMs <= 0 || !Number.isSafeInteger(maxResponseBytes) || maxResponseBytes < 0 ||
        !Number.isSafeInteger(maxConcurrent) || maxConcurrent < 1 || maxConcurrent > 2) throw new Error('Invalid upload configuration');
    Object.assign(this, { directory, authorize, timeoutMs, maxResponseBytes, maxConcurrent });
    this.jobs = new Map(); this.closed = false; this.draining = 0;
  }
  permitted(value) {
    if (typeof value !== 'string' || value.length > 16384 || /[\x00-\x20\x7f\\]/.test(value)) throw new Error('Upload URL refused');
    const url = new URL(value);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || this.authorize(url.origin) !== true)
      throw new Error('Upload origin refused');
    url.hash = ''; return url;
  }
  execute(id, args) {
    if (this.closed || this.draining || this.jobs.size >= this.maxConcurrent || this.jobs.has(id)) return Promise.reject(new Error('Upload unavailable'));
    if (!args || typeof args !== 'object' || Array.isArray(args) ||
        Object.keys(args).some(key => !['url', 'artifact_id', 'name', 'bytes', 'sha256', 'field_name'].includes(key)) ||
        !['url', 'artifact_id', 'name', 'bytes', 'sha256'].every(key => Object.hasOwn(args, key)) ||
        (args.field_name !== undefined && (typeof args.field_name !== 'string' || !/^[A-Za-z0-9_.\[\]-]{1,64}$/.test(args.field_name))))
      return Promise.reject(new Error('Expected an explicit snapshot identity and URL'));
    const controller = new AbortController(), job = { controller };
    this.jobs.set(id, job);
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    job.promise = this.transfer(args, controller.signal).finally(() => { clearTimeout(timer); this.jobs.delete(id); });
    return job.promise;
  }
  has(id) { return this.jobs.has(id); }
  cancel(id) { this.jobs.get(id)?.controller.abort(); }
  async drain(closeBrowser = async () => {}, permanent = false) {
    if (permanent) this.closed = true;
    this.draining++;
    try {
      const pending = [...this.jobs.values()];
      for (const job of pending) job.controller.abort();
      await Promise.allSettled(pending.map(job => job.promise));
      await closeBrowser();
    } finally { this.draining--; }
  }
  async transfer(args, signal) {
    const url = this.permitted(args.url); // Refuse the destination before reading any selected bytes.
    const snapshot = await readStaged(this.directory, args.artifact_id, signal);
    const selected = snapshot.metadata;
    if (selected.name !== args.name || selected.bytes !== args.bytes || selected.sha256 !== args.sha256)
      throw new Error('Approval-visible upload identity does not match selection');
    signal.throwIfAborted();
    this.permitted(url.href); // Grants may have changed during local I/O.
    if (Date.now() >= selected.expires_at) throw new Error('Upload selection expired');
    const boundary = 'atomcode-' + randomUUID();
    // Match HTML multipart escaping for quotes; source selection forbids CR/LF,
    // path separators and other controls. Unicode is encoded in the body as UTF-8.
    const filename = selected.name.replace(/"/g, '%22');
    const prefix = Buffer.from(`--${boundary}\r\nContent-Disposition: form-data; name="${args.field_name || 'file'}"; filename="${filename}"\r\nContent-Type: application/octet-stream\r\n\r\n`);
    const suffix = Buffer.from(`\r\n--${boundary}--\r\n`);
    const status = await new Promise((resolve, reject) => {
      let request, response, settled = false, finished = false, complete = false;
      const fail = () => {
        if (settled) return; settled = true;
        request?.destroy(); response?.destroy();
        reject(new Error('Upload outcome unknown or refused; do not retry automatically'));
      };
      const done = () => {
        if (!settled && finished && complete) { settled = true; resolve(response.statusCode); }
      };
      request = (url.protocol === 'https:' ? https : http).request(url, {
        method: 'POST', agent: false, signal, rejectUnauthorized: true, insecureHTTPParser: false, maxHeaderSize: 16384,
        headers: { 'Content-Type': `multipart/form-data; boundary=${boundary}`,
          'Content-Length': prefix.length + snapshot.body.length + suffix.length, 'Accept-Encoding': 'identity' },
      }, incoming => {
        response = incoming;
        response.on('error', fail); response.on('aborted', fail);
        if (response.statusCode < 200 || response.statusCode >= 300 ||
            !['', 'identity'].includes(String(response.headers['content-encoding'] || '').toLowerCase())) return fail();
        const length = response.headers['content-length'];
        if (length !== undefined && (!/^\d+$/.test(length) || Number(length) > this.maxResponseBytes)) return fail();
        let received = 0;
        response.on('data', chunk => { received += chunk.length; if (received > this.maxResponseBytes) fail(); });
        response.once('end', () => {
          if (!response.complete) return fail();
          complete = true; done();
        });
      });
      request.once('error', fail);
      request.once('finish', () => { finished = true; done(); });
      request.once('close', () => { if (!finished || !complete) fail(); });
      // These are the exact verified buffers. Never reopen a selected path and
      // never resubmit on a 3xx, transport failure or cancellation.
      request.write(prefix); request.write(snapshot.body); request.end(suffix);
    });
    // No post-ack abort check turns a completed upload into a retryable failure.
    return { artifact_id: selected.artifact_id, name: selected.name, bytes: selected.bytes, sha256: selected.sha256, status };
  }
}
module.exports = { ControlledUploads };
