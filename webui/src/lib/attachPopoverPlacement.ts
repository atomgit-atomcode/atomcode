interface ViewportBounds {
  top: number;
  left: number;
  bottom: number;
  right: number;
}

export function placeAttachPopover(
  anchor: Pick<ViewportBounds, 'top' | 'bottom' | 'left'>,
  viewport: ViewportBounds,
  preferredHeight: number,
) {
  const gap = 6;
  const inset = 8;
  const top = viewport.top + inset;
  const bottom = Math.max(top, viewport.bottom - inset);
  const aboveEdge = Math.max(top, Math.min(bottom, anchor.top - gap));
  const belowEdge = Math.max(top, Math.min(bottom, anchor.bottom + gap));
  const above = aboveEdge - top;
  const below = bottom - belowEdge;
  const opensBelow = above < preferredHeight && below > above;
  const width = Math.max(0, Math.min(340, viewport.right - viewport.left - 2 * inset));
  const left = Math.max(viewport.left + inset, Math.min(anchor.left, viewport.right - inset - width));

  return {
    opensBelow,
    edge: opensBelow ? belowEdge : aboveEdge,
    maxHeight: opensBelow ? below : above,
    left,
    width,
  };
}
