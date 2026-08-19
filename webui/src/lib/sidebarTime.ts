export type SidebarTimeUnit = 'now' | 'minutes' | 'hours' | 'days' | 'months' | 'years';

export interface SidebarRelativeTime {
  unit: SidebarTimeUnit;
  n: number;
}

/** Compact relative-time bucket used by workspace session rows. */
export function sidebarRelativeTime(timestamp: number, now = Date.now()): SidebarRelativeTime {
  const ms = timestamp < 1e12 ? timestamp * 1000 : timestamp;
  const diff = Math.max(0, now - ms);
  const MIN = 60_000;
  const HOUR = 3_600_000;
  const DAY = 86_400_000;
  if (diff < MIN) return { unit: 'now', n: 0 };
  if (diff < HOUR) return { unit: 'minutes', n: Math.floor(diff / MIN) };
  if (diff < DAY) return { unit: 'hours', n: Math.floor(diff / HOUR) };
  if (diff < 30 * DAY) return { unit: 'days', n: Math.floor(diff / DAY) };
  if (diff < 365 * DAY) return { unit: 'months', n: Math.floor(diff / (30 * DAY)) };
  return { unit: 'years', n: Math.floor(diff / (365 * DAY)) };
}
