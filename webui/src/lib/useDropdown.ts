import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks';
import {
  placeDropdown,
  safeTopInset,
  type DropdownPlacement,
} from './dropdownPlacement';

export interface UseDropdownOptions {
  /** Whether the menu is open. */
  open: boolean;
  /** Called when the user clicks outside the trigger and the menu. */
  onClose: () => void;
  /** Preferred side: 'above' opens upward (composer footer), 'below' downward. */
  prefer?: 'above' | 'below';
  /** The menu's preferred width in CSS pixels. */
  width: number;
}

export interface UseDropdownResult {
  /** Put on the trigger button. */
  triggerRef: (element: HTMLButtonElement | null) => void;
  /** Put on the menu element. */
  menuRef: (element: HTMLDivElement | null) => void;
  /**
   * The inline style for the menu.
   *
   * Before the menu has been measured it is rendered `visibility: hidden` at
   * the viewport's origin: it still has a layout box, so its height can be
   * measured, but it cannot be seen. The chicken-and-egg problem is why the
   * menu is mounted as soon as it opens — mounting it only once a placement
   * exists would leave nothing to measure.
   */
  style: Record<string, string>;
  /** The computed placement, or null until the menu has been measured. */
  placement: DropdownPlacement | null;
}

/**
 * A dropdown that is portaled to `<body>` and placed against the viewport.
 *
 * Two things make this more than a positioning helper:
 *
 * 1. The menu's own height has to be measured before it can be placed. The
 *    menu opens flush with the trigger, so its position depends on how tall
 *    it turned out to be. The menu is rendered hidden for one frame, then
 *    measured, then positioned.
 *
 * 2. Because the menu is portaled, it is no longer a descendant of the
 *    trigger's container — so a click on a menu item is a click "outside"
 *    the trigger. The outside-click handler must know about the menu too, or
 *    the menu closes on mousedown and the item never receives its click.
 */
export function useDropdown(options: UseDropdownOptions): UseDropdownResult {
  const { open, onClose, prefer = 'below', width } = options;
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const [placement, setPlacement] = useState<DropdownPlacement | null>(null);

  // Measure and place. Runs after the menu is in the DOM (layout effect), so
  // its height is known on the first frame the menu is visible.
  useLayoutEffect(() => {
    if (!open) {
      setPlacement(null);
      return;
    }
    const measure = () => {
      const trigger = triggerRef.current;
      const menu = menuRef.current;
      if (!trigger || !menu) return;
      const anchorRect = trigger.getBoundingClientRect();
      const menuRect = menu.getBoundingClientRect();
      setPlacement(
        placeDropdown({
          anchor: {
            top: anchorRect.top,
            bottom: anchorRect.bottom,
            left: anchorRect.left,
            right: anchorRect.right,
            height: anchorRect.height,
          },
          width,
          height: menuRect.height,
          prefer,
          viewportWidth: window.innerWidth,
          viewportHeight: window.innerHeight,
          safeTop: safeTopInset(),
        }),
      );
    };
    measure();
    window.addEventListener('resize', measure);
    window.addEventListener('scroll', measure, true);
    return () => {
      window.removeEventListener('resize', measure);
      window.removeEventListener('scroll', measure, true);
    };
  }, [open, prefer, width]);

  // Outside click. The menu is portaled, so it is checked separately from the
  // trigger — otherwise every menu item would count as an outside click.
  useEffect(() => {
    if (!open) return;
    const handle = (event: MouseEvent) => {
      const target = event.target as Node;
      if (triggerRef.current?.contains(target)) return;
      if (menuRef.current?.contains(target)) return;
      onClose();
    };
    document.addEventListener('mousedown', handle);
    return () => document.removeEventListener('mousedown', handle);
  }, [open, onClose]);

  return {
    triggerRef: (element) => {
      triggerRef.current = element;
    },
    menuRef: (element) => {
      menuRef.current = element;
    },
    style: {
      left: placement ? `${placement.left}px` : '0px',
      top: placement ? `${placement.top}px` : '0px',
      // The width is fixed before measuring, so the measured height is the
      // height the menu will actually have.
      width: `${placement ? placement.width : width}px`,
      visibility: placement ? 'visible' : 'hidden',
      // The cap is only applied once the menu has been measured: leaving it
      // off during the measurement lets the natural height be seen.
      ...(placement ? { maxHeight: `${placement.maxHeight}px` } : {}),
    },
    placement,
  };
}
