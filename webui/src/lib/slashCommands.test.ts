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
