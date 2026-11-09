import { test } from 'node:test';
import assert from 'node:assert/strict';
import { clearWorkspaceSessions, visibleWorkspaces, applyWorkspacePreference, WorkspaceClearError } from './workspaceActions.ts';
import type { SessionMetaWithProject } from '../api.ts';

const sessions = ['a', 'b', 'c'].map(id => ({ id, project_hash: 'project' }) as SessionMetaWithProject);

test('pins precede selected workspace; hidden current workspace stays hidden', () => {
  const projects = [{ hash: 'older' }, { hash: 'selected' }, { hash: 'pinned', pinned: true }, { hash: 'hidden', hidden: true }];
  assert.deepEqual(visibleWorkspaces(projects, 'selected').map(p => p.hash), ['pinned', 'selected', 'older']);
  assert.deepEqual(visibleWorkspaces(projects, 'hidden').map(p => p.hash), ['pinned', 'older', 'selected']);
  assert.equal(projects.length, 4);
});

test('clearing obtains the full list and reports only committed deletions', async () => {
  const deleted: string[] = [];
  const callbacks: string[] = [];
  const count = await clearWorkspaceSessions('project', null, s => callbacks.push(s.id), {
    list: async hash => { assert.equal(hash, 'project'); return sessions; },
    remove: async (hash, id) => { assert.equal(hash, 'project'); deleted.push(id); },
  });
  assert.equal(count, 3);
  assert.deepEqual(deleted, ['a', 'b', 'c']);
  assert.deepEqual(callbacks, deleted);
});

test('a running session refuses the batch before deleting anything', async () => {
  let writes = 0;
  await assert.rejects(clearWorkspaceSessions('project', 'b', () => { writes++; }, {
    list: async () => sessions, remove: async () => { writes++; },
  }), error => error instanceof WorkspaceClearError && error.busy && error.deleted === 0);
  assert.equal(writes, 0);
});

test('a lease failure stops immediately, keeps failed/remaining sessions, and can be retried', async () => {
  const remaining = new Set(['a', 'b', 'c']);
  const notified: string[] = [];
  let fail = true;
  const api = {
    list: async () => sessions.filter(s => remaining.has(s.id)),
    remove: async (_hash: string, id: string) => {
      if (fail && id === 'b') throw new Error('SESSION_IN_USE');
      remaining.delete(id);
    },
  };
  await assert.rejects(clearWorkspaceSessions('project', null, s => notified.push(s.id), api),
    error => error instanceof WorkspaceClearError && error.deleted === 1 && error.message === 'SESSION_IN_USE');
  assert.deepEqual([...remaining], ['b', 'c']);
  assert.deepEqual(notified, ['a']);
  fail = false;
  assert.equal(await clearWorkspaceSessions('project', null, s => notified.push(s.id), api), 2);
  assert.equal(remaining.size, 0);
});

test('a failed snapshot does not start any deletion', async () => {
  let writes = 0;
  await assert.rejects(clearWorkspaceSessions('project', null, () => {}, {
    list: async () => { throw new Error('offline'); }, remove: async () => { writes++; },
  }), /offline/);
  assert.equal(writes, 0);
});


test('confirmed preferences survive a failed refresh, including an unsaved current group', () => {
  const initial = [{ hash: 'other' }];
  const hidden = applyWorkspacePreference(initial, 'current', { hidden: true }, { hash: 'current' });
  assert.deepEqual(visibleWorkspaces(hidden, 'current').map(p => p.hash), ['other']);
  const pinned = applyWorkspacePreference(hidden, 'other', { pinned: true });
  assert.equal(pinned.find(p => p.hash === 'other')?.pinned, true);
  assert.deepEqual(initial, [{ hash: 'other' }]);
});
