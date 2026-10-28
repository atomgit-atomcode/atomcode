import type { WebuiAccess } from './compat.ts';

/** Rechecks protected access only. It never retries a user's mutation or changes credentials. */
export function createAccessRecovery(options: {
  probe: () => Promise<WebuiAccess>;
  onResult: (result: WebuiAccess) => void;
  onChecking?: (checking: boolean) => void;
  schedule?: (run: () => void, delay: number) => ReturnType<typeof setTimeout>;
  cancel?: (timer: ReturnType<typeof setTimeout>) => void;
}) {
  const schedule = options.schedule ?? setTimeout;
  const cancel = options.cancel ?? clearTimeout;
  let disposed = false;
  let pending: Promise<void> | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  function check(): Promise<void> {
    if (disposed) return Promise.resolve();
    if (pending) return pending;
    if (timer !== null) { cancel(timer); timer = null; }
    options.onChecking?.(true);
    pending = (async () => {
      let result: WebuiAccess;
      try { result = await options.probe(); } catch { result = 'unavailable'; }
      if (disposed) return;
      options.onResult(result);
      if (result !== 'allowed') timer = schedule(() => { timer = null; void check(); }, 5000);
    })().finally(() => {
      pending = null;
      if (!disposed) options.onChecking?.(false);
    });
    return pending;
  }
  return {
    check,
    dispose() {
      disposed = true;
      if (timer !== null) cancel(timer);
      timer = null;
    },
  };
}
