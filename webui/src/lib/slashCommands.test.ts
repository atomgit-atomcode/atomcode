import { test } from 'node:test';
import assert from 'node:assert';
import { parseSlashCommand } from './slashCommands.ts';

test('parses a bare command', () => {
  assert.deepEqual(parseSlashCommand('/plan'), { name: 'plan', arg: '' });
});

test('parses a command with an argument', () => {
  assert.deepEqual(parseSlashCommand('/cd ~/work/atomcode'), {
    name: 'cd',
    arg: '~/work/atomcode',
  });
});

test('trims leading whitespace and collapses arg whitespace', () => {
  assert.deepEqual(parseSlashCommand('  /model   glm-5.2  '), {
    name: 'model',
    arg: 'glm-5.2',
  });
});

test('returns null for non-command text', () => {
  assert.equal(parseSlashCommand('hello world'), null);
});

test('rejects a unix path (not a command)', () => {
  assert.equal(parseSlashCommand('/Users/me/file.txt'), null);
});

test('rejects a bare slash', () => {
  assert.equal(parseSlashCommand('/'), null);
});

test('allows colon and dash in names (mcp:foo, back-up)', () => {
  assert.deepEqual(parseSlashCommand('/mcp:status'), { name: 'mcp:status', arg: '' });
});

import {
  buildCommandMap,
  dispatchSlashCommand,
  FRONTEND_COMMANDS,
  buildHelpText,
  type SlashHandlers,
} from './slashCommands.ts';

function fakeHandlers(): { h: SlashHandlers; calls: string[] } {
  const calls: string[] = [];
  const h: SlashHandlers = {
    setMode: (m) => { calls.push(`setMode:${m}`); },
    openModelPicker: () => { calls.push('openModelPicker'); },
    setProvider: (n) => { calls.push(`setProvider:${n}`); },
    changeDir: (p) => { calls.push(`changeDir:${p}`); },
    openSessionSidebar: () => { calls.push('openSessionSidebar'); },
    reloadConfig: () => { calls.push('reloadConfig'); },
    openSlashSkillsMenu: () => { calls.push('openSlashSkillsMenu'); },
    notice: (t) => { calls.push(`notice:${t}`); },
    t: (k) => k,
  };
  return { h, calls };
}

test('/plan switches mode via handler', async () => {
  const { h, calls } = fakeHandlers();
  const map = buildCommandMap(FRONTEND_COMMANDS);
  const r = await dispatchSlashCommand('/plan', map, h);
  assert.deepEqual(r, { handled: true });
  assert.deepEqual(calls, ['setMode:plan']);
});

test('/model with arg sets provider; bare /model opens picker', async () => {
  const map = buildCommandMap(FRONTEND_COMMANDS);
  const a = fakeHandlers();
  await dispatchSlashCommand('/model glm-5.2', map, a.h);
  assert.deepEqual(a.calls, ['setProvider:glm-5.2']);
  const b = fakeHandlers();
  await dispatchSlashCommand('/model', map, b.h);
  assert.deepEqual(b.calls, ['openModelPicker']);
});

test('/cd without arg emits a notice, does not change dir', async () => {
  const { h, calls } = fakeHandlers();
  const map = buildCommandMap(FRONTEND_COMMANDS);
  await dispatchSlashCommand('/cd', map, h);
  assert.equal(calls.length, 1);
  assert.match(calls[0], /^notice:/);
});

test('unknown command is not handled (falls through to chat)', async () => {
  const { h } = fakeHandlers();
  const map = buildCommandMap(FRONTEND_COMMANDS);
  const r = await dispatchSlashCommand('/definitely-not-a-command', map, h);
  assert.deepEqual(r, { handled: false, unknown: true });
});

test('non-command text is not handled', async () => {
  const { h } = fakeHandlers();
  const map = buildCommandMap(FRONTEND_COMMANDS);
  const r = await dispatchSlashCommand('just chatting', map, h);
  assert.deepEqual(r, { handled: false });
});

test('/help notice lists command names', async () => {
  const { h, calls } = fakeHandlers();
  const map = buildCommandMap(FRONTEND_COMMANDS);
  await dispatchSlashCommand('/help', map, h);
  assert.match(calls[0], /\/plan/);
  assert.match(calls[0], /\/model/);
});
