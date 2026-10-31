// Paging math for the plugin market, kept apart from the component so it can be
// asserted directly. The interesting part is not the slice — it is what happens
// when a filter or search shrinks the list while the person is on page 3: the
// view must land on a page that still exists instead of showing nothing.

/** How many pages a list of `total` items needs. Always at least one: an empty
 *  result still shows "第 1 / 1 页" rather than no pager at all. */
export function pageCount(total: number, size: number): number {
  if (size <= 0) return 1;
  return Math.max(1, Math.ceil(Math.max(0, total) / size));
}

/** `page` clamped into range. */
export function clampPage(page: number, total: number, size: number): number {
  const last = pageCount(total, size) - 1;
  if (!Number.isFinite(page)) return 0;
  return Math.min(Math.max(0, Math.floor(page)), last);
}

/** The items on `page`, clamped the same way. */
export function pageSlice<T>(items: T[], page: number, size: number): T[] {
  if (size <= 0) return [];
  const current = clampPage(page, items.length, size);
  const start = current * size;
  return items.slice(start, start + size);
}
