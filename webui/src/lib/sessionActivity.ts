import type { SessionActivity } from '../api';

/** Waiting is a more specific state than an accepted, still-active turn. */
export function sessionActivityBadge(activity: SessionActivity | undefined, running: boolean, active: boolean): SessionActivity | undefined {
  if (activity === 'waiting') return 'waiting';
  if (running || activity === 'running') return 'running';
  return !active && activity === 'open' ? 'open' : undefined;
}

/** At most one scan in flight. Teardown aborts it and rejects late results. */
export function pollSessionActivity<T>(options: {
  hashes: string[];
  delay: number;
  hidden: () => boolean;
  load: (hash: string, signal: AbortSignal) => Promise<T>;
  apply: (hash: string, value: T) => void;
  schedule: (task: () => void, delay: number) => () => void;
}): () => void {
  const controller = new AbortController();
  let cancelTimer: (() => void) | undefined;
  const tick = async () => {
    try {
      if (options.hidden()) return;
      for (const hash of options.hashes) {
        if (controller.signal.aborted) return;
        try {
          const value = await options.load(hash, controller.signal);
          if (controller.signal.aborted) return;
          options.apply(hash, value);
        } catch {
          // One failed project does not hide the other projects' activity.
        }
      }
    } finally {
      if (!controller.signal.aborted) cancelTimer = options.schedule(() => void tick(), options.delay);
    }
  };
  cancelTimer = options.schedule(() => void tick(), options.delay);
  return () => { controller.abort(); cancelTimer?.(); };
}
