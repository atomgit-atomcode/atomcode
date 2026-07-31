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
    execServerCommand: (cmd, arg) => { calls.push(`exec:${cmd}:${arg}`); },
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

import { buildSlashMenuItems } from './slashCommands.ts';

test('menu lists matching commands before skills', () => {
  const skills = [{ name: 'summarize', description: 'sum' }, { name: 'plan-trip', description: 'trip' }];
  const items = buildSlashMenuItems(FRONTEND_COMMANDS, skills, 'pl', (k) => k);
  // 'pl' 命中命令 plan 与 skill plan-trip；命令在前
  assert.equal(items[0].kind, 'command');
  assert.equal(items[0].name, 'plan');
  assert.ok(items.some((i) => i.kind === 'skill' && i.name === 'plan-trip'));
});

test('empty query returns all commands then all skills', () => {
  const skills = [{ name: 's1' }];
  const items = buildSlashMenuItems(FRONTEND_COMMANDS, skills, '', (k) => k);
  assert.equal(items.filter((i) => i.kind === 'command').length, FRONTEND_COMMANDS.length);
  assert.equal(items[items.length - 1].kind, 'skill');
});

test('skills are sorted alphabetically by name (commands keep FRONTEND_COMMANDS order)', () => {
  const skills = [{ name: 'zebra', description: 'z' }, { name: 'apple', description: 'a' }];
  const items = buildSlashMenuItems(FRONTEND_COMMANDS, skills, '', (k) => k);
  const skillItems = items.filter((i) => i.kind === 'skill');
  assert.equal(skillItems[0].name, 'apple');
  assert.equal(skillItems[1].name, 'zebra');
});

test('/undo /remember /forget /memory dispatch to execServerCommand', async () => {
  const calls: string[] = [];
  const h: SlashHandlers = {
    setMode: () => {}, openModelPicker: () => {}, setProvider: () => {},
    changeDir: () => {}, openSessionSidebar: () => {}, reloadConfig: () => {},
    openSlashSkillsMenu: () => {}, notice: (t) => { calls.push(`notice:${t}`); },
    execServerCommand: (cmd, arg) => { calls.push(`exec:${cmd}:${arg}`); },
    t: (k) => k,
  };
  const map = buildCommandMap(FRONTEND_COMMANDS);
  await dispatchSlashCommand('/undo 2', map, h);
  await dispatchSlashCommand('/memory', map, h);
  await dispatchSlashCommand('/remember a fact', map, h);
  await dispatchSlashCommand('/forget stale', map, h);
  assert.deepEqual(calls, ['exec:undo:2', 'exec:memory:', 'exec:remember:a fact', 'exec:forget:stale']);
});

test('/context and /compact dispatch to execServerCommand', async () => {
  const calls: string[] = [];
  const h: SlashHandlers = {
    setMode: () => {}, openModelPicker: () => {}, setProvider: () => {}, changeDir: () => {},
    openSessionSidebar: () => {}, reloadConfig: () => {}, openSlashSkillsMenu: () => {},
    notice: () => {}, execServerCommand: (cmd, arg) => { calls.push(`exec:${cmd}:${arg}`); }, t: (k) => k,
  };
  const map = buildCommandMap(FRONTEND_COMMANDS);
  await dispatchSlashCommand('/context', map, h);
  await dispatchSlashCommand('/compact focus on the bug', map, h);
  assert.deepEqual(calls, ['exec:context:', 'exec:compact:focus on the bug']);
});

test('display commands dispatch to execServerCommand', async () => {
  const calls: string[] = [];
  const h: SlashHandlers = {
    setMode: () => {}, openModelPicker: () => {}, setProvider: () => {}, changeDir: () => {},
    openSessionSidebar: () => {}, reloadConfig: () => {}, openSlashSkillsMenu: () => {},
    notice: () => {}, execServerCommand: (cmd, arg) => { calls.push(`${cmd}:${arg}`); }, t: (k) => k,
  };
  const map = buildCommandMap(FRONTEND_COMMANDS);
  for (const c of ['whoami','status','config','diff','cost','todo']) await dispatchSlashCommand(`/${c}`, map, h);
  assert.deepEqual(calls, ['whoami:','status:','config:','diff:','cost:','todo:']);
});

test('/remember and /forget without arg emit a notice', async () => {
  const calls: string[] = [];
  const h: SlashHandlers = {
    setMode: () => {}, openModelPicker: () => {}, setProvider: () => {},
    changeDir: () => {}, openSessionSidebar: () => {}, reloadConfig: () => {},
    openSlashSkillsMenu: () => {}, notice: (t) => { calls.push(t); },
    execServerCommand: (cmd, arg) => { calls.push(`exec:${cmd}:${arg}`); },
    t: (k) => k,
  };
  const map = buildCommandMap(FRONTEND_COMMANDS);
  await dispatchSlashCommand('/remember', map, h);
  await dispatchSlashCommand('/forget', map, h);
  assert.deepEqual(calls, ['cmd.remember.needArg', 'cmd.forget.needArg']);
});

test('/review dispatches an explicit code_review scope through chat', async () => {
  const prompts: string[] = [];
  const h: SlashHandlers = {
    setMode: () => {}, openModelPicker: () => {}, setProvider: () => {},
    changeDir: () => {}, openSessionSidebar: () => {}, reloadConfig: () => {},
    openSlashSkillsMenu: () => {}, notice: () => {},
    submitPrompt: (text) => { prompts.push(text); },
    execServerCommand: () => {}, t: (k) => k,
  };
  const map = buildCommandMap(FRONTEND_COMMANDS);
  await dispatchSlashCommand('/review staged', map, h);
  assert.equal(prompts.length, 1);
  assert.match(prompts[0], /"scope":\{"kind":"staged"\}/);
});

test('/review range JSON-escapes the ref without duplicating it into prose', async () => {
  const prompts: string[] = [];
  const h: SlashHandlers = {
    setMode: () => {}, openModelPicker: () => {}, setProvider: () => {},
    changeDir: () => {}, openSessionSidebar: () => {}, reloadConfig: () => {},
    openSlashSkillsMenu: () => {}, notice: () => {},
    submitPrompt: (text) => { prompts.push(text); },
    execServerCommand: () => {}, t: (k) => k,
  };
  await dispatchSlashCommand('/review odd"ref', buildCommandMap(FRONTEND_COMMANDS), h);
  assert.match(prompts[0], /"base":"odd\\"ref"/);
  assert.doesNotMatch(prompts[0], /odd"ref\.\.HEAD/);
});
