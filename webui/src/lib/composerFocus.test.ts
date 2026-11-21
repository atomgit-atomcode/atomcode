import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  createComposerToolbarHoldCore,
  createComposerToolbarHoldDom,
  type ComposerHoldTarget,
} from './composerFocus.ts';

// A stand-in for the composer element. The core only knows how to mark and
// unmark it, so these tests never need a DOM.
function fakeTarget() {
  let marked = false;
  const target: ComposerHoldTarget = {
    mark: () => { marked = true; },
    unmark: () => { marked = false; },
  };
  return { target, isMarked: () => marked };
}

// The click that follows a pointerup is dispatched in the same task, so the
// mark has to survive it. A fake clock lets the test drive that boundary.
function fakeClock() {
  const tasks: (() => void)[] = [];
  return {
    defer: (task: () => void) => {
      tasks.push(task);
      return () => {
        const index = tasks.indexOf(task);
        if (index >= 0) tasks.splice(index, 1);
      };
    },
    pending: () => tasks.length,
    flush: () => {
      const due = tasks.splice(0);
      for (const task of due) task();
    },
  };
}

// Releases that land outside the pressed control: a press that drags off the
// button and lets go never delivers pointerup to the button itself.
function fakeWatch() {
  const handlers = new Set<(kind: 'up' | 'cancel') => void>();
  return {
    watch: (onPointer: (kind: 'up' | 'cancel') => void) => {
      handlers.add(onPointer);
      return () => { handlers.delete(onPointer); };
    },
    fire: (kind: 'up' | 'cancel') => {
      assert.ok(handlers.size > 0, 'the core must watch for releases while it holds');
      for (const handler of [...handlers]) handler(kind);
    },
    watching: () => handlers.size > 0,
    count: () => handlers.size,
  };
}

test('a press marks the composer', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);

  assert.equal(composer.isMarked(), true);
});

test('the mark outlives the click and is released on the next task', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);
  core.lift();

  assert.equal(composer.isMarked(), true, 'the click still has to reach the button');
  assert.equal(clock.pending(), 1);

  clock.flush();

  assert.equal(composer.isMarked(), false);
});

test('a cancelled press releases the mark immediately', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);
  core.cancel();

  assert.equal(composer.isMarked(), false);
  assert.equal(clock.pending(), 0, 'a cancelled press has no click left to protect');
});

test('a release outside the pressed control still releases the mark', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);
  watcher.fire('up');
  clock.flush();

  assert.equal(composer.isMarked(), false, 'dragging off the button must not strand the mark');
});

test('a cancellation outside the pressed control releases the mark immediately', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);
  watcher.fire('cancel');

  assert.equal(composer.isMarked(), false);
  assert.equal(clock.pending(), 0);
});

test('the composer is released once the owner goes away', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);
  core.dispose();

  assert.equal(composer.isMarked(), false);
  assert.equal(watcher.watching(), false, 'a released composer stops watching for releases');
});

test('a press without a composer marks nothing and does not throw', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });

  core.press(null);
  core.lift();
  core.cancel();
  core.dispose();

  assert.equal(watcher.watching(), false);
  assert.equal(clock.pending(), 0);
});

test('a second press while the first is still held keeps the composer marked', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);
  core.lift();
  core.press(composer.target);

  assert.equal(composer.isMarked(), true, 'the newer press outlives the pending release');
  assert.equal(clock.pending(), 0, 'the stale release must not fire under the new press');

  clock.flush();

  assert.equal(composer.isMarked(), true);
});

test('a second press does not leave the first release subscription behind', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const first = fakeTarget();
  const second = fakeTarget();

  core.press(first.target);
  core.lift();
  core.press(second.target);

  assert.equal(watcher.count(), 1, 'the composer is watched by exactly one subscription');
});

test('a second release does not leave an orphaned pending task behind', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const core = createComposerToolbarHoldCore({ defer: clock.defer, watch: watcher.watch });
  const composer = fakeTarget();

  core.press(composer.target);
  core.lift();
  core.lift();

  assert.equal(clock.pending(), 1, 'only the latest release is still scheduled');
  assert.equal(composer.isMarked(), true);

  clock.flush();

  assert.equal(composer.isMarked(), false);
});

// ── The DOM wiring ────────────────────────────────────────────────────────
// A stand-in for a control element: the composer is found by walking up to
// the nearest .input-box, and the mark is a class on that box.
function fakeControl(marked: { value: boolean }) {
  const classes = new Set<string>();
  const composer = {
    classList: {
      add: (name: string) => classes.add(name),
      remove: (name: string) => classes.delete(name),
      contains: (name: string) => classes.has(name),
    },
  };
  const control = {
    closest: (selector: string) => (selector === '.input-box' ? composer : null),
  };
  return { control, composer, isMarked: () => marked.value && classes.has('toolbar-held') };
}

test('the DOM wiring marks the composer that owns the pressed control', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const marked = { value: true };
  const { control, isMarked } = fakeControl(marked);
  const hold = createComposerToolbarHoldDom({
    defer: clock.defer,
    composerOf: element => element.closest('.input-box'),
    watch: watcher.watch,
  });

  hold.onPointerDown({ currentTarget: control as unknown as HTMLElement });

  assert.equal(isMarked(), true);

  hold.onPointerUp();
  clock.flush();

  assert.equal(isMarked(), false);
});

test('the DOM wiring survives a cancel after a release was scheduled', () => {
  // The browser's timer API hands back a timer id, not a cancel function. If
  // the core mistook the id for the cancel function, this would throw.
  const timers: (() => void)[] = [];
  const marked = { value: true };
  const { control } = fakeControl(marked);
  const hold = createComposerToolbarHoldDom({
    defer: task => {
      timers.push(task);
      return () => {
        const index = timers.indexOf(task);
        if (index >= 0) timers.splice(index, 1);
      };
    },
    composerOf: element => element.closest('.input-box'),
    watch: () => () => {},
  });

  hold.onPointerDown({ currentTarget: control as unknown as HTMLElement });
  hold.onPointerUp();
  hold.onPointerCancel();

  assert.equal(timers.length, 0, 'the scheduled release is dropped rather than left to run');
});

test('the DOM wiring ignores a press whose control has no composer', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const hold = createComposerToolbarHoldDom({
    defer: clock.defer,
    composerOf: () => null,
    watch: watcher.watch,
  });

  hold.onPointerDown({ currentTarget: {} as HTMLElement });
  hold.onPointerUp();
  hold.onPointerCancel();
  hold.dispose();

  assert.equal(watcher.watching(), false);
  assert.equal(clock.pending(), 0);
});

test('the DOM wiring releases the composer when it goes away', () => {
  const clock = fakeClock();
  const watcher = fakeWatch();
  const marked = { value: true };
  const { control, isMarked } = fakeControl(marked);
  const hold = createComposerToolbarHoldDom({
    defer: clock.defer,
    composerOf: element => element.closest('.input-box'),
    watch: watcher.watch,
  });

  hold.onPointerDown({ currentTarget: control as unknown as HTMLElement });
  hold.dispose();

  assert.equal(isMarked(), false);
  assert.equal(watcher.watching(), false);
});
