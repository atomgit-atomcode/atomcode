import type { GoalRecoveryProgress } from '../api';

export function goalRecoveryDisplay(recovery: GoalRecoveryProgress, sampledAt: number, now: number, kind: 'goal' | 'loop' = 'goal') {
  const elapsed = Math.floor(Math.max(0, now - sampledAt) / 1000);
  const secs = Math.max(0, recovery.retry_in_secs - elapsed);
  const key = recovery.waiting_for_quota && kind === 'loop'
    ? secs > 0 ? 'loop.recovery.quota' : 'loop.recovery.quotaChecking'
    : recovery.waiting_for_quota
    ? secs > 0 ? 'goal.recovery.quota' : 'goal.recovery.quotaChecking'
    : secs === 0 ? 'goal.recovery.running'
    : recovery.kind === 'evaluator' ? 'goal.recovery.evaluator' : 'goal.recovery.provider';
  return { key, secs } as const;
}
