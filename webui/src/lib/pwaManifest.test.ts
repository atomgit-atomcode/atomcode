import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();
const indexHtml = readFileSync(join(root, 'index.html'), 'utf8');
const manifest = JSON.parse(
  readFileSync(join(root, 'public/manifest.webmanifest'), 'utf8'),
) as {
  id: string;
  name: string;
  start_url: string;
  scope: string;
  display: string;
  icons: Array<{ src: string; sizes: string; type: string }>;
};

test('webui advertises an installable standalone app manifest', () => {
  assert.match(indexHtml, /<link rel="manifest" href="\.\/manifest\.webmanifest" \/>/);
  assert.match(
    indexHtml,
    /<meta name="theme-color" content="#ffffff" media="\(prefers-color-scheme: light\)" \/>/i,
  );
  assert.match(
    indexHtml,
    /<meta name="theme-color" content="#151517" media="\(prefers-color-scheme: dark\)" \/>/i,
  );

  assert.equal(manifest.id, '/');
  assert.equal(manifest.name, 'AtomCode');
  assert.equal(manifest.start_url, '/');
  assert.equal(manifest.scope, '/');
  assert.equal(manifest.display, 'standalone');
  assert.deepEqual(
    manifest.icons.map(({ sizes }) => sizes),
    ['192x192', '512x512'],
  );
});

test('manifest icon assets exist and use PNG metadata', () => {
  for (const icon of manifest.icons) {
    assert.equal(icon.type, 'image/png');
    const bytes = readFileSync(join(root, 'public', icon.src.slice(1)));
    assert.deepEqual([...bytes.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10]);
    const [declaredWidth, declaredHeight] = icon.sizes.split('x').map(Number);
    assert.equal(bytes.readUInt32BE(16), declaredWidth);
    assert.equal(bytes.readUInt32BE(20), declaredHeight);
  }
});
