import { test } from 'node:test';
import assert from 'node:assert/strict';
import { headerBody, headerProblem } from './providerHeaders.ts';
test('stored values stay on the server, removed rows delete, and blanks do not create headers', () => {
  assert.deepEqual(headerBody([{ name: 'X-Token', value: '', stored: true }, { name: '', value: '' }]), { 'x-token': null });
  assert.deepEqual(headerBody([]), {});
  assert.deepEqual(headerBody([{ name: 'X-Token', value: 'new', stored: true }]), { 'x-token': 'new' });
});
test('invalid names, duplicates, controls and transport routing headers are rejected', () => {
  for (const rows of [
    [{ name: 'Bad Name', value: 'x' }], [{ name: 'X-Key', value: 'a\r\nb' }],
    [{ name: 'Host', value: 'other' }], [{ name: 'X-Key', value: 'a' }, { name: 'x-key', value: 'b' }],
  ]) assert.equal(headerProblem(rows), true);
  assert.equal(headerProblem([{ name: 'X-Key', value: 'a' }]), false);
});
