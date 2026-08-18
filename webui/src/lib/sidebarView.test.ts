import { test } from 'node:test';
import assert from 'node:assert';
import { loadSidebarViewMode, saveSidebarViewMode, sidebarProjectScope } from './sidebarView.ts';

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

test('workspace view scopes to one project while flat view loads across projects', () => {
  assert.equal(sidebarProjectScope('workspace', 'project-1'), 'project-1');
  assert.equal(sidebarProjectScope('workspace', ''), null);
  assert.equal(sidebarProjectScope('flat', 'project-1'), null);
});
