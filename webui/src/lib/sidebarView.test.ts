import { test } from 'node:test';
import assert from 'node:assert';
import {
  decrementProjectSessionCount,
  loadSidebarViewMode,
  saveSidebarViewMode,
  sidebarProjectScopes,
} from './sidebarView.ts';

test('sidebar view defaults to workspace and accepts only the flat preference', () => {
  assert.equal(loadSidebarViewMode(null), 'workspace');
  assert.equal(loadSidebarViewMode({ getItem: () => 'workspace', setItem() {} }), 'workspace');
  assert.equal(loadSidebarViewMode({ getItem: () => 'flat', setItem() {} }), 'flat');
  assert.equal(loadSidebarViewMode({ getItem: () => 'unexpected', setItem() {} }), 'workspace');
});

test('sidebar view preference is persisted without making storage mandatory', () => {
  let saved = '';
  saveSidebarViewMode('flat', {
    getItem: () => null,
    setItem: (_key, value) => { saved = value; },
  });
  assert.equal(saved, 'flat');

  assert.doesNotThrow(() => saveSidebarViewMode('workspace', {
    getItem: () => null,
    setItem: () => { throw new Error('storage disabled'); },
}));
});

test('workspace view loads requested expanded buckets while flat view uses the global feed', () => {
  assert.deepEqual(sidebarProjectScopes('workspace', ['project-1', 'project-2']), [
    'project-1',
    'project-2',
  ]);
  assert.deepEqual(sidebarProjectScopes('workspace', ['', 'project-2', 'project-2']), ['project-2']);
  assert.deepEqual(sidebarProjectScopes('flat', ['project-1']), []);
});

test('successful deletion decrements only the matching project summary', () => {
  const projects = [
    { hash: 'project-1', session_count: 2, name: 'one' },
    { hash: 'project-2', session_count: 4, name: 'two' },
  ];

  assert.deepEqual(decrementProjectSessionCount(projects, 'project-1'), [
    { hash: 'project-1', session_count: 1, name: 'one' },
    { hash: 'project-2', session_count: 4, name: 'two' },
  ]);
  assert.equal(projects[0].session_count, 2);
});

test('project session count never becomes negative', () => {
  const projects = [{ hash: 'project-1', session_count: 0 }];
  assert.deepEqual(decrementProjectSessionCount(projects, 'project-1'), [
    { hash: 'project-1', session_count: 0 },
  ]);
});
