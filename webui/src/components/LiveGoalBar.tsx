import { useEffect, useRef, useState } from 'preact/hooks';
import { postLiveGoalAction, type LiveGoalProgress } from '../api';
import { useT } from '../settings';

function duration(seconds: number) {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor(seconds / 60) % 60;
  const remainder = `${minutes}m ${String(seconds % 60).padStart(2, '0')}s`;
  return hours ? `${hours}h ${remainder}` : remainder;
}

export function LiveGoalBar({ goal, sampledAt, sessionId, controlId }: {
  goal: LiveGoalProgress;
  controlId: string;
  sampledAt: number;
  sessionId: string;
}) {
  const t = useT();
  const [now, setNow] = useState(Date.now());
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inFlight = useRef(false);
  useEffect(() => {
    if (!goal.active) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [goal.active]);

  async function act(action: 'pause' | 'remove') {
    if (inFlight.current) return;
    inFlight.current = true;
    setPending(true);
    setError(null);
    try {
      await postLiveGoalAction(sessionId, controlId, action);
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    } finally {
      inFlight.current = false;
      setPending(false);
    }
  }

  const status = t(goal.active ? 'goal.bar.running' : 'goal.bar.paused');
  const elapsed = goal.elapsed_secs === undefined ? null : goal.elapsed_secs
    + (goal.active ? Math.max(0, Math.floor((now - sampledAt) / 1000)) : 0);
  return (
    <section class="live-goal-bar" aria-label={status}>
      <div class="live-goal-row">
        <span class="live-goal-state" role="status" data-active={goal.active}
          title={goal.phase === 'paused' ? t('goal.bar.resumeHint') : undefined}>{status}</span>
        <span class="live-goal-condition" title={goal.condition}>{goal.condition}</span>
        {elapsed !== null && <span class="live-goal-elapsed">{duration(elapsed)}</span>}
        {/* Keep a focused composer from collapsing under a mouse click on narrow screens. */}
        <div class="live-goal-actions" aria-busy={pending} onMouseDown={event => event.preventDefault()}>
          <button type="button" disabled={pending || !goal.active} onClick={() => void act('pause')}>
            {t('goal.bar.pause')}
          </button>
          <button class="live-goal-remove" type="button" disabled={pending} onClick={() => void act('remove')}>
            {t('goal.bar.remove')}
          </button>
        </div>
      </div>
      {error && <div class="live-goal-error" role="alert">{t('goal.bar.actionFailed', { message: error })}</div>}
    </section>
  );
}
