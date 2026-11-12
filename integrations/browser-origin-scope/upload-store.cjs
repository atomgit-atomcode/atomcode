'use strict';
// Host-selected immutable byte snapshots. This is not a model-visible file API
// or an OS sandbox against other processes running as the same user.
const fs = require('node:fs');
const path = require('node:path');
const { randomUUID, createHash } = require('node:crypto');
const MAX_BYTES = 64 * 1024 * 1024;
const TTL_MS = 15 * 60 * 1000;
const ID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const flags = fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW || 0) | (fs.constants.O_NONBLOCK || 0);

function owned(stat) { return !process.getuid || stat.uid === process.getuid(); }
async function checkDirectory(directory, create = false) {
  if (typeof directory !== 'string' || !path.isAbsolute(directory) || path.resolve(directory) === path.parse(directory).root)
    throw new Error('Expected an owned upload store directory');
  if (create) await fs.promises.mkdir(directory, { mode: 0o700 }).catch(error => { if (error.code !== 'EEXIST') throw error; });
  const stat = await fs.promises.lstat(directory);
  if (!stat.isDirectory() || !owned(stat)) throw new Error('Upload store must be a real owned directory');
  if (process.platform !== 'win32' && (stat.mode & 0o077)) throw new Error('Upload store must be private');
}
function validName(name) {
  return typeof name === 'string' && name.length > 0 && Buffer.byteLength(name) <= 255 && !/[\x00-\x1f\x7f/\\]/.test(name);
}
function metadata(value, id) {
  if (!value || Array.isArray(value) || Object.keys(value).sort().join(',') !== 'artifact_id,bytes,created_at,expires_at,name,sha256,version' ||
      value.version !== 1 || value.artifact_id !== id || !ID.test(id) || !validName(value.name) ||
      !Number.isSafeInteger(value.created_at) || value.created_at <= 0 || !Number.isSafeInteger(value.expires_at) ||
      value.expires_at - value.created_at !== TTL_MS || Date.now() < value.created_at || Date.now() >= value.expires_at ||
      !Number.isSafeInteger(value.bytes) || value.bytes < 0 || value.bytes > MAX_BYTES ||
      typeof value.sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(value.sha256)) throw new Error('Invalid upload snapshot metadata');
  return value;
}
async function regular(file, { privateFile = false, maxBytes = MAX_BYTES } = {}) {
  // O_NONBLOCK avoids hanging on a FIFO before fstat can refuse it. The final
  // symlink is refused on platforms that do not implement O_NOFOLLOW as well.
  const before = await fs.promises.lstat(file);
  if (!before.isFile() || before.isSymbolicLink()) throw new Error('Upload source must be a regular file');
  const handle = await fs.promises.open(file, flags);
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.dev !== before.dev || stat.ino !== before.ino || stat.size > maxBytes ||
        (privateFile && (!owned(stat) || stat.nlink !== 1 || (process.platform !== 'win32' && (stat.mode & 0o077)))))
      throw new Error('Upload file identity, privacy or size is invalid');
    return { handle, stat };
  } catch (error) { await handle.close().catch(() => {}); throw error; }
}
async function writeAll(handle, buffer) {
  let offset = 0;
  while (offset < buffer.length) {
    const { bytesWritten } = await handle.write(buffer, offset, buffer.length - offset);
    if (!bytesWritten) throw new Error('Upload snapshot write stalled');
    offset += bytesWritten;
  }
}
async function readBounded(handle, maxBytes, signal) {
  const chunks = []; let bytes = 0;
  while (true) {
    signal.throwIfAborted();
    const buffer = Buffer.alloc(Math.min(65536, maxBytes - bytes + 1));
    const { bytesRead } = await handle.read(buffer, 0, buffer.length);
    if (!bytesRead) break;
    bytes += bytesRead;
    if (bytes > maxBytes) throw new Error('Upload file exceeds limit');
    chunks.push(buffer.subarray(0, bytesRead));
  }
  return Buffer.concat(chunks, bytes);
}
function stable(before, after, bytes) {
  return before.dev === after.dev && before.ino === after.ino && before.size === bytes && after.size === bytes &&
    before.mtimeMs === after.mtimeMs && before.ctimeMs === after.ctimeMs;
}
async function stageFile(directory, file, signal = new AbortController().signal) {
  signal.throwIfAborted();
  if (typeof file !== 'string' || !path.isAbsolute(file) || !validName(path.basename(file))) throw new Error('Expected an explicit absolute regular file');
  await checkDirectory(directory, true);
  const id = randomUUID(), data = path.join(directory, id + '.data'), manifest = path.join(directory, id + '.json');
  const temporary = data + '.partial', manifestTemporary = manifest + '.partial';
  let input, output, manifestHandle, committed = false, dataOwned = false, temporaryOwned = false, manifestTemporaryOwned = false, manifestOwned = false;
  try {
    input = await regular(file);
    signal.throwIfAborted();
    output = await fs.promises.open(temporary, 'wx', 0o600); temporaryOwned = true;
    const hash = createHash('sha256'), buffer = Buffer.alloc(65536);
    let bytes = 0;
    while (true) {
      signal.throwIfAborted();
      const { bytesRead } = await input.handle.read(buffer, 0, buffer.length);
      if (!bytesRead) break;
      bytes += bytesRead;
      if (bytes > MAX_BYTES) throw new Error('Upload source exceeds limit');
      const chunk = buffer.subarray(0, bytesRead);
      hash.update(chunk); await writeAll(output, chunk);
    }
    signal.throwIfAborted();
    if (!stable(input.stat, await input.handle.stat(), bytes)) throw new Error('Upload source changed while staging');
    const created_at = Date.now();
    const value = { version: 1, artifact_id: id, name: path.basename(file), bytes, sha256: hash.digest('hex'), created_at, expires_at: created_at + TTL_MS };
    await output.sync(); await output.close(); output = undefined;
    await fs.promises.link(temporary, data); dataOwned = true;
    await fs.promises.unlink(temporary);
    manifestHandle = await fs.promises.open(manifestTemporary, 'wx', 0o600); manifestTemporaryOwned = true;
    await writeAll(manifestHandle, Buffer.from(JSON.stringify(value) + '\n'));
    await manifestHandle.sync(); await manifestHandle.close(); manifestHandle = undefined;
    signal.throwIfAborted();
    metadata(value, id);
    // Synchronous exclusive publication is the commit point. No await or
    // cancellation check may misreport a committed selection as uncommitted.
    fs.linkSync(manifestTemporary, manifest); manifestOwned = true;
    fs.unlinkSync(manifestTemporary);
    committed = true;
    return value;
  } finally {
    for (const handle of [input?.handle, output, manifestHandle]) if (handle) await handle.close().catch(() => {});
    const cleanup = [temporaryOwned && temporary, manifestTemporaryOwned && manifestTemporary,
      !committed && dataOwned && data, !committed && manifestOwned && manifest];
    for (const name of cleanup) if (name) await fs.promises.unlink(name).catch(() => {});
  }
}
async function readStaged(directory, id, signal = new AbortController().signal) {
  signal.throwIfAborted();
  if (typeof id !== 'string' || !ID.test(id)) throw new Error('Expected an opaque upload snapshot ID');
  await checkDirectory(directory);
  let manifest, data;
  try {
    manifest = await regular(path.join(directory, id + '.json'), { privateFile: true, maxBytes: 4096 });
    const raw = await readBounded(manifest.handle, 4096, signal);
    if (raw.length > 4096 || !stable(manifest.stat, await manifest.handle.stat(), raw.length)) throw new Error('Upload metadata changed');
    const value = metadata(JSON.parse(raw.toString('utf8')), id);
    data = await regular(path.join(directory, id + '.data'), { privateFile: true });
    if (data.stat.size !== value.bytes) throw new Error('Upload snapshot size mismatch');
    // Buffer only bounded, already host-selected bytes. The future HTTP sender
    // uses this exact buffer, never reopens a path after integrity validation.
    const body = await readBounded(data.handle, MAX_BYTES, signal);
    const bytes = body.length;
    if (!stable(data.stat, await data.handle.stat(), bytes) || bytes !== value.bytes ||
        createHash('sha256').update(body).digest('hex') !== value.sha256) throw new Error('Upload snapshot integrity mismatch');
    signal.throwIfAborted();
    metadata(value, id); // Expiry may have elapsed during bounded disk I/O.
    return { metadata: value, body };
  } finally { for (const handle of [manifest?.handle, data?.handle]) if (handle) await handle.close().catch(() => {}); }
}
module.exports = { stageFile, readStaged, MAX_BYTES, TTL_MS };
