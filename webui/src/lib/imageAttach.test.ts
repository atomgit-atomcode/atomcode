// Covers the attachment planner that replaced the old "reject anything over 2 MB"
// gate. The phone case that motivated it: a 4000×3000 JPEG of 6 MB, which used to
// be dropped silently and must now be fitted to 1568 px before upload.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  MAX_EDGE,
  MAX_RAW_BYTES,
  MAX_UPLOAD_BYTES,
  QUALITY_LADDER,
  encodedType,
  fitWithin,
  isDecodable,
  planUpload,
} from './imageAttach.ts';

test('an image already within the edge cap keeps its size', () => {
  assert.deepEqual(fitWithin(800, 600), { width: 800, height: 600 });
  assert.deepEqual(fitWithin(MAX_EDGE, 10), { width: MAX_EDGE, height: 10 });
});

test('a phone photo is fitted to the longest edge with the aspect ratio kept', () => {
  const photo = fitWithin(4000, 3000);
  assert.equal(Math.max(photo.width, photo.height), MAX_EDGE);
  // 4:3 stays 4:3 (within rounding).
  assert.ok(Math.abs(photo.width / photo.height - 4000 / 3000) < 0.01, `${photo.width}x${photo.height}`);

  const portrait = fitWithin(3000, 4000);
  assert.equal(portrait.height, MAX_EDGE);
  assert.ok(portrait.width < portrait.height);
});

test('an extreme panorama never collapses to zero pixels', () => {
  const pano = fitWithin(20000, 3);
  assert.equal(pano.width, MAX_EDGE);
  assert.equal(pano.height, 1);
});

test('the upload plan only re-encodes when a budget is exceeded', () => {
  // Small in both bytes and pixels: send the original bytes untouched.
  assert.equal(planUpload(800, 600, 300_000), null);
  // 3–8 MB camera photo: the case that used to be rejected outright.
  assert.deepEqual(planUpload(4000, 3000, 6 * 1024 * 1024), { width: MAX_EDGE, height: 1176 });
  // Few pixels but a huge payload (e.g. an uncompressed bitmap) still gets re-encoded.
  assert.deepEqual(planUpload(1000, 1000, MAX_UPLOAD_BYTES + 1), { width: 1000, height: 1000 });
});

test('a file that is not worth decoding is refused up front', () => {
  assert.equal(isDecodable({ type: 'image/jpeg', size: 6 * 1024 * 1024 }), true);
  assert.equal(isDecodable({ type: 'application/pdf', size: 1024 }), false);
  assert.equal(isDecodable({ type: 'image/png', size: 0 }), false);
  assert.equal(isDecodable({ type: 'image/png', size: MAX_RAW_BYTES + 1 }), false);
});

test('only a small PNG-like image stays lossless; a photograph becomes JPEG', () => {
  assert.equal(encodedType('image/png', 800 * 600), 'image/png');
  assert.equal(encodedType('image/gif', 500 * 500), 'image/png');
  // A 1568-px photo-sized PNG would blow the budget as PNG.
  assert.equal(encodedType('image/png', MAX_EDGE * MAX_EDGE), 'image/jpeg');
  assert.equal(encodedType('image/jpeg', 800 * 600), 'image/jpeg');
  assert.equal(encodedType('image/heic', 800 * 600), 'image/jpeg');
});

test('the quality ladder steps down', () => {
  assert.ok(QUALITY_LADDER.length >= 2);
  for (let i = 1; i < QUALITY_LADDER.length; i += 1) {
    assert.ok(QUALITY_LADDER[i]! < QUALITY_LADDER[i - 1]!, 'qualities must descend');
  }
});
