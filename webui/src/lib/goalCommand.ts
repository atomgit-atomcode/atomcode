import type { LiveGoalProgress, LiveWireEvent } from '../api.ts';
import type { MsgKey } from '../i18n.ts';

type GoalWireEvent = Extract<LiveWireEvent, { type: 'snapshot' | 'goal_changed' }>;
type Translate = (key: MsgKey, params?: Record<string, string | number>) => string;

export function goalFromLiveEvent(event: GoalWireEvent): LiveGoalProgress | null {
  if (event.type === 'snapshot') return event.goal ?? null;
  if (event.phase === 'ended') return null;
  const { type: _type, ...progress } = event;
  return progress;
}

export function goalStatusNotice(
  goal: LiveGoalProgress | null,
  t: Translate,
  secondsSinceEvent = 0,
): string {
  if (!goal) return t('cmd.goal.none');

  const phaseKey: Record<string, MsgKey> = {
    pursuing: 'cmd.goal.phase.pursuing',
    paused: 'cmd.goal.phase.paused',
    paused_at_cap: 'cmd.goal.phase.pausedAtCap',
    satisfied: 'cmd.goal.phase.satisfied',
    ended: 'cmd.goal.phase.ended',
  };
  const terminalKey: Record<string, MsgKey> = {
    met: 'cmd.goal.terminal.met',
    stopped: 'cmd.goal.terminal.stopped',
    failed: 'cmd.goal.terminal.failed',
    cancelled: 'cmd.goal.terminal.cancelled',
  };
  const elapsed = Math.max(0, Math.floor(goal.elapsed_secs))
    + (goal.active ? Math.max(0, Math.floor(secondsSinceEvent)) : 0);
  const minutes = Math.floor(elapsed / 60);
  const seconds = String(elapsed % 60).padStart(2, '0');
  const phase = phaseKey[goal.phase] ? t(phaseKey[goal.phase]) : goal.phase;
  const lines = [
    t('cmd.goal.status', { condition: goal.condition }),
    t('cmd.goal.statusPhase', { phase }),
    t('cmd.goal.statusRound', { round: Math.max(0, goal.round) + 1 }),
    t('cmd.goal.statusElapsed', { minutes, seconds }),
    t('cmd.goal.statusActive', { active: t(goal.active ? 'cmd.goal.yes' : 'cmd.goal.no') }),
  ];
  if (goal.terminal) {
    const terminal = terminalKey[goal.terminal]
      ? t(terminalKey[goal.terminal])
      : goal.terminal;
    lines.push(t('cmd.goal.statusTerminal', { terminal }));
  }
  if (goal.last_reason?.trim()) {
    lines.push(t('cmd.goal.statusReason', { reason: goal.last_reason }));
  }
  return lines.join('\n');
}

interface GoalStartOptions {
  sync: boolean;
  busy: boolean;
  sessionId: string | null;
  submit: (condition: string, sessionId: string | null) => Promise<{ accepted: boolean; error?: string }>;
  restore: (command: string) => void;
  notice: (message: string) => void;
  t: Translate;
}

export async function submitGoalStart(condition: string, options: GoalStartOptions): Promise<void> {
  const command = `/goal ${condition}`;
  if (!options.sync) {
    options.restore(command);
    options.notice(options.t('cmd.goal.syncRequired'));
    return;
  }
  if (options.busy) {
    options.restore(command);
    options.notice(options.t('cmd.session.busy'));
    return;
  }
  try {
    const result = await options.submit(condition, options.sessionId);
    if (!result.accepted) options.restore(command);
    options.notice(result.accepted
      ? options.t('cmd.goal.startRequested')
      : (result.error ?? options.t('cmd.goal.rejected')));
  } catch (error) {
    options.restore(command);
    throw error;
  }
}

export function restoreGoalInput(current: string, command: string): string {
  return current || command;
}
