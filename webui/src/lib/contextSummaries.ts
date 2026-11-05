/** Absolute event-log counts: reconnect/replay never increments a local counter. */
export function nextContextSummary(previous: number | null, incoming?: number, record = false) {
  const parsed = incoming != null && Number.isSafeInteger(incoming) && incoming >= 0 ? incoming : null;
  const count = parsed == null ? previous : Math.max(previous ?? 0, parsed);
  return { count, notice: record && parsed != null && count != null && count > (previous ?? 0) };
}
