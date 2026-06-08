import { test } from 'node:test';
import assert from 'node:assert/strict';
import { homedir } from 'node:os';
import { expandHome } from '../src/config.js';

test('expandHome 展开前导 ~', () => {
  assert.equal(expandHome('~/.atomcode/x'), `${homedir()}/.atomcode/x`);
  assert.equal(expandHome('/abs/path'), '/abs/path');
});
