import assert from 'node:assert/strict';
import { classifyAuthDisplayState } from '../../src/auth/status';

assert.equal(
  classifyAuthDisplayState({ logged_in: true, expired: true }),
  'expired',
);
assert.equal(
  classifyAuthDisplayState({ logged_in: true, expired: false }),
  'signed_in',
);
assert.equal(
  classifyAuthDisplayState({ logged_in: false, expired: false }),
  'signed_out',
);
assert.equal(classifyAuthDisplayState(undefined), 'signed_out');
