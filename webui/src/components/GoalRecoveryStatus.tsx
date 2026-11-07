import { useEffect, useState } from 'preact/hooks';
import type { GoalRecoveryProgress } from '../api';
import { useT } from '../settings';
import { goalRecoveryDisplay } from '../lib/goalRecovery';

// Tick this status alone, rather than re-rendering the whole conversation every second.
export function GoalRecoveryStatus({ recovery, sampledAt, kind = 'goal' }: {
  kind?: 'goal' | 'loop';
  recovery: GoalRecoveryProgress;
  sampledAt: number;
}) {
  const t = useT();
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [sampledAt]);
  const display = goalRecoveryDisplay(recovery, sampledAt, now, kind);
  return (
    <div class="goal-recovery-status" role="status" aria-live={display.secs > 0 ? 'off' : 'polite'}>
      {t(display.key, { attempt: recovery.attempt, max: recovery.max_attempts, secs: display.secs })}
    </div>
  );
}
