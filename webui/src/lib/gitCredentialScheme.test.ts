// The two decisions behind the credential form's scheme picker.
//
// Worth pinning because both failures are silent: defaulting to `http` would put
// every token on the wire in cleartext, and failing to *mark* an already-stored
// `http` entry would leave it looking exactly like an `https` one.

import test from 'node:test';
import assert from 'node:assert/strict';

import { needsCleartextWarning, schemeFor } from './gitCredentialScheme.ts';

test('a credential is https unless the person opted out of it', () => {
  assert.equal(schemeFor(false), 'https', 'the unticked default must be the safe one');
  assert.equal(schemeFor(true), 'http');
});

test('only a stored http scheme is marked as cleartext', () => {
  assert.equal(needsCleartextWarning('http'), true);
  assert.equal(needsCleartextWarning('https'), false);
});

test('a hand-edited file still reads as cleartext', () => {
  // The value comes off disk, where someone may have typed it by hand: case and
  // stray spacing must not turn an insecure entry into a safe-looking one.
  assert.equal(needsCleartextWarning(' HTTP '), true);
  assert.equal(needsCleartextWarning('Http'), true);
  // An unknown scheme is not a cleartext claim — but it is also not https, and
  // the daemon refuses to store anything else, so this is only about not lying.
  assert.equal(needsCleartextWarning(''), false);
  assert.equal(needsCleartextWarning('ftp'), false);
});