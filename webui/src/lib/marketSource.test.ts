import { test } from 'node:test';
import assert from 'node:assert';
import { marketInstallSource } from './marketSource.ts';

const plugin = (overrides: Record<string, unknown> = {}) => ({
  name: 'numpy',
  title: 'NumPy',
  summary: '',
  category: 'python',
  tags: [],
  version: '1.26.2',
  size_bytes: 1,
  sha256: 'ec1da8d0',
  installed: false,
  ...overrides,
});

test('a local bundle is preferred over the URL', () => {
  // The daemon can read the file directly; going back out through its own
  // loopback server would be a pointless round-trip.
  const source = marketInstallSource(plugin({
    download_path: '/sdcard/Download/atomcode-plugins/packages/numpy-1.26.2-arm64.tar.gz',
    download_url: 'https://example.test/packages/numpy-1.26.2-arm64.tar.gz',
  }) as never);
  assert.deepEqual(source, {
    path: '/sdcard/Download/atomcode-plugins/packages/numpy-1.26.2-arm64.tar.gz',
    sha256: 'ec1da8d0',
  });
});

test('a remote market installs from its URL', () => {
  const source = marketInstallSource(plugin({
    download_url: 'https://example.test/packages/numpy-1.26.2-arm64.tar.gz',
  }) as never);
  assert.deepEqual(source, {
    url: 'https://example.test/packages/numpy-1.26.2-arm64.tar.gz',
    sha256: 'ec1da8d0',
  });
});

test('an entry without a hash is refused, not installed unverified', () => {
  assert.equal(
    marketInstallSource(plugin({
      sha256: '',
      download_path: '/sdcard/whatever.tar.gz',
    }) as never),
    null,
  );
});

test('an entry with neither path nor url is refused', () => {
  assert.equal(marketInstallSource(plugin() as never), null);
});
