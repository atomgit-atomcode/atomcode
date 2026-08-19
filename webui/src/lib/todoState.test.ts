import { test } from 'node:test';
import assert from 'node:assert';
import { applyTodoCall, projectTodoCalls } from './todoState.ts';

test('full TodoWrite plans replace the current list and decode repaired string arrays', () => {
  const next = applyTodoCall(
    [{ content: 'old', status: 'completed' }],
    'todowrite',
    JSON.stringify({ todos: JSON.stringify([
      { content: 'inspect state', status: 'in_progress' },
      { content: 'write tests', status: 'pending' },
    ]) }),
  );

  assert.deepEqual(next, [
    { content: 'inspect state', status: 'in_progress' },
    { content: 'write tests', status: 'pending' },
  ]);
});

test('incremental updates preserve one in-progress task and append pending work', () => {
  const initial = [
    { content: 'inspect state', status: 'in_progress' as const },
    { content: 'write tests', status: 'pending' as const },
  ];
  const moved = applyTodoCall(initial, 'todowrite', '{"action":"update","id":2,"status":"in_progress"}');
  const appended = applyTodoCall(moved, 'todowrite', '{"action":"add","content":" verify build "}');

  assert.deepEqual(appended, [
    { content: 'inspect state', status: 'pending' },
    { content: 'write tests', status: 'in_progress' },
    { content: 'verify build', status: 'pending' },
  ]);
});

test('malformed TodoWrite calls leave committed state unchanged', () => {
  const initial = [{ content: 'keep me', status: 'in_progress' as const }];

  assert.deepEqual(applyTodoCall(initial, 'todowrite', '{"todos":[{"content":"","status":"pending"}]}'), initial);
  assert.deepEqual(applyTodoCall(initial, 'todowrite', '{"action":"update","id":9,"status":"completed"}'), initial);
  assert.deepEqual(applyTodoCall(initial, 'bash', '{"todos":[]}'), initial);
});

test('an empty full plan clears the task panel', () => {
  const initial = [{ content: 'finished', status: 'completed' as const }];
  assert.deepEqual(applyTodoCall(initial, 'todowrite', '{"todos":[]}'), []);
});

test('parallel TodoWrite results are projected in call order, not completion order', () => {
  const calls = [
    {
      id: 'plan',
      name: 'todowrite',
      args: '{"todos":[{"content":"inspect","status":"in_progress"},{"content":"fix","status":"pending"}]}',
      success: undefined,
    },
    {
      id: 'advance',
      name: 'todowrite',
      args: '{"action":"update","id":2,"status":"in_progress"}',
      success: true,
    },
  ];

  const whilePlanPending = projectTodoCalls([], calls);
  assert.deepEqual(whilePlanPending.committed, []);
  assert.deepEqual(whilePlanPending.preview, [
    { content: 'inspect', status: 'pending' },
    { content: 'fix', status: 'in_progress' },
  ]);

  calls[0].success = true;
  const settled = projectTodoCalls([], calls);
  assert.deepEqual(settled.committed, whilePlanPending.preview);
  assert.equal(settled.hasUnresolved, false);
});

test('failed TodoWrite calls are skipped while later successful calls remain ordered', () => {
  const projected = projectTodoCalls(
    [{ content: 'existing', status: 'in_progress' }],
    [
      { id: 'failed', name: 'todowrite', args: '{"todos":[]}', success: false },
      { id: 'add', name: 'todowrite', args: '{"action":"add","content":"verify"}', success: true },
    ],
  );

  assert.deepEqual(projected.committed, [
    { content: 'existing', status: 'in_progress' },
    { content: 'verify', status: 'pending' },
  ]);
  assert.equal(projected.hasApplicable, true);
});

test('failed and malformed calls do not make a hidden prior plan applicable', () => {
  const base = [{ content: 'old plan', status: 'completed' as const }];

  assert.equal(projectTodoCalls(base, [
    { id: 'failed', name: 'todowrite', args: '{"todos":[]}', success: false },
  ]).hasApplicable, false);
  assert.equal(projectTodoCalls(base, [
    { id: 'malformed', name: 'todowrite', args: '{"todos":[{"content":"","status":"pending"}]}' },
  ]).hasApplicable, false);
});
