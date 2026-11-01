// The Android host bridge, exercised without a phone.
//
// Everything here is a mapping over `window.AtomCodeHost`, and each mapping has a
// silent failure mode worth pinning: a malformed share payload must not throw into
// Preact's render, a throwing bridge must read as "this page cannot know" rather
// than as a real answer, and `openWithHost` must say `false` — not throw — so the
// caller can fall back to the daemon.

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  gatewaySigningAvailable,
  hasAndroidHost,
  openWithHost,
  takeShares,
} from './androidHost.ts';

/** Install a fake bridge for one assertion, then remove it. */
function withBridge<T>(bridge: Record<string, unknown> | null, run: () => T): T {
  const globals = globalThis as Record<string, unknown>;
  const previous = globals.window;
  globals.window = bridge === null ? undefined : { AtomCodeHost: bridge };
  try {
    return run();
  } finally {
    globals.window = previous;
  }
}

test('a page outside the app has no host answers at all', () => {
  withBridge(null, () => {
    assert.equal(hasAndroidHost(), false);
    assert.equal(gatewaySigningAvailable(), null);
    assert.equal(takeShares(), null);
    assert.equal(openWithHost('/anything'), false);
  });
});

test('the bridge object alone is enough to be in the app', () => {
  withBridge({}, () => {
    assert.equal(hasAndroidHost(), true);
    // An older app without the newer methods must not look like a "no" answer.
    assert.equal(gatewaySigningAvailable(), null);
    assert.equal(openWithHost('/anything'), false);
  });
});

test('gateway signing is reported as unavailable, available, or unknown', () => {
  withBridge({ gatewaySigning: () => false }, () => {
    assert.equal(gatewaySigningAvailable(), false);
  });
  withBridge({ gatewaySigning: () => true }, () => {
    assert.equal(gatewaySigningAvailable(), true);
  });
  withBridge(
    {
      gatewaySigning: () => {
        throw new Error('bridge gone');
      },
    },
    () => {
      // Unknown, so the page does not claim the build is broken.
      assert.equal(gatewaySigningAvailable(), null);
    },
  );
});

test('a share payload is parsed, and a broken one is not an exception', () => {
  const payload = JSON.stringify({ paths: ['/cache/shares/a.png', 7], text: 'look' });
  withBridge({ takeShares: () => payload }, () => {
    assert.deepEqual(takeShares(), { paths: ['/cache/shares/a.png'], text: 'look' });
  });
  withBridge({ takeShares: () => 'not json' }, () => {
    assert.equal(takeShares(), null);
  });
  withBridge({ takeShares: () => JSON.stringify({ paths: 'nope' }) }, () => {
    assert.deepEqual(takeShares(), { paths: [], text: '' });
  });
});

test('opening a path reports whether the host took it', () => {
  withBridge({ openPath: () => true }, () => {
    assert.equal(openWithHost('/workspace/report.md'), true);
  });
  withBridge({ openPath: () => false }, () => {
    assert.equal(openWithHost('/outside'), false);
  });
  withBridge(
    {
      openPath: () => {
        throw new Error('no viewer');
      },
    },
    () => {
      // The chip then asks the daemon, which at worst names the path.
      assert.equal(openWithHost('/workspace/report.md'), false);
    },
  );
});
