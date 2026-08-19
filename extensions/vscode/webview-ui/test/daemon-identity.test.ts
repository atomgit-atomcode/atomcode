import assert from 'node:assert/strict';
import { daemonIdentityMatches } from '../../src/daemon/identity';

const expected = {
  version: '5.0.7',
  binaryHash: 'bundled-build-sha256',
};

assert.equal(
  daemonIdentityMatches(
    { version: '5.0.7', binary_hash: 'bundled-build-sha256' },
    expected,
  ),
  true,
);

// Regression: provider-schema changes shipped without a Cargo version bump.
// Version-only matching reused the pre-fix daemon, so /login wrote the new
// model catalog while /chat failed with "Provider 'AtomGit-…' not found".
assert.equal(
  daemonIdentityMatches(
    { version: '5.0.3', binary_hash: 'older-build-sha256' },
    expected,
  ),
  false,
);

assert.equal(
  daemonIdentityMatches({ version: '5.0.3' }, expected),
  false,
  'a daemon that cannot prove the expected bundled hash must be replaced',
);

assert.equal(
  daemonIdentityMatches(
    { version: '5.0.2', binary_hash: 'bundled-build-sha256' },
    expected,
  ),
  false,
);

assert.equal(
  daemonIdentityMatches({ version: 'custom-build' }, {}),
  true,
  'custom/external daemon selection has no bundled identity expectation',
);
