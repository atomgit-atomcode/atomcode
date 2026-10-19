/** A shared list with explicit refresh/invalidation, never a permanent empty-on-error cache. */
export function createRefreshingList<T>(load: () => Promise<T[]>) {
  let snapshot: { items: T[] | null; loading: boolean; failed: boolean } = {
    items: null, loading: false, failed: false,
  };
  const listeners = new Set<(value: typeof snapshot) => void>();
  let generation = 0;
  let pending: Promise<void> | null = null;
  const publish = (value: typeof snapshot) => {
    snapshot = value;
    listeners.forEach((listener) => listener(snapshot));
  };
  const refresh = (fresh = false): Promise<void> => {
    if (pending && !fresh) return pending;
    const current = ++generation;
    publish({ ...snapshot, loading: true, failed: false });
    pending = Promise.resolve().then(load).then((items) => {
      if (current === generation) publish({ items, loading: false, failed: false });
    }).catch(() => {
      if (current === generation) publish({ ...snapshot, loading: false, failed: true });
    }).finally(() => {
      if (current === generation) pending = null;
    });
    return pending;
  };
  return {
    getSnapshot: () => snapshot,
    subscribe(listener: (value: typeof snapshot) => void) {
      listeners.add(listener);
      listener(snapshot);
      return () => { listeners.delete(listener); };
    },
    refresh,
    invalidate() {
      ++generation;
      pending = null;
      publish({ items: null, loading: false, failed: false });
      if (listeners.size) void refresh();
    },
  };
}
