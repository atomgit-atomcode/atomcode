import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { LiveGoalProgress } from '../api.ts';
import { messages, type MsgKey } from '../i18n.ts';
import { goalFromLiveEvent, goalStatusNotice, restoreGoalInput, submitGoalStart } from './goalCommand.ts';

function goal(overrides: Partial<LiveGoalProgress> = {}): LiveGoalProgress {
  return {
    active: true,
    round: 0,
    elapsed_secs: 0,
    condition: 'fix CI',
    terminal: null,
    phase: 'pursuing',
    last_reason: null,
    ...overrides,
  };
}

function t(key: string, params: Record<string, string | number> = {}): string {
  const template = messages.en[key as MsgKey] ?? key;
  return template.replace(/\{([^}]+)\}/g, (_match, name: string) => String(params[name] ?? ''));
}

test('goal status shows round, elapsed time, terminal, and cap reason', () => {
  const notice = goalStatusNotice(goal({
    active: false,
    round: 11,
    elapsed_secs: 271,
    phase: 'paused_at_cap',
    terminal: 'stopped',
    last_reason: 'Reached the round limit',
  }), t);
  assert.equal(notice, [
    'Current goal: fix CI',
    'Status: round limit reached',
    'Round: 12',
    'Elapsed: 4m 31s',
    'Active: no',
    'Result: stopped',
    'Reason: Reached the round limit',
  ].join('\n'));
});

test('goal status covers empty, running, paused, completed, and ended states', () => {
  assert.equal(goalStatusNotice(null, t), 'No current goal');
  for (const [phase, label] of [
    ['pursuing', 'running'],
    ['paused', 'paused'],
    ['satisfied', 'completed'],
    ['ended', 'ended'],
  ]) {
    assert.match(goalStatusNotice(goal({ phase }), t), new RegExp(`Status: ${label}`));
  }
  assert.match(goalStatusNotice(goal(), t), /Active: yes/);
  assert.doesNotMatch(goalStatusNotice(goal(), t), /Reason:|Result:/);
  assert.match(goalStatusNotice(goal({ elapsed_secs: 45 }), t, 30), /Elapsed: 1m 15s/);
  assert.match(goalStatusNotice(goal({ active: false, phase: 'paused', elapsed_secs: 45 }), t, 30), /Elapsed: 0m 45s/);
});

test('live snapshots replace goal state and ended events clear it', () => {
  const current = goal({ phase: 'paused', last_reason: 'Waiting for input' });
  const snapshot = {
    type: 'snapshot' as const,
    messages: [],
    session_id: 'session-1',
    project_hash: 'project-1',
    provider: 'test',
    mode: 'build' as const,
    goal: current,
  };
  assert.deepEqual(goalFromLiveEvent(snapshot), current);
  assert.equal(goalFromLiveEvent({ ...snapshot, goal: null }), null);
  assert.deepEqual(goalFromLiveEvent({ type: 'goal_changed', ...goal() }), goal());
  assert.equal(goalFromLiveEvent({ type: 'goal_changed', ...goal({ phase: 'ended' }) }), null);
});

test('goal start keeps the command when sync is off or a turn is busy', async () => {
  for (const [sync, busy, expectedNotice] of [
    [false, false, 'cmd.goal.syncRequired'],
    [true, true, 'cmd.session.busy'],
  ] as const) {
    const restored: string[] = [];
    const notices: string[] = [];
    await submitGoalStart('fix CI', {
      sync,
      busy,
      sessionId: 'session-1',
      submit: async () => { throw new Error('request must not be sent'); },
      restore: (command) => restored.push(command),
      notice: (message) => notices.push(message),
      t: (key) => key,
    });
    assert.deepEqual(restored, ['/goal fix CI']);
    assert.deepEqual(notices, [expectedNotice]);
  }
});

test('goal start keeps rejected input and reports the server reason', async () => {
  const restored: string[] = [];
  const notices: string[] = [];
  let submittedSession: string | null = null;
  await submitGoalStart('fix CI', {
    sync: true,
    busy: false,
    sessionId: 'session-1',
    submit: async (_condition, sessionId) => {
      submittedSession = sessionId;
      return { accepted: false, error: 'runtime unavailable' };
    },
    restore: (command) => restored.push(command),
    notice: (message) => notices.push(message),
    t: (key) => key,
  });
  assert.equal(submittedSession, 'session-1');
  assert.deepEqual(restored, ['/goal fix CI']);
  assert.deepEqual(notices, ['runtime unavailable']);
});

test('goal start keeps input after a network error and leaves accepted input cleared', async () => {
  const restored: string[] = [];
  const notices: string[] = [];
  const options = {
    sync: true,
    busy: false,
    sessionId: null,
    restore: (command: string) => restored.push(command),
    notice: (message: string) => notices.push(message),
    t: (key: string) => key,
  };
  await assert.rejects(
    submitGoalStart('fix CI', {
      ...options,
      submit: async () => { throw new Error('offline'); },
    }),
    /offline/,
  );
  assert.deepEqual(restored, ['/goal fix CI']);
  restored.length = 0;
  await submitGoalStart('fix CI', { ...options, submit: async () => ({ accepted: true }) });
  assert.deepEqual(restored, []);
  assert.deepEqual(notices, ['cmd.goal.startRequested']);
  assert.equal(restoreGoalInput('', '/goal fix CI'), '/goal fix CI');
  assert.equal(restoreGoalInput('new text', '/goal fix CI'), 'new text');
});
