/** Navigation never transfers ownership of an executing conversation, nor
 *  takes a session another runtime (a TUI in another terminal) holds: that one
 *  can only be watched, or forked. */
export function sessionSelectionDisposition(bound: string | null, selected: string, executing: boolean, browsing: boolean, heldElsewhere = false): 'return' | 'browse' | 'activate' {
  if (bound === selected) return 'return';
  return executing || browsing || heldElsewhere ? 'browse' : 'activate';
}

/** Serialize refreshes and discard results after navigation or while history is
 * being read. A skipped/failed refresh never acknowledges the pending update. */
export function createBrowseRefresh<T>(options: {
  needed: () => boolean;
  ready: () => boolean;
  read: (signal: AbortSignal) => Promise<T>;
  apply: (value: T) => void;
}) {
  const controller = new AbortController();
  let inFlight = false;
  return {
    async run() {
      if (controller.signal.aborted || inFlight || !options.needed() || !options.ready()) return;
      inFlight = true;
      try {
        const value = await options.read(controller.signal);
        if (!controller.signal.aborted && options.ready()) options.apply(value);
      } catch { /* Keep the displayed page and retry the pending update. */ }
      finally { inFlight = false; }
    },
    stop() { controller.abort(); },
  };
}
