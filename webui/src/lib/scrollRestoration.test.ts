import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createScrollRestoration } from './scrollRestoration.ts';

function fixture(follow = true) {
  const viewport = { scrollTop: follow ? 1500 : 420, scrollHeight: 2000, clientHeight: 500 };
  let session = 'session-a';
  let following = follow;
  let next = 0;
  const frames = new Map<number, () => void>();
  const scroll = createScrollRestoration({
    viewport: () => viewport, session: () => session, following: () => following,
    setFollowing: v => { following = v; },
    schedule: run => { frames.set(++next, run); return next; }, cancel: id => { frames.delete(id); },
  });
  return { viewport, scroll, frames,
    follows: () => following, setFollowing: (v: boolean) => { following = v; },
    switchSession: () => { session = 'session-b'; },
    flush: () => { for (let i = 0; frames.size && i < 10; i++) { const pending = [...frames.values()]; frames.clear(); pending.forEach(run => run()); } },
  };
}

test('resuming a backgrounded task follows new output instead of accepting the browser top reset', () => {
  const f = fixture();
  f.scroll.suspend();
  f.viewport.scrollTop = 0;
  f.viewport.scrollHeight = 3500;
  assert.equal(f.scroll.onScroll(), null);
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 0, 'do not scroll a hidden page');
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 3000);
  assert.equal(f.follows(), true);
  assert.equal(f.scroll.onScroll(), true);
});

test('a reader in history keeps their place across backgrounding and later content changes', () => {
  const f = fixture(false);
  f.scroll.suspend();
  f.viewport.scrollTop = 0;
  f.viewport.scrollHeight = 5000;
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 420);
  assert.equal(f.follows(), false);
  f.viewport.scrollHeight += 200;
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 420);
});

test('zero-height viewport during browser restoration does not discard saved scroll intent', () => {
  const f = fixture();
  f.scroll.suspend();
  f.viewport.clientHeight = 0;
  f.viewport.scrollTop = 0;
  f.scroll.resume();
  f.flush();
  assert.equal(f.scroll.onScroll(), null);
  f.viewport.clientHeight = 600;
  f.viewport.scrollHeight = 4000;
  f.scroll.layout();
  f.flush();
  assert.equal(f.viewport.scrollTop, 3400);
});

test('manual scrolling wins over queued resume frames', () => {
  const f = fixture();
  f.scroll.suspend();
  f.scroll.resume();
  f.scroll.interact();
  f.viewport.scrollTop = 300;
  f.setFollowing(f.scroll.onScroll()!);
  f.flush();
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 300);
  assert.equal(f.follows(), false);
});

test('old session positions and disposed callbacks cannot move a replacement view', () => {
  const f = fixture(false);
  f.scroll.suspend();
  f.switchSession();
  f.viewport.scrollTop = 200;
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 200);
  f.scroll.resume();
  f.scroll.dispose();
  f.viewport.scrollTop = 100;
  f.flush();
  assert.equal(f.viewport.scrollTop, 100);
});


test('a session switched while hidden uses its own follow intent rather than the old position', () => {
  const f = fixture(false);
  f.scroll.suspend();
  f.switchSession();
  f.setFollowing(true);
  f.viewport.scrollTop = 0;
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 1500);
});
