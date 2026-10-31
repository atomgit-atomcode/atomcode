import { test } from 'node:test';
import assert from 'node:assert/strict';
import { placeDropdown } from './dropdownPlacement.ts';

const PHONE = { viewportWidth: 360, viewportHeight: 785 };
const GAP = 4;

/** The trigger geometry of the composer footer on a 360px phone. */
const COMPOSER_TRIGGER = { top: 664, bottom: 708, left: 21, right: 107, height: 44 };

test('a menu opens flush above its trigger', () => {
  // The bug this pins down: the menu used to be placed with the ROOM it had
  // instead of its own height, so it floated up to the top of the screen
  // (y = 32) instead of sitting on the trigger (y = 664).
  const placement = placeDropdown({
    ...PHONE,
    anchor: COMPOSER_TRIGGER,
    width: 240,
    height: 215,
    prefer: 'above',
  });
  assert.equal(placement.side, 'above');
  assert.equal(
    placement.top + placement.maxHeight,
    COMPOSER_TRIGGER.top - GAP,
    `the menu's bottom edge must touch the trigger's top edge`,
  );
  assert.ok(placement.top < COMPOSER_TRIGGER.top, 'the menu must be above');
});

test('a menu opens flush below its trigger', () => {
  const placement = placeDropdown({
    ...PHONE,
    anchor: { top: 100, bottom: 140, left: 20, right: 120, height: 40 },
    width: 240,
    height: 215,
    prefer: 'below',
  });
  assert.equal(placement.side, 'below');
  assert.equal(placement.top, 140 + GAP);
});

test('a wide menu next to a narrow trigger stays inside the viewport', () => {
  // The real case from a 360px phone: the mode pill is ~86px wide at the far
  // left of the composer footer, and the menu needs 200px. Anchoring the
  // menu's right edge to the pill's right edge put it at x = -93.
  const placement = placeDropdown({
    ...PHONE,
    anchor: COMPOSER_TRIGGER,
    width: 240,
    height: 215,
    prefer: 'above',
  });
  assert.ok(placement.left >= 8, `left must stay on screen: ${placement.left}`);
  assert.ok(
    placement.left + placement.width <= PHONE.viewportWidth - 8 + 0.001,
    `menu must not cross the right edge: ${placement.left + placement.width}`,
  );
});

test('a menu near the right edge grows leftward instead of off screen', () => {
  const placement = placeDropdown({
    ...PHONE,
    anchor: { top: 664, bottom: 708, left: 274, right: 340, height: 44 },
    width: 240,
    height: 215,
    prefer: 'above',
  });
  assert.ok(placement.left >= 8);
  assert.ok(placement.left + placement.width <= PHONE.viewportWidth - 8 + 0.001);
});

test('a menu that does not fit above opens below instead', () => {
  // Trigger hugging the top of the screen: no room above, plenty below.
  const placement = placeDropdown({
    ...PHONE,
    anchor: { top: 4, bottom: 40, left: 20, right: 120, height: 36 },
    width: 240,
    height: 215,
    prefer: 'above',
  });
  assert.equal(placement.side, 'below');
  assert.equal(placement.top, 40 + GAP);
});

test('a menu that does not fit below opens above instead', () => {
  const placement = placeDropdown({
    ...PHONE,
    anchor: { top: 740, bottom: 780, left: 20, right: 120, height: 40 },
    width: 240,
    height: 215,
    prefer: 'below',
  });
  assert.equal(placement.side, 'above');
  assert.ok(placement.top >= 8);
});

test('the menu never gets taller than the space it opens into', () => {
  const placement = placeDropdown({
    ...PHONE,
    anchor: { top: 400, bottom: 440, left: 20, right: 120, height: 40 },
    width: 240,
    height: 600,
    prefer: 'above',
  });
  assert.ok(placement.maxHeight <= 400 - 8 - GAP);
  // It still touches the trigger, and scrolls instead of being cut off.
  assert.equal(placement.top + placement.maxHeight, 400 - GAP);
});

test('a menu wider than the viewport is clamped to the viewport width', () => {
  const placement = placeDropdown({
    ...PHONE,
    anchor: { top: 400, bottom: 440, left: 20, right: 120, height: 40 },
    width: 900,
    height: 200,
    prefer: 'above',
  });
  assert.ok(placement.left >= 8);
  assert.ok(placement.left <= PHONE.viewportWidth - 8);
  assert.ok(placement.width <= PHONE.viewportWidth - 16);
});

test('a menu never slides under the status bar', () => {
  // A trigger just below the status bar, with the menu opening upward: the
  // menu must start below the inset, not behind the clock.
  const placement = placeDropdown({
    ...PHONE,
    anchor: { top: 36, bottom: 70, left: 20, right: 120, height: 34 },
    width: 240,
    height: 100,
    prefer: 'above',
    safeTop: 32,
  });
  assert.ok(placement.top >= 32, `must clear the status bar: ${placement.top}`);
});
