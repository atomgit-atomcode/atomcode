'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { createHash, randomUUID } = require('node:crypto');
const { spawnSync } = require('node:child_process');
const { stageFile, readStaged, MAX_BYTES, TTL_MS } = require('./upload-store.cjs');
function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'atomcode-upload-store-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return { root, directory: path.join(root, 'uploads'), file: path.join(root, 'selected.txt') };
}
test('explicit selection snapshots exact bytes, private files and an opaque metadata-only identity', async t => {
  const f = fixture(t), content = Buffer.from('UPLOAD_FIXTURE\0binary\xff', 'latin1');
  fs.writeFileSync(f.file, content);
  const selected = await stageFile(f.directory, f.file);
  assert.equal(selected.bytes, content.length);
  assert.equal(selected.sha256, createHash('sha256').update(content).digest('hex'));
  assert.equal(JSON.stringify(selected).includes(f.root), false);
  assert.deepEqual(fs.readdirSync(f.directory).sort(), [selected.artifact_id + '.data', selected.artifact_id + '.json'].sort());
  for (const entry of fs.readdirSync(f.directory)) {
    const s = fs.statSync(path.join(f.directory, entry)); assert.equal(s.nlink, 1);
    if (process.platform !== 'win32') assert.equal(s.mode & 0o077, 0);
  }
  fs.writeFileSync(f.file, 'SOURCE_CHANGED_AFTER_SELECTION');
  const snapshot = await readStaged(f.directory, selected.artifact_id);
  assert.deepEqual(snapshot.body, content); assert.deepEqual(snapshot.metadata, selected);
  // Later disk changes cannot alter the already verified body buffer.
  fs.writeFileSync(path.join(f.directory, selected.artifact_id + '.data'), Buffer.alloc(content.length));
  assert.deepEqual(snapshot.body, content);
  await assert.rejects(readStaged(f.directory, selected.artifact_id), /integrity/);
});
test('pre-cancelled selection does not create a store or snapshot', async t => {
  const f = fixture(t), controller = new AbortController(); fs.writeFileSync(f.file, 'fixture'); controller.abort();
  await assert.rejects(stageFile(f.directory, f.file, controller.signal));
  assert.equal(fs.existsSync(f.directory), false);
});
test('directories, final symlinks, FIFOs and oversized sources are refused without publishing a handle', async t => {
  const f = fixture(t); fs.mkdirSync(f.directory, { mode: 0o700 });
  await assert.rejects(stageFile(f.directory, f.root));
  fs.writeFileSync(f.file, 'fixture');
  const link = path.join(f.root, 'link'); fs.symlinkSync(f.file, link);
  await assert.rejects(stageFile(f.directory, link));
  if (process.platform !== 'win32') {
    const pipe = path.join(f.root, 'pipe'); assert.equal(spawnSync('mkfifo', [pipe]).status, 0);
    await assert.rejects(stageFile(f.directory, pipe));
  }
  fs.truncateSync(f.file, MAX_BYTES + 1);
  await assert.rejects(stageFile(f.directory, f.file));
  assert.deepEqual(fs.readdirSync(f.directory), []);
});
test('IDs are opaque, path traversal and unknown identities never select local paths', async t => {
  const f = fixture(t); fs.mkdirSync(f.directory, { mode: 0o700 });
  for (const id of ['../selected.txt', f.file, 'https://example.test/', '', randomUUID()]) await assert.rejects(readStaged(f.directory, id));
});
test('metadata tampering, oversized manifests and linked private files are refused', async t => {
  const f = fixture(t); fs.writeFileSync(f.file, 'fixture'); const selected = await stageFile(f.directory, f.file);
  const manifest = path.join(f.directory, selected.artifact_id + '.json');
  for (const value of [{ ...selected, bytes: -1 }, { ...selected, artifact_id: randomUUID() }, { ...selected, extra: true }, { ...selected, name: 'bad\r\nheader' }]) {
    fs.writeFileSync(manifest, JSON.stringify(value)); await assert.rejects(readStaged(f.directory, selected.artifact_id));
  }
  fs.writeFileSync(manifest, 'x'.repeat(4097)); await assert.rejects(readStaged(f.directory, selected.artifact_id));
  fs.writeFileSync(manifest, JSON.stringify(selected));
  fs.linkSync(manifest, path.join(f.root, 'unexpected-link')); await assert.rejects(readStaged(f.directory, selected.artifact_id));
});
test('a symlink store or nonprivate store is not accepted as an artifact boundary', async t => {
  const f = fixture(t); fs.mkdirSync(f.directory, { mode: 0o700 });
  const link = path.join(f.root, 'linked-store'); fs.symlinkSync(f.directory, link);
  fs.writeFileSync(f.file, 'fixture'); await assert.rejects(stageFile(link, f.file));
  if (process.platform !== 'win32') {
    fs.chmodSync(f.directory, 0o755); await assert.rejects(stageFile(f.directory, f.file));
  }
});
test('explicit host command requires absolute selection and does not print source bytes', async t => {
  const f = fixture(t); fs.writeFileSync(f.file, 'PRIVATE_FIXTURE_BYTES');
  const command = path.join(__dirname, 'stage-upload.cjs');
  const result = spawnSync(process.execPath, [command, '--output-dir', path.join(f.root, 'owned-output'), '--file', f.file], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr); assert.equal(result.stderr, '');
  const value = JSON.parse(result.stdout); assert.equal(value.bytes, 21);
  assert.equal(result.stdout.includes('PRIVATE_FIXTURE_BYTES'), false);
  assert.equal(result.stdout.includes(f.root), false);
  const refused = spawnSync(process.execPath, [command, '--output-dir', path.join(f.root, 'other-output'), '--file', 'relative'], { encoding: 'utf8' });
  assert.equal(refused.status, 1); assert.equal(refused.stdout, '');
  assert.equal(fs.existsSync(path.join(f.root, 'other-output')), false);
});

test('cancellation during observed staging removes incomplete bytes and publishes no selection', async t => {
  const f = fixture(t); fs.mkdirSync(f.directory, { mode: 0o700 });
  fs.writeFileSync(f.file, Buffer.alloc(8 * 1024 * 1024, 0x41));
  const controller = new AbortController();
  const watch = fs.watch(f.directory, (_event, name) => {
    if (String(name).endsWith('.data.partial')) controller.abort();
  });
  t.after(() => watch.close());
  await assert.rejects(stageFile(f.directory, f.file, controller.signal));
  assert.equal(controller.signal.aborted, true);
  assert.deepEqual(fs.readdirSync(f.directory), []);
});
test('empty and Unicode-named explicit files retain their selected names and exact digest', async t => {
  const f = fixture(t), file = path.join(f.root, '选择 "fixture".txt'); fs.writeFileSync(file, '');
  const selected = await stageFile(f.directory, file);
  assert.equal(selected.name, path.basename(file)); assert.equal(selected.bytes, 0);
  const staged = await readStaged(f.directory, selected.artifact_id); assert.equal(staged.body.length, 0);
  assert.equal(staged.metadata.sha256, createHash('sha256').update('').digest('hex'));
});

test('expired selections fail closed even when their bytes and digest remain valid', async t => {
  const f = fixture(t); fs.writeFileSync(f.file, 'fixture'); const selected = await stageFile(f.directory, f.file);
  const manifest = path.join(f.directory, selected.artifact_id + '.json');
  const expires_at = Date.now() - 1;
  fs.writeFileSync(manifest, JSON.stringify({ ...selected, created_at: expires_at - TTL_MS, expires_at }));
  await assert.rejects(readStaged(f.directory, selected.artifact_id));
});
