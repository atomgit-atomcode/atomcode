import assert from 'node:assert/strict';
import { parseOpenFileSelection } from '../../src/chat/filePosition';

assert.deepEqual(
  parseOpenFileSelection({ startLine: 42, startColumn: 8 }),
  { startLine: 42, startColumn: 8, endLine: 42, endColumn: 8 },
);

assert.deepEqual(
  parseOpenFileSelection({ startLine: 3, endLine: 5 }),
  { startLine: 3, startColumn: 1, endLine: 5, endColumn: 1 },
);

assert.equal(parseOpenFileSelection({}), undefined);
assert.equal(parseOpenFileSelection({ startLine: 0 }), undefined);
assert.equal(parseOpenFileSelection({ startLine: '4' }), undefined);
assert.equal(parseOpenFileSelection({ startLine: 4, startColumn: -1 }), undefined);
assert.equal(parseOpenFileSelection({ startLine: 5, endLine: 4 }), undefined);
assert.equal(
  parseOpenFileSelection({ startLine: 5, startColumn: 8, endLine: 5, endColumn: 7 }),
  undefined,
);
