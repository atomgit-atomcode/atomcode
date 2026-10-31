import { test } from 'node:test';
import assert from 'node:assert';
import { clampPage, pageCount, pageSlice } from './marketPaging.ts';

const range = (n: number) => Array.from({ length: n }, (_, index) => index);

test('an empty market still has one page', () => {
  // Showing no pager at all would leave "1 / 1" unexplained; one page is honest.
  assert.equal(pageCount(0, 12), 1);
  assert.deepEqual(pageSlice([], 0, 12), []);
});

test('exactly one page needs no pager and clamps a stray page number', () => {
  assert.equal(pageCount(12, 12), 1);
  // 请求第 2 页时被夹回唯一的那一页，返回整页而不是空数组——空数组会让人
  // 以为“这里什么都没有”。
  assert.equal(pageSlice(range(12), 1, 12).length, 12);
});

test('twenty items make two pages of twelve and eight', () => {
  assert.equal(pageCount(20, 12), 2);
  assert.equal(pageSlice(range(20), 0, 12).length, 12);
  assert.equal(pageSlice(range(20), 1, 12).length, 8);
  assert.deepEqual(pageSlice(range(20), 1, 12), [12, 13, 14, 15, 16, 17, 18, 19]);
});

test('a shrinking filter pulls the view back into range', () => {
  // Standing on page 3, then searching down to 6 results: landing on page 3
  // would render an empty page with no way back.
  assert.equal(clampPage(2, 6, 12), 0);
  assert.equal(pageSlice(range(6), 2, 12).length, 6);
});

test('odd page input is clamped instead of trusted', () => {
  assert.equal(clampPage(-4, 20, 12), 0);
  assert.equal(clampPage(99, 20, 12), 1);
  assert.equal(clampPage(Number.NaN, 20, 12), 0);
});

test('a non-positive page size cannot loop forever', () => {
  assert.equal(pageCount(20, 0), 1);
  assert.deepEqual(pageSlice(range(20), 0, 0), []);
});
