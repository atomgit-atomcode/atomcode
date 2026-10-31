// Where an anchored dropdown may sit, in viewport coordinates.
//
// A dropdown that is anchored to a trigger with plain CSS (`left: 0` /
// `right: 0` inside a `position: relative` parent) cannot know how much room
// the viewport has, so a wide menu next to a narrow trigger overflows the
// screen: the mode pill on a 360px phone is ~86px wide, but the menu needs
// 200px, and anchoring its right edge to the trigger's right edge pushes it
// to x = -93 — off the left edge, where nothing can be tapped.
//
// The fix is to measure the trigger AND the menu, then place the menu against
// the viewport: flush with the trigger on the preferred side, and clamped
// inside the screen.

export interface DropdownPlacement {
  /** Distance from the viewport's left edge. */
  left: number;
  /** Distance from the viewport's top edge. */
  top: number;
  /** The menu's width, in CSS pixels. */
  width: number;
  /** How tall the menu may be before it has to scroll. */
  maxHeight: number;
  /** Which side of the trigger the menu ended up on. */
  side: 'above' | 'below';
}

export interface DropdownRect {
  top: number;
  bottom: number;
  left: number;
  right: number;
  height: number;
}

export interface DropdownOptions {
  /** The trigger's rectangle, in viewport coordinates. */
  anchor: DropdownRect;
  /** The menu's preferred width. */
  width: number;
  /** The menu's measured natural height. */
  height: number;
  /** Space to keep between the menu and the viewport edges. */
  margin?: number;
  /** Space to keep between the menu and the trigger. */
  gap?: number;
  /** Preferred side: 'above' opens upward (the composer footer), 'below' downward. */
  prefer?: 'above' | 'below';
  /** Viewport size. */
  viewportWidth: number;
  viewportHeight: number;
  /**
   * The safe-area inset at the top of the viewport (the status bar, a notch).
   * A menu that opens upward from a trigger near the top of the screen would
   * otherwise slide under the status bar.
   */
  safeTop?: number;
}

const DEFAULT_MARGIN = 8;
const DEFAULT_GAP = 4;

/**
 * The safe-area inset at the top of the viewport, in CSS pixels.
 *
 * Android draws every app edge-to-edge since API 35, so the WebView extends
 * under the status bar; `env(safe-area-inset-top)` reports how far. A menu
 * that opens upward from a trigger near the top of the screen has to respect
 * it, or it ends up behind the clock.
 */
export function safeTopInset(): number {
  if (typeof document === 'undefined') return 0;
  const probe = document.createElement('div');
  probe.style.cssText =
    'position:fixed;top:0;left:0;visibility:hidden;padding-top:env(safe-area-inset-top,0px)';
  document.body.appendChild(probe);
  const value = parseFloat(getComputedStyle(probe).paddingTop) || 0;
  probe.remove();
  return value;
}

/**
 * Place a dropdown so it sits flush with its trigger and stays on screen.
 *
 * The menu opens on the preferred side when its measured height fits there,
 * and flips to the other side when it does not — a dropdown clipped by the
 * screen edge is unusable, one that opens the "wrong" way is merely
 * unexpected. When neither side fits, the roomier side wins and the menu
 * scrolls.
 *
 * The menu's edge is flush with the trigger: above, its bottom sits `gap`
 * pixels above the trigger's top; below, its top sits `gap` pixels below the
 * trigger's bottom.
 */
export function placeDropdown(options: DropdownOptions): DropdownPlacement {
  const margin = options.margin ?? DEFAULT_MARGIN;
  const gap = options.gap ?? DEFAULT_GAP;
  const prefer = options.prefer ?? 'below';
  const { anchor, viewportWidth, viewportHeight } = options;
  // The top edge of the usable area: below the status bar / notch.
  const topEdge = Math.max(margin, options.safeTop ?? 0);

  // Horizontal: start at the trigger's left edge, then clamp into the viewport.
  // A menu narrower than the trigger keeps its left edge aligned with it; a
  // wider one grows to the right but never past the viewport.
  const width = Math.min(options.width, Math.max(0, viewportWidth - margin * 2));
  let left = anchor.left;
  if (left + width > viewportWidth - margin) {
    left = viewportWidth - margin - width;
  }
  if (left < margin) left = margin;

  // Vertical: the room on each side of the trigger.
  const height = Math.max(0, options.height);
  const roomAbove = anchor.top - gap - topEdge;
  const roomBelow = viewportHeight - anchor.bottom - gap - margin;

  const fitsAbove = height <= roomAbove;
  const fitsBelow = height <= roomBelow;
  let side: 'above' | 'below';
  if (prefer === 'above') {
    if (fitsAbove) side = 'above';
    else if (fitsBelow) side = 'below';
    else side = roomAbove >= roomBelow ? 'above' : 'below';
  } else if (fitsBelow) {
    side = 'below';
  } else if (fitsAbove) {
    side = 'above';
  } else {
    side = roomBelow >= roomAbove ? 'below' : 'above';
  }

  // The menu never grows past the room it opens into; when it does not fit it
  // scrolls instead of being cut off by the screen edge.
  const room = side === 'above' ? roomAbove : roomBelow;
  const rendered = Math.min(height, Math.max(0, room));

  const top =
    side === 'above'
      ? anchor.top - gap - rendered
      : anchor.bottom + gap;

  return { left, top, width, maxHeight: rendered, side };
}
