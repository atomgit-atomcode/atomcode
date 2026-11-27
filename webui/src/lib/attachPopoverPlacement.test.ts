import assert from 'node:assert/strict';
import test from 'node:test';
import { placeAttachPopover } from './attachPopoverPlacement.ts';

test('attachment popover prefers the space above when the menu fits', () => {
  assert.deepEqual(placeAttachPopover({ left: 20, top: 440, bottom: 472 }, { left: 0, top: 0, right: 1000, bottom: 1000 }, 400), {
    opensBelow: false, edge: 434, maxHeight: 426, left: 20, width: 340,
  });
});

test('attachment popover flips down and clips the menu to the visible viewport', () => {
  assert.deepEqual(placeAttachPopover({ left: 310, top: 120, bottom: 152 }, { left: 45, top: 80, right: 365, bottom: 420 }, 400), {
    opensBelow: true, edge: 158, maxHeight: 254, left: 53, width: 304,
  });
});

test('attachment popover uses the larger side when neither fits', () => {
  const placement = placeAttachPopover({ left: 24, top: 159, bottom: 191 }, { left: 0, top: 0, right: 400, bottom: 280 }, 400);
  assert.equal(placement.opensBelow, false);
  assert.equal(placement.maxHeight, 145);
});
